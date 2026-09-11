//! Is Enhanced usable on this machine right now?
//!
//! The bridge gate proves the bridge is correct; the installed gate proves the
//! packaged Codex Desktop adopts it. Both are expensive, and the installed one
//! restarts the user's app twice. Most of the time the question is smaller:
//! given what is on disk and in the environment at this second, would pressing
//! the buttons work — and if not, which button?
//!
//! Answering that by reading the Settings screen is what this exists to
//! replace. The checks live here rather than in the printer because the
//! installed gate records the same ones: a machine this call refuses must
//! never be able to produce a green qualification report.

use std::path::{Path, PathBuf};

use crate::enhanced_runtime::{desktop_runtime_status, DesktopRuntimeStatus};

use super::CaseResult;

/// What can be established without a restart, an attestation, or a provider.
pub struct Preflight {
    pub status: DesktopRuntimeStatus,
    pub checks: Vec<CaseResult>,
}

impl Preflight {
    pub fn passed(&self) -> bool {
        self.checks.iter().all(|check| check.passed)
    }

    /// The failures only, in the order they would block the user.
    pub fn blockers(&self) -> Vec<&CaseResult> {
        self.checks.iter().filter(|check| !check.passed).collect()
    }
}

/// Everything here reads; nothing writes, leases, or restarts.
pub fn evaluate(data_root: &Path) -> Preflight {
    let status = desktop_runtime_status(data_root);
    let mut checks = Vec::new();

    let configured = status.configured && status.enabled;
    checks.push(CaseResult::new(
        "enhanced-is-configured-and-enabled",
        configured,
        if configured {
            format!(
                "enhanced={}",
                display(status.enhanced_codex_executable.as_deref())
            )
        } else if status.configured {
            "Enhanced is configured but switched off".into()
        } else {
            "no Enhanced Runtime settings have been written yet".into()
        },
    ));

    // The blockers are one flat list for the whole reading, so they cannot be
    // attributed to a single check without guessing. Carry them, because the
    // gate's report has nowhere else to put a reason — but do not phrase them
    // as if every line were about the artifact. Two of the three printed here
    // are usually the live bridge's business, not this check's.
    checks.push(CaseResult::new(
        "the-pinned-artifact-verifies",
        status.artifact_ready,
        if status.artifact_ready {
            format!(
                "digest={}",
                status.enhanced_runtime_digest.clone().unwrap_or_default()
            )
        } else {
            format!(
                "the artifact did not verify; the runtime reported: {}",
                joined(&status.blockers)
            )
        },
    ));

    // `leased` is ours and current; `released` is clean and ready to be taken.
    // Everything else — a foreign value, an older build's sidecar with no
    // lease, an unreadable environment — means the handover cannot happen and
    // no amount of restarting will change that.
    let lease_ok = matches!(status.environment_state.as_str(), "leased" | "released");
    checks.push(CaseResult::new(
        "the-codex-cli-path-lease-is-ours-or-clear",
        lease_ok,
        format!(
            "state={} value={}",
            status.environment_state,
            status
                .environment_value
                .clone()
                .unwrap_or_else(|| "<unset>".into())
        ),
    ));

    checks.push(live_bridge_is_configured(
        status.observed_bridge_executable.as_deref(),
        status.bridge_executable.as_deref(),
    ));

    checks.push(no_sibling_vellum_host(
        &crate::enhanced_runtime::process_info::leftover_vellum_hosts(),
    ));

    checks.push(subagent_model_resolves(data_root));

    Preflight { status, checks }
}

/// The attestation records which *children* are up, never which bridge binary
/// wrote it. An older sidecar still held by Codex Desktop keeps refreshing that
/// file, so "ready, adopted, both children" can be entirely true of a build
/// that was replaced hours ago. The executables have to be compared, or the
/// gate certifies whichever bridge happened to survive.
pub fn live_bridge_is_configured(observed: Option<&Path>, configured: Option<&Path>) -> CaseResult {
    let (passed, detail) = match (observed, configured) {
        // Nothing is running. Whether Desktop has adopted anything is step
        // three's business; it is not a stale binary.
        (None, _) => (true, "no bridge process is running".to_string()),
        (Some(observed), Some(configured)) if same_path(observed, configured) => {
            (true, format!("live bridge is {}", observed.display()))
        }
        (Some(observed), Some(configured)) => (
            false,
            format!(
                "the live bridge runs from {}, but the configured bridge is {}",
                observed.display(),
                configured.display()
            ),
        ),
        (Some(observed), None) => (
            false,
            format!(
                "a bridge is running from {} while nothing is configured",
                observed.display()
            ),
        ),
    };
    CaseResult::new("any-live-bridge-is-the-configured-bridge", passed, detail)
}

/// A leftover installed Vellum host keeps 15721 and the Codex config lease
/// after this process exits. Account switching and Proxy start both fail
/// closed until that process is gone.
pub fn no_sibling_vellum_host(
    peers: &[crate::enhanced_runtime::process_info::ProcessImage],
) -> CaseResult {
    if peers.is_empty() {
        CaseResult::new(
            "no-sibling-vellum-host-owns-the-local-proxy",
            true,
            "no other Vellum host is running",
        )
    } else {
        CaseResult::new(
            "no-sibling-vellum-host-owns-the-local-proxy",
            false,
            format!(
                "another Vellum host is still running: {}",
                crate::enhanced_runtime::process_info::format_process_images(peers)
            ),
        )
    }
}

/// Shared with the bridge gate, which decides promotion eligibility the same
/// way: two paths name the same binary or they do not.
/// Does the model a spawned sub-agent would run on still exist?
///
/// Renaming or recreating a Provider route mints a new catalog id, and the
/// sub-agent setting keeps the old one. Nothing revalidates the pair, so
/// `default_subagent_model` in Codex's own config goes on naming a model the
/// catalog no longer has. The bridge then refuses the spawn — correctly, since
/// an unresolvable model must never fall through to Official and quietly spend
/// OpenAI quota — but from the outside sub-agents simply stop working, and no
/// screen says why.
fn subagent_model_resolves(data_root: &Path) -> CaseResult {
    const NAME: &str = "a-spawned-subagent-still-has-a-model-to-run-on";
    let settings: serde_json::Value = match std::fs::read(data_root.join("settings.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
    {
        Some(settings) => settings,
        None => return CaseResult::new(NAME, true, "no Vellum settings to check"),
    };
    let subagent = &settings["subagent"];
    if subagent["mode"].as_str() != Some("custom") {
        return CaseResult::new(NAME, true, "sub-agents inherit Codex's own default");
    }
    let Some(catalog_id) = subagent["catalogId"].as_str().filter(|id| !id.is_empty()) else {
        return CaseResult::new(NAME, true, "no sub-agent model is pinned");
    };
    let map_path = data_root.join("enhanced-runtime/model-provider-map.json");
    let Ok(map) = crate::enhanced_runtime::TrustedModelProviderMap::read(&map_path) else {
        return CaseResult::new(
            NAME,
            true,
            "no model provider map yet; it is written when Enhanced is armed",
        );
    };
    match map.resolve(catalog_id) {
        Some(route) => {
            CaseResult::new(NAME, true, format!("{catalog_id} -> {}", route.provider_id))
        }
        None => CaseResult::new(
            NAME,
            false,
            format!(
                "the sub-agent model {catalog_id} is not in the catalog any more; \
                 re-pick the sub-agent model in Settings (a renamed Provider route mints a new id)"
            ),
        ),
    }
}

pub(super) fn same_path(left: &Path, right: &Path) -> bool {
    canonical(left) == canonical(right)
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn display(path: Option<&Path>) -> String {
    path.map(|path| path.display().to_string())
        .unwrap_or_else(|| "<unset>".into())
}

fn joined(blockers: &[String]) -> String {
    if blockers.is_empty() {
        "no reason was recorded".into()
    } else {
        blockers.join("; ")
    }
}

/// The single sentence worth printing: what to do next, in the vocabulary of
/// the Settings card, so the CLI and the screen never disagree.
pub fn next_action(status: &DesktopRuntimeStatus) -> &'static str {
    match status.activation_state.as_str() {
        "active" => "nothing — Enhanced is serving this Codex Desktop",
        "disabled" => "turn Enhanced on in Settings and verify the three binaries",
        "artifactBlocked" => "fix the Enhanced core path, then verify again",
        "environmentDrift" => {
            "release the takeover first (Settings, `Disable and release`), then enable again"
        }
        "disablePendingRestart" => "restart Codex Desktop to finish disabling",
        "failed" => "read the blockers below; the bridge itself reported failed",
        _ => "restart Codex Desktop through Vellum (Settings, `Take over and restart`)",
    }
}

/// `vellum-eval enhanced-runtime-status [--json]`
pub fn run(args: &[String]) -> crate::error::AppResult<()> {
    let data_root = crate::state::app_data_dir();
    let preflight = evaluate(&data_root);
    let status = &preflight.status;

    if args.iter().any(|argument| argument == "--json") {
        let encoded = serde_json::json!({
            "dataRoot": data_root,
            "ready": status.ready,
            "activationState": status.activation_state,
            "environmentState": status.environment_state,
            "environmentValue": status.environment_value,
            "observedBridgeExecutable": status.observed_bridge_executable,
            "bridgeExecutable": status.bridge_executable,
            "restartRequired": status.restart_required,
            "launchId": status.launch_id,
            "bridgeState": status.bridge_state,
            "checks": preflight.checks,
            "blockers": status.blockers,
            "nextAction": next_action(status),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&encoded)
                .map_err(|error| crate::error::AppError::Message(error.to_string()))?
        );
    } else {
        for check in &preflight.checks {
            println!(
                "{} {:<42} {}",
                if check.passed { "PASS" } else { "FAIL" },
                check.name,
                check.detail
            );
        }
        println!();
        // The state words cannot be checked by hand; the paths can. Print both.
        row("activationState", &status.activation_state);
        row("environmentState", &status.environment_state);
        row(
            "CODEX_CLI_PATH",
            &status
                .environment_value
                .clone()
                .unwrap_or_else(|| "<unset>".into()),
        );
        row(
            "configuredBridge",
            &display(status.bridge_executable.as_deref()),
        );
        row(
            "liveBridge",
            &display(status.observed_bridge_executable.as_deref()),
        );
        row("launchId", status.launch_id.as_deref().unwrap_or("<none>"));
        row(
            "bridgeState",
            status.bridge_state.as_deref().unwrap_or("<none>"),
        );
        row(
            "restartRequired",
            if status.restart_required { "yes" } else { "no" },
        );
        if !status.blockers.is_empty() {
            println!();
            println!("Blockers:");
            for blocker in &status.blockers {
                println!("  - {blocker}");
            }
        }
        println!();
        println!(
            "Enhanced Runtime: {}",
            if status.ready { "READY" } else { "NOT READY" }
        );
        println!("Next: {}", next_action(status));
    }

    if status.ready {
        Ok(())
    } else {
        // Exit non-zero so a rebuild script can stop here rather than continue
        // into a gate that would only report the same thing more slowly.
        Err(crate::error::AppError::Message(format!(
            "Enhanced Runtime is not ready ({})",
            status.activation_state
        )))
    }
}

fn row(name: &str, value: &str) {
    println!("  {name:<18} {value}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enhanced_runtime::DesktopRuntimeStatus;

    fn status_with_activation(activation: &str) -> DesktopRuntimeStatus {
        let mut status = crate::enhanced_runtime::desktop_runtime_status(Path::new(
            "this-path-does-not-exist-so-the-runtime-reads-as-unconfigured",
        ));
        status.activation_state = activation.into();
        status
    }

    /// The state that cost an evening: a fresh launch manifest, a live
    /// attestation naming both children, and every turn served by the bridge a
    /// previous build left running.
    #[test]
    fn a_live_bridge_from_another_build_fails_the_preflight() {
        let check = live_bridge_is_configured(
            Some(Path::new(
                "C:/vellum/target/debug/vellum-codex-app-server.exe",
            )),
            Some(Path::new(
                "C:/vellum/binaries/dev/vellum-codex-app-server-d9ac.exe",
            )),
        );
        assert!(!check.passed);
        assert!(check.detail.contains("target/debug"));
        assert!(check.detail.contains("binaries/dev"));
    }

    #[test]
    fn nothing_running_is_not_a_stale_bridge() {
        let check = live_bridge_is_configured(
            None,
            Some(Path::new(
                "C:/vellum/binaries/dev/vellum-codex-app-server-d9ac.exe",
            )),
        );
        assert!(check.passed);
    }

    #[test]
    fn the_configured_bridge_running_is_the_only_pass_with_a_live_process() {
        let bridge = Path::new("C:/vellum/binaries/dev/vellum-codex-app-server-d9ac.exe");
        assert!(live_bridge_is_configured(Some(bridge), Some(bridge)).passed);
        assert!(!live_bridge_is_configured(Some(bridge), None).passed);
    }

    /// The CLI and the Settings card must never disagree about what to do next,
    /// so every activation state the status machine can produce has a sentence.
    #[test]
    fn every_activation_state_names_a_next_action() {
        for activation in [
            "active",
            "disabled",
            "artifactBlocked",
            "environmentDrift",
            "disablePendingRestart",
            "failed",
            "awaitingDesktopRestart",
        ] {
            let status = status_with_activation(activation);
            let action = next_action(&status);
            assert!(!action.is_empty(), "{activation} has no next action");
            if activation == "active" {
                assert!(action.contains("nothing"));
            }
        }
    }

    /// An unconfigured machine is refused before anything is restarted, and the
    /// refusal says which of the four things is missing.
    #[test]
    fn an_unconfigured_machine_fails_the_first_check_and_not_the_bridge_one() {
        let temp = tempfile::tempdir().unwrap();
        let preflight = evaluate(temp.path());
        assert!(!preflight.passed());
        let first = &preflight.checks[0];
        assert_eq!(first.name, "enhanced-is-configured-and-enabled");
        assert!(!first.passed);
        let bridge = preflight
            .checks
            .iter()
            .find(|check| check.name == "any-live-bridge-is-the-configured-bridge")
            .unwrap();
        assert!(
            bridge.passed,
            "nothing is configured and nothing is running"
        );
    }

    /// The exact state found on 2026-09-03: the route had been recreated as
    /// `806-2`, which minted `vlm-beb60d2887-qwen`, while the sub-agent setting
    /// and Codex's `default_subagent_model` still named `vlm-8d90f51c1f-qwen`.
    #[test]
    fn a_subagent_model_left_behind_by_a_renamed_route_is_caught() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("settings.json"),
            br#"{"subagent":{"mode":"custom","catalogId":"vlm-8d90f51c1f-qwen"}}"#,
        )
        .unwrap();
        std::fs::create_dir_all(temp.path().join("enhanced-runtime")).unwrap();
        std::fs::write(
            temp.path().join("enhanced-runtime/model-provider-map.json"),
            br#"{"schemaVersion":1,"models":{"vlm-beb60d2887-qwen":{"providerId":"806-2","childProviderId":"vellum"}}}"#,
        )
        .unwrap();
        let check = subagent_model_resolves(temp.path());
        assert!(!check.passed);
        assert!(check.detail.contains("vlm-8d90f51c1f-qwen"));
    }

    #[test]
    fn a_subagent_model_that_is_still_in_the_catalog_passes() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("settings.json"),
            br#"{"subagent":{"mode":"custom","catalogId":"vlm-beb60d2887-qwen"}}"#,
        )
        .unwrap();
        std::fs::create_dir_all(temp.path().join("enhanced-runtime")).unwrap();
        std::fs::write(
            temp.path().join("enhanced-runtime/model-provider-map.json"),
            br#"{"schemaVersion":1,"models":{"vlm-beb60d2887-qwen":{"providerId":"806-2","childProviderId":"vellum"}}}"#,
        )
        .unwrap();
        let check = subagent_model_resolves(temp.path());
        assert!(check.passed, "{}", check.detail);
        assert!(check.detail.contains("806-2"));
    }

    #[test]
    fn a_leftover_installed_vellum_host_fails_closed() {
        let peers = [crate::enhanced_runtime::process_info::ProcessImage {
            pid: 41264,
            executable: PathBuf::from(
                r"C:\Users\developer\AppData\Local\Vellum\vellum-proxy-desktop.exe",
            ),
        }];
        let check = no_sibling_vellum_host(&peers);
        assert!(!check.passed);
        assert!(check.detail.contains("41264"));
        assert!(check.detail.contains("vellum-proxy-desktop.exe"));
        assert!(no_sibling_vellum_host(&[]).passed);
    }
}
