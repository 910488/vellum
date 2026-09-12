//! Bundled lockfile vs signed downloaded core: two trust sources.
//! `active` / `pending` / `previous` persist separately. Download never
//! mutates the running launch identity.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::write_atomic;

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
    pub path: PathBuf,
    pub helpers: Vec<PathBuf>,
    pub launch_id: Option<String>,
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
        if let Some(active) = slots.active.take() {
            slots.pending = Some(active);
        }
        slots.active = Some(previous);
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
    let slots = CoreSlots::load(data_root);
    if let Some(active) = slots.active {
        if active.path.is_file() && !active.digest.is_empty() {
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
    let slots = CoreSlots::load(root);
    let normalized = hash.strip_prefix("sha256:").unwrap_or(hash);
    let candidates = [slots.active, slots.pending, slots.previous];
    candidates.iter().flatten().any(|slot| {
        let digest = slot.digest.strip_prefix("sha256:").unwrap_or(&slot.digest);
        digest == normalized
            && slot.path.is_file()
            && slot
                .helpers
                .iter()
                .all(|helper| helper.is_file() || helper.as_os_str().is_empty())
    })
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
            path: PathBuf::from(name),
            helpers: vec![PathBuf::from(format!("{name}-helper"))],
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
}
