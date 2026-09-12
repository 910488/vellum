//! Bundled lockfile vs signed downloaded core: two trust sources.
//! `active` / `pending` / `previous` persist separately. Download never
//! mutates the running launch identity.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::manifest::{current_arch, current_platform, verify_signed_manifest, CoreTarget};
use super::trust::TrustStore;
use super::write_atomic;

const SIGNED_MANIFEST_FILE: &str = "signed-manifest.json";
const SIGNED_SIGNATURE_FILE: &str = "signed-manifest.json.sig";
const SLOT_COMPLETE_MARKER: &str = ".complete";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RuntimeSource {
    BundledLockfile,
    SignedDownload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedCore {
    pub source: RuntimeSource,
    pub version: String,
    pub digest: String,
    pub protocol: String,
    pub path: PathBuf,
    pub helpers: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CoreSlot {
    pub version: String,
    pub digest: String,
    pub protocol: String,
    #[serde(default)]
    pub protocol_compat: String,
    pub path: PathBuf,
    pub helpers: Vec<PathBuf>,
    #[serde(default)]
    pub files: Vec<CoreSlotFile>,
    #[serde(default)]
    pub enhanced_commit: String,
    #[serde(default)]
    pub feature_profile: String,
    pub launch_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CoreSlotFile {
    pub path: PathBuf,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CoreSlots {
    pub active: Option<CoreSlot>,
    pub pending: Option<CoreSlot>,
    pub previous: Option<CoreSlot>,
}

impl CoreSlots {
    pub fn path(root: &Path) -> PathBuf {
        root.join("updates").join("core-slots.json")
    }

    pub fn load(root: &Path) -> Self {
        let Ok(bytes) = std::fs::read(Self::path(root)) else {
            return Self::default();
        };
        serde_json::from_slice(&bytes).unwrap_or_default()
    }

    pub fn save(&self, root: &Path) -> std::io::Result<()> {
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        write_atomic(&Self::path(root), &bytes)
    }
}

pub fn persist_slots(root: &Path, slots: &CoreSlots) -> std::io::Result<()> {
    slots.save(root)
}

pub fn slot_root(root: &Path, version: &str) -> PathBuf {
    root.join("updates").join("core").join(version)
}

pub fn slot_tree_root(root: &Path, version: &str) -> PathBuf {
    slot_root(root, version).join("current")
}

/// Persist the original signed bytes next to the slot tree. Identity is
/// re-verified from these files, never from mutable `core-slots.json`.
pub fn persist_signed_core_trust(
    root: &Path,
    version: &str,
    raw_manifest: &[u8],
    signature: &[u8],
) -> std::io::Result<()> {
    let dir = slot_root(root, version);
    std::fs::create_dir_all(&dir)?;
    write_atomic(&dir.join(SIGNED_MANIFEST_FILE), raw_manifest)?;
    write_atomic(&dir.join(SIGNED_SIGNATURE_FILE), signature)?;
    Ok(())
}

pub fn load_signed_core_trust(root: &Path, version: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let dir = slot_root(root, version);
    let raw = std::fs::read(dir.join(SIGNED_MANIFEST_FILE)).ok()?;
    let signature = std::fs::read(dir.join(SIGNED_SIGNATURE_FILE)).ok()?;
    if raw.is_empty() || signature.is_empty() {
        return None;
    }
    Some((raw, signature))
}

/// Staging a candidate only writes `pending`. Active launch identity is
/// untouched, including any recorded launch id / lease.
pub fn set_pending(root: &Path, pending: CoreSlot) -> std::io::Result<CoreSlots> {
    let mut slots = CoreSlots::load(root);
    let previous_active = slots.active.clone();
    slots.pending = Some(pending);
    slots.save(root)?;
    debug_assert_eq!(slots.active, previous_active);
    Ok(slots)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreConclude {
    None,
    Promoted,
    RolledBack { reason: String },
    LeftPending { reason: String },
}

/// Next-start apply. Download never calls this; only a live attestation does.
pub fn conclude_pending_core(
    root: &Path,
    input: super::apply::CorePromoteInput,
) -> Result<CoreConclude, String> {
    if pending_core(root).is_none() {
        return Ok(CoreConclude::None);
    }
    match super::apply::core_may_promote(&input) {
        super::apply::ApplyDecision::Allow => {
            promote_pending(root).map_err(|error| error.to_string())?;
            Ok(CoreConclude::Promoted)
        }
        super::apply::ApplyDecision::Refuse { reason }
            if !input.user_turn_accepted
                || reason == "attestationFailedBeforeTurn"
                || reason == "pendingUnverified"
                || reason == "bridgeIncompatible"
                || reason == "officialProtocolIncompatible" =>
        {
            abort_unpromoted_pending(root).map_err(|error| error.to_string())?;
            Ok(CoreConclude::RolledBack { reason })
        }
        super::apply::ApplyDecision::Refuse { reason }
        | super::apply::ApplyDecision::Wait { reason } => Ok(CoreConclude::LeftPending { reason }),
    }
}

pub fn promote_pending(root: &Path) -> std::io::Result<CoreSlots> {
    let mut slots = CoreSlots::load(root);
    if let Some(pending) = slots.pending.take() {
        if let Some(active) = slots.active.take() {
            slots.previous = Some(active);
        }
        slots.active = Some(pending);
    }
    slots.save(root)?;
    Ok(slots)
}

pub fn rollback_to_previous(root: &Path) -> std::io::Result<CoreSlots> {
    let mut slots = CoreSlots::load(root);
    if let Some(previous) = slots.previous.take() {
        let rolled_back = slots.active.take();
        slots.active = Some(previous);
        slots.previous = rolled_back;
        // A rollback is a refusal to launch the reverted candidate again.
        slots.pending = None;
    }
    slots.save(root)?;
    Ok(slots)
}

/// Next-start apply never promoted, so "rollback before the user turn" means
/// drop `pending` and keep the running `active` identity.
fn abort_unpromoted_pending(root: &Path) -> std::io::Result<CoreSlots> {
    let mut slots = CoreSlots::load(root);
    if slots.previous.is_some() && slots.pending.is_none() {
        return rollback_to_previous(root);
    }
    slots.pending = None;
    slots.save(root)?;
    Ok(slots)
}

pub fn bundled_baseline(data_root: &Path) -> Option<ResolvedCore> {
    let lock = crate::enhanced_runtime::desktop_manager::LOCK_BYTES;
    let digest = format!("sha256:{:x}", Sha256::digest(lock));
    let path = crate::enhanced_runtime::desktop_manager::packaged_enhanced_executable(data_root)?;
    Some(ResolvedCore {
        source: RuntimeSource::BundledLockfile,
        version: "bundled".into(),
        digest,
        protocol: String::new(),
        path,
        helpers: Vec::new(),
    })
}

/// Resolver used by prepare / launch / diagnostics. Path existence or a
/// version string is never enough: the slot must name a verified digest.
pub fn resolve_enhanced_runtime(data_root: &Path) -> Option<ResolvedCore> {
    resolve_enhanced_runtime_with(data_root, &TrustStore::bundled())
}

pub fn resolve_enhanced_runtime_with(data_root: &Path, trust: &TrustStore) -> Option<ResolvedCore> {
    let slots = CoreSlots::load(data_root);
    if let Some(active) = slots.active {
        if verified_slot(data_root, &active, trust) {
            return Some(ResolvedCore {
                source: RuntimeSource::SignedDownload,
                version: active.version,
                digest: active.digest,
                protocol: active.protocol,
                path: active.path,
                helpers: active.helpers,
            });
        }
    }
    bundled_baseline(data_root)
}

/// Pending is visible for diagnostics and next-start apply, never as the
/// running launch identity.
pub fn pending_core(data_root: &Path) -> Option<CoreSlot> {
    CoreSlots::load(data_root).pending
}

pub fn next_start_should_apply_pending(data_root: &Path) -> bool {
    pending_core(data_root).is_some()
}

pub fn signed_core_digest(root: &Path, hash: &str) -> bool {
    signed_core_digest_with(root, hash, &TrustStore::bundled())
}

pub fn signed_core_digest_with(root: &Path, hash: &str, trust: &TrustStore) -> bool {
    signed_core_identity_with(root, None, hash, trust).is_some()
}

pub fn signed_core_identity(root: &Path, path: &Path, hash: &str) -> Option<CoreSlot> {
    signed_core_identity_with(root, Some(path), hash, &TrustStore::bundled())
}

/// Re-verify the persisted signed manifest, then the full slot tree.
/// Unsigned `core-slots.json` is only an index of versions, not a trust root.
pub fn signed_core_identity_with(
    root: &Path,
    path: Option<&Path>,
    hash: &str,
    trust: &TrustStore,
) -> Option<CoreSlot> {
    let slots = CoreSlots::load(root);
    let normalized = hash.strip_prefix("sha256:").unwrap_or(hash);
    let candidates = [slots.active, slots.pending, slots.previous];
    candidates.into_iter().flatten().find(|slot| {
        let digest = slot.digest.strip_prefix("sha256:").unwrap_or(&slot.digest);
        if digest != normalized {
            return false;
        }
        if let Some(path) = path {
            if canonical(&slot.path) != canonical(path) {
                return false;
            }
        }
        verified_slot(root, slot, trust)
    })
}

fn verified_slot(root: &Path, slot: &CoreSlot, trust: &TrustStore) -> bool {
    let Some((raw, signature)) = load_signed_core_trust(root, &slot.version) else {
        return false;
    };
    let Ok(manifest) = verify_signed_manifest(&raw, &signature, trust) else {
        return false;
    };
    let Some(core) = manifest.core.as_ref() else {
        return false;
    };
    let Some(target) = core.target(current_platform(), current_arch()) else {
        return false;
    };
    let tree = slot_tree_root(root, &slot.version);
    if slot.version != manifest.version
        || !tree.join(SLOT_COMPLETE_MARKER).is_file()
        || verify_tree_against_signed_target(&tree, target).is_err()
    {
        return false;
    }
    let executable = tree.join(&target.executable);
    if canonical(&executable) != canonical(&slot.path) || !executable.is_file() {
        return false;
    }
    let expected = target
        .files
        .iter()
        .find(|file| file.path == target.executable)
        .map(|file| file.sha256.as_str())
        .unwrap_or("");
    let expected = expected.strip_prefix("sha256:").unwrap_or(expected);
    let digest = slot.digest.strip_prefix("sha256:").unwrap_or(&slot.digest);
    let expected_protocol = if target.protocol_schema_sha256.is_empty() {
        core.protocol_schema_sha256.as_str()
    } else {
        target.protocol_schema_sha256.as_str()
    };
    let expected_helpers = target
        .helpers
        .iter()
        .map(|helper| tree.join(helper))
        .collect::<Vec<_>>();
    let helpers_match = slot.helpers.len() == expected_helpers.len()
        && slot
            .helpers
            .iter()
            .zip(&expected_helpers)
            .all(|(actual, expected)| canonical(actual) == canonical(expected));
    let expected_files = target
        .files
        .iter()
        .map(|file| (canonical(&tree.join(&file.path)), file))
        .collect::<HashMap<_, _>>();
    let mut observed_files = HashSet::new();
    let files_match = slot.files.len() == expected_files.len()
        && slot.files.iter().all(|actual| {
            let path = canonical(&actual.path);
            let Some(expected) = expected_files.get(&path) else {
                return false;
            };
            observed_files.insert(path)
                && actual.size == expected.size
                && actual
                    .sha256
                    .strip_prefix("sha256:")
                    .unwrap_or(&actual.sha256)
                    .eq_ignore_ascii_case(
                        expected
                            .sha256
                            .strip_prefix("sha256:")
                            .unwrap_or(&expected.sha256),
                    )
        });
    digest == expected
        && slot.protocol == expected_protocol
        && slot.protocol_compat == core.protocol_compat.0
        && slot.enhanced_commit == core.enhanced_commit
        && slot.feature_profile == core.feature_profile
        && helpers_match
        && files_match
        && super::manifest::verify_asset_file(&executable, expected).is_ok()
        && expected_helpers.iter().all(|helper| helper.is_file())
}

fn verify_tree_against_signed_target(root: &Path, target: &CoreTarget) -> Result<(), String> {
    if !root.is_dir() {
        return Err("signed core tree is missing".into());
    }
    let expected = target
        .files
        .iter()
        .map(|file| (normalize_tree_path(&file.path, &target.platform), file))
        .collect::<HashMap<_, _>>();
    let mut observed = HashSet::new();
    collect_tree_files(root, root, &mut |relative, path| {
        if relative == SLOT_COMPLETE_MARKER {
            return Ok(());
        }
        let key = normalize_tree_path(&relative, &target.platform);
        let Some(file) = expected.get(&key) else {
            return Err(format!("core tree contains unsigned file {relative}"));
        };
        let size = std::fs::metadata(path)
            .map_err(|error| error.to_string())?
            .len();
        if size != file.size {
            return Err(format!(
                "core file {relative} size {size} does not match signed size {}",
                file.size
            ));
        }
        super::manifest::verify_asset_file(path, &file.sha256)
            .map_err(|error| error.to_string())?;
        observed.insert(key);
        Ok(())
    })?;
    for path in expected.keys() {
        if !observed.contains(path) {
            return Err(format!("core tree is missing signed file {path}"));
        }
    }
    Ok(())
}

fn normalize_tree_path(path: &str, platform: &str) -> String {
    let path = path.replace('\\', "/");
    if platform == "windows" {
        path.to_ascii_lowercase()
    } else {
        path
    }
}

fn collect_tree_files(
    root: &Path,
    dir: &Path,
    visit: &mut impl FnMut(String, &Path) -> Result<(), String>,
) -> Result<(), String> {
    for entry in std::fs::read_dir(dir).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "core tree contains forbidden link {}",
                path.display()
            ));
        }
        if metadata.is_dir() {
            collect_tree_files(root, &path, visit)?;
        } else if metadata.is_file() {
            let relative = path
                .strip_prefix(root)
                .map_err(|error| error.to_string())?
                .to_string_lossy()
                .replace('\\', "/");
            visit(relative, &path)?;
        } else {
            return Err(format!(
                "core tree contains unsupported entry {}",
                path.display()
            ));
        }
    }
    Ok(())
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

static APPLY_PENDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn arm_core_pending_apply() {
    APPLY_PENDING.store(true, std::sync::atomic::Ordering::SeqCst);
}

pub fn consume_core_pending_apply() -> bool {
    APPLY_PENDING.swap(false, std::sync::atomic::Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(name: &str) -> CoreSlot {
        CoreSlot {
            version: name.into(),
            digest: format!("sha256:{name}"),
            protocol: "p".into(),
            protocol_compat: "*".into(),
            path: PathBuf::from(name),
            helpers: vec![PathBuf::from(format!("{name}-helper"))],
            files: Vec::new(),
            enhanced_commit: "commit".into(),
            feature_profile: "profile".into(),
            launch_id: Some("launch-live".into()),
        }
    }

    #[test]
    fn download_sets_pending_only() {
        let dir = tempfile::tempdir().unwrap();
        let initial = CoreSlots {
            active: Some(slot("active")),
            ..CoreSlots::default()
        };
        initial.save(dir.path()).unwrap();
        let after = set_pending(dir.path(), slot("pending")).unwrap();
        assert_eq!(after.active.as_ref().unwrap().version, "active");
        assert_eq!(
            after.active.as_ref().unwrap().launch_id.as_deref(),
            Some("launch-live")
        );
        assert_eq!(after.pending.as_ref().unwrap().version, "pending");
    }

    #[test]
    fn promote_moves_active_to_previous() {
        let dir = tempfile::tempdir().unwrap();
        let slots = CoreSlots {
            active: Some(slot("old")),
            pending: Some(slot("new")),
            ..CoreSlots::default()
        };
        slots.save(dir.path()).unwrap();
        let after = promote_pending(dir.path()).unwrap();
        assert_eq!(after.active.unwrap().version, "new");
        assert_eq!(after.previous.unwrap().version, "old");
        assert!(after.pending.is_none());
    }

    #[test]
    fn conclude_promotes_only_when_core_may_promote_allows() {
        let dir = tempfile::tempdir().unwrap();
        let slots = CoreSlots {
            active: Some(slot("old")),
            pending: Some(slot("new")),
            ..CoreSlots::default()
        };
        slots.save(dir.path()).unwrap();
        let refused = conclude_pending_core(
            dir.path(),
            super::super::apply::CorePromoteInput {
                pending_verified: false,
                bridge_compatible: true,
                official_protocol_ok: true,
                attestation_ok: true,
                user_turn_accepted: false,
            },
        )
        .unwrap();
        assert!(matches!(refused, CoreConclude::RolledBack { .. }));
        assert_eq!(CoreSlots::load(dir.path()).active.unwrap().version, "old");

        let slots = CoreSlots {
            active: Some(slot("old")),
            pending: Some(slot("new")),
            ..CoreSlots::default()
        };
        slots.save(dir.path()).unwrap();
        let allowed = conclude_pending_core(
            dir.path(),
            super::super::apply::CorePromoteInput {
                pending_verified: true,
                bridge_compatible: true,
                official_protocol_ok: true,
                attestation_ok: true,
                user_turn_accepted: false,
            },
        )
        .unwrap();
        assert_eq!(allowed, CoreConclude::Promoted);
        let after = CoreSlots::load(dir.path());
        assert_eq!(after.active.unwrap().version, "new");
        assert_eq!(after.previous.unwrap().version, "old");
        assert!(after.pending.is_none());
    }

    #[test]
    fn conclude_rolls_back_failed_attestation_before_user_turn() {
        let dir = tempfile::tempdir().unwrap();
        let slots = CoreSlots {
            active: Some(slot("old")),
            pending: Some(slot("new")),
            ..CoreSlots::default()
        };
        slots.save(dir.path()).unwrap();
        let outcome = conclude_pending_core(
            dir.path(),
            super::super::apply::CorePromoteInput {
                pending_verified: true,
                bridge_compatible: true,
                official_protocol_ok: true,
                attestation_ok: false,
                user_turn_accepted: false,
            },
        )
        .unwrap();
        assert!(
            matches!(outcome, CoreConclude::RolledBack { reason } if reason == "attestationFailedBeforeTurn")
        );
        let after = CoreSlots::load(dir.path());
        assert_eq!(after.active.unwrap().version, "old");
        assert!(after.pending.is_none());
    }

    #[test]
    fn helpers_travel_with_the_slot() {
        let pending = slot("set");
        assert_eq!(pending.helpers.len(), 1);
        let dir = tempfile::tempdir().unwrap();
        set_pending(dir.path(), pending).unwrap();
        assert!(next_start_should_apply_pending(dir.path()));
    }

    #[test]
    fn rollback_does_not_requeue_the_failed_core() {
        let dir = tempfile::tempdir().unwrap();
        CoreSlots {
            active: Some(slot("new")),
            previous: Some(slot("old")),
            ..CoreSlots::default()
        }
        .save(dir.path())
        .unwrap();
        let after = rollback_to_previous(dir.path()).unwrap();
        assert_eq!(after.active.unwrap().version, "old");
        assert_eq!(after.previous.unwrap().version, "new");
        assert!(after.pending.is_none());
        assert!(!next_start_should_apply_pending(dir.path()));
    }

    fn write_planted_tree(root: &Path, version: &str, extra: bool) -> (PathBuf, String) {
        let tree = slot_tree_root(root, version);
        std::fs::create_dir_all(&tree).unwrap();
        let exe = tree.join("codex.exe");
        let helper = tree.join("codex-command-runner.exe");
        std::fs::write(&exe, b"planted-core").unwrap();
        std::fs::write(&helper, b"planted-helper").unwrap();
        std::fs::write(tree.join(SLOT_COMPLETE_MARKER), b"complete").unwrap();
        if extra {
            std::fs::write(tree.join("evil.dll"), b"sidecar").unwrap();
        }
        let digest = format!("sha256:{:x}", Sha256::digest(b"planted-core"));
        (exe, digest)
    }

    fn planted_slot(root: &Path, version: &str, exe: &Path, digest: &str) -> CoreSlot {
        let tree = slot_tree_root(root, version);
        let helper = tree.join("codex-command-runner.exe");
        CoreSlot {
            version: version.into(),
            digest: digest.into(),
            protocol: format!("sha256:{}", "cd".repeat(32)),
            protocol_compat: "*".into(),
            path: exe.to_path_buf(),
            helpers: vec![helper.clone()],
            files: vec![
                CoreSlotFile {
                    path: exe.to_path_buf(),
                    size: 12,
                    sha256: digest.strip_prefix("sha256:").unwrap_or(digest).into(),
                },
                CoreSlotFile {
                    path: helper,
                    size: 14,
                    sha256: format!("{:x}", Sha256::digest(b"planted-helper")),
                },
            ],
            enhanced_commit: "b".repeat(40),
            feature_profile: "enhanced-mvp-v1".into(),
            launch_id: None,
        }
    }

    #[test]
    fn unsigned_planted_slot_json_is_not_a_signed_identity() {
        let dir = tempfile::tempdir().unwrap();
        let (exe, digest) = write_planted_tree(dir.path(), "9.9.9", false);
        CoreSlots {
            active: Some(planted_slot(dir.path(), "9.9.9", &exe, &digest)),
            ..CoreSlots::default()
        }
        .save(dir.path())
        .unwrap();
        assert!(
            signed_core_identity(dir.path(), &exe, &digest).is_none(),
            "unsigned core-slots.json must not mint a signed identity"
        );
        assert!(!signed_core_digest(dir.path(), &digest));
        assert!(resolve_enhanced_runtime(dir.path())
            .is_none_or(|resolved| resolved.source == RuntimeSource::BundledLockfile));
    }

    fn signed_core_fixture(
        signing: &ed25519_dalek::SigningKey,
        extra: bool,
    ) -> (Vec<u8>, Vec<u8>, super::super::manifest::UpdateManifest) {
        use super::super::manifest::{
            sign_raw, AssetRef, CompatRange, CoreFile, CoreManifest, DataFormat, UpdateManifest,
            MANIFEST_SCHEMA_VERSION,
        };
        use crate::updates::UpdateComponent;
        let mut files = vec![
            CoreFile {
                path: "codex.exe".into(),
                size: 12,
                sha256: format!("{:x}", Sha256::digest(b"planted-core")),
                executable: true,
            },
            CoreFile {
                path: "codex-command-runner.exe".into(),
                size: 14,
                sha256: format!("{:x}", Sha256::digest(b"planted-helper")),
                executable: true,
            },
        ];
        if extra {
            files.push(CoreFile {
                path: "evil.dll".into(),
                size: 7,
                sha256: format!("{:x}", Sha256::digest(b"sidecar")),
                executable: false,
            });
        }
        let uncompressed: u64 = files.iter().map(|file| file.size).sum();
        let manifest = UpdateManifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            component: UpdateComponent::Core,
            version: "9.9.9".into(),
            source_commit: "c".into(),
            release_tag: "core-v9.9.9".into(),
            sequence: 1,
            key_id: "test-key".into(),
            prerelease: false,
            release_notes: None,
            min_desktop_version: "0.1.0".into(),
            bridge_api_compat: CompatRange::new("*"),
            remote_protocol_compat: CompatRange::new("*"),
            data_format: DataFormat::default(),
            assets: vec![AssetRef {
                platform: current_platform().into(),
                arch: current_arch().into(),
                name: "payload.zip".into(),
                size: 1,
                sha256: "aa".repeat(32),
            }],
            core: Some(CoreManifest {
                upstream_commit: "a".repeat(40),
                enhanced_commit: "b".repeat(40),
                feature_profile: "enhanced-mvp-v1".into(),
                protocol_schema_sha256: format!("sha256:{}", "ab".repeat(32)),
                protocol_compat: CompatRange::new("*"),
                targets: vec![super::super::manifest::CoreTarget {
                    platform: current_platform().into(),
                    arch: current_arch().into(),
                    asset: "payload.zip".into(),
                    executable: "codex.exe".into(),
                    helpers: vec!["codex-command-runner.exe".into()],
                    uncompressed_size: uncompressed,
                    protocol_schema_sha256: format!("sha256:{}", "cd".repeat(32)),
                    files,
                }],
            }),
        };
        let raw = serde_json::to_vec(&manifest).unwrap();
        let sig = sign_raw(&raw, signing).to_vec();
        (raw, sig, manifest)
    }

    #[test]
    fn unlisted_sidecar_in_the_slot_tree_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let signing = ed25519_dalek::SigningKey::from_bytes(&[11u8; 32]);
        let trust = TrustStore::from_key("test-key", signing.verifying_key());
        let (exe, digest) = write_planted_tree(dir.path(), "9.9.9", true);
        let (raw, sig, _) = signed_core_fixture(&signing, false);
        persist_signed_core_trust(dir.path(), "9.9.9", &raw, &sig).unwrap();
        CoreSlots {
            active: Some(planted_slot(dir.path(), "9.9.9", &exe, &digest)),
            ..CoreSlots::default()
        }
        .save(dir.path())
        .unwrap();
        assert!(
            signed_core_identity_with(dir.path(), Some(&exe), &digest, &trust).is_none(),
            "an unlisted DLL/sidecar must not verify"
        );
    }

    #[test]
    fn signed_blob_and_exact_tree_are_required_before_identity() {
        let dir = tempfile::tempdir().unwrap();
        let signing = ed25519_dalek::SigningKey::from_bytes(&[11u8; 32]);
        let trust = TrustStore::from_key("test-key", signing.verifying_key());
        let (exe, digest) = write_planted_tree(dir.path(), "9.9.9", false);
        let (raw, sig, _) = signed_core_fixture(&signing, false);
        persist_signed_core_trust(dir.path(), "9.9.9", &raw, &sig).unwrap();
        CoreSlots {
            active: Some(planted_slot(dir.path(), "9.9.9", &exe, &digest)),
            ..CoreSlots::default()
        }
        .save(dir.path())
        .unwrap();
        assert!(signed_core_identity_with(dir.path(), Some(&exe), &digest, &trust).is_some());
        assert!(signed_core_identity(dir.path(), &exe, &digest).is_none());
    }

    #[test]
    fn unsigned_helper_path_cannot_escape_the_signed_slot() {
        let dir = tempfile::tempdir().unwrap();
        let signing = ed25519_dalek::SigningKey::from_bytes(&[11u8; 32]);
        let trust = TrustStore::from_key("test-key", signing.verifying_key());
        let (exe, digest) = write_planted_tree(dir.path(), "9.9.9", false);
        let (raw, sig, _) = signed_core_fixture(&signing, false);
        persist_signed_core_trust(dir.path(), "9.9.9", &raw, &sig).unwrap();
        let outside = dir.path().join("attacker-helper.exe");
        std::fs::write(&outside, b"planted-helper").unwrap();
        let mut slot = planted_slot(dir.path(), "9.9.9", &exe, &digest);
        slot.helpers = vec![outside];
        CoreSlots {
            active: Some(slot),
            ..CoreSlots::default()
        }
        .save(dir.path())
        .unwrap();
        assert!(signed_core_identity_with(dir.path(), Some(&exe), &digest, &trust).is_none());
    }
}
