//! Remote Manager end-to-end against the disposable dev host.
//!
//! Every step here is the code Desktop runs: `discovery::discover` reads the
//! real `~/.ssh/config`, `import_discovered_host` writes the real cache, and
//! `bootstrap` opens real SSH connections that install real binaries onto a
//! real systemd host. Nothing is faked. What makes it a dev loop rather than
//! an acceptance run is only the target: `scripts/dev-host.ps1` boots a
//! container that offers Docker, user systemd and lingering, and throws it
//! away afterwards, so no machine has to be borrowed and no full installer
//! has to be built.
//!
//! These cases need a *pristine* host, and `scripts/remote-e2e.ps1` resets one
//! before running them. Each case builds its own throwaway Desktop data root,
//! and the boundary key is a pairing between one such root and one host, so a
//! host still running a Codex daemon paired to a data root that no longer
//! exists fails at `BoundaryNativeRestartIdentityMissing`. That is the design
//! working, not a flake: re-run through the script, or `pnpm run dev:host:reset`
//! before invoking cargo directly.
//!
//! Run: `pnpm run remote:e2e` (see docs/remote-dev-loop.md).

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::json;
use vellum_lib::remote::deployment::{self, RemoteModelSelection, RemotePolicyOverrides};
use vellum_lib::remote::discovery::{self, RemoteHostCandidate};
use vellum_lib::remote::{bootstrap, desired_state, pinned_install, ssh_trust, RemoteHostManager};
use vellum_lib::state::AppState;

const ALIAS: &str = "vellum-dev-host";

struct Fixture {
    state: AppState,
    host_id: String,
    _data_root: tempfile::TempDir,
}

/// Resolve the dev host through the same discovery Desktop uses, then refuse
/// to continue unless it is the loopback container. These tests install
/// software and enable lingering; pointing them at a borrowed machine by way
/// of a stale alias would be a real mutation of someone else's host.
fn fixture() -> Fixture {
    let candidates = discovery::discover(&[]).expect("ssh discovery failed");
    let candidate: RemoteHostCandidate = candidates
        .into_iter()
        .find(|candidate| candidate.ssh_alias == ALIAS)
        .unwrap_or_else(|| panic!("no '{ALIAS}' in ~/.ssh/config; run: pnpm run dev:host:up"));

    let hostname = candidate.hostname.as_deref().unwrap_or_default();
    assert!(
        matches!(hostname, "127.0.0.1" | "::1" | "localhost"),
        "'{ALIAS}' resolves to {hostname}, which is not the loopback dev host. \
         These tests mutate the host they are pointed at; refusing to continue."
    );
    if let Some(error) = candidate.validation_error.as_deref() {
        panic!("'{ALIAS}' did not validate: {error}");
    }

    let data_root = tempfile::tempdir().expect("temp data root");
    let state = AppState::with_data_dir(data_root.path().to_path_buf());

    // Vellum keeps its own known_hosts and refuses to connect to a host whose
    // key nobody confirmed. In the product a person reads the fingerprint and
    // clicks confirm; here the test is that person. Standing in for them is
    // only defensible because of the loopback assertion above -- and because
    // each `dev-host.ps1 reset` really does present a new key, which this
    // re-confirms against a fresh keyscan rather than remembering the old one.
    //
    // Two roots, because the product reads two: `bootstrap` checks the
    // AppState's root while `RemoteAgentClient` falls back to
    // `ssh_trust::runtime_data_root()`, which is the real app data directory.
    // So this does leave one loopback entry in the developer's own trust
    // store; that is the honest cost of not faking the transport.
    let target = ssh_trust::resolve_ssh_target(&candidate.ssh_alias).expect("resolve ssh target");
    let pending = ssh_trust::fetch_pending_fingerprints(&target).expect("keyscan the dev host");
    let preferred =
        ssh_trust::preferred_fingerprint(pending).expect("the dev host offered no host key");
    // Confirmed unconditionally rather than only when unconfirmed: a reset
    // host presents a new key, and a remembered confirmation from the previous
    // container would be exactly the stale trust this gate exists to prevent.
    for root in [
        data_root.path().to_path_buf(),
        vellum_lib::state::app_data_dir(),
    ] {
        println!(
            "confirming {} {} in {}",
            preferred.key_type,
            preferred.fingerprint,
            root.display()
        );
        ssh_trust::confirm_and_trust(&root, &target, &preferred.fingerprint)
            .expect("confirm the dev host key");
    }

    let host = state
        .remote()
        .import_discovered_host(&candidate)
        .expect("import discovered host");

    Fixture {
        state,
        host_id: host.id,
        _data_root: data_root,
    }
}

/// The artifacts bootstrap will actually install. Reported rather than
/// asserted, because which pair is in play decides what a failure means:
/// an override points at a build you just made, the staged payload does not.
fn artifact_sources() -> Vec<String> {
    ["AGENT", "BROKER"]
        .into_iter()
        .map(|component| {
            let key = format!("VELLUM_REMOTE_{component}_AMD64");
            match std::env::var(&key) {
                Ok(value) => format!("{key}={value}"),
                Err(_) => format!(
                    "{key} unset -> src-tauri/resources/remote/linux-amd64/{}",
                    if component == "AGENT" {
                        "vellum-remote-agent"
                    } else {
                        "vellum-remote-broker"
                    }
                ),
            }
        })
        .collect()
}

/// Ask the pinned Codex binary on the host to parse the deployed catalog.
/// A matching catalog hash only proves that the bytes arrived; `model/list`
/// is the acceptance boundary that rejects an entry when a newly-required
/// Codex schema field is absent.
fn native_codex_model_list() -> serde_json::Value {
    let mut child = Command::new("ssh")
        .args(["-o", "BatchMode=yes", "-T", ALIAS])
        .arg("timeout 30s \"$HOME/.local/bin/codex\" app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the pinned Codex app-server over SSH");
    let mut stdin = child.stdin.take().expect("Codex stdin");
    let stdout = child.stdout.take().expect("Codex stdout");
    let mut lines = BufReader::new(stdout).lines();
    writeln!(
        stdin,
        r#"{{"id":1,"method":"initialize","params":{{"clientInfo":{{"name":"vellum-remote-e2e","title":"Vellum Remote E2E","version":"1"}}}}}}"#
    )
    .expect("write initialize");
    stdin.flush().expect("flush initialize");
    let initialized = lines
        .by_ref()
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(&line).ok())
        .find(|message| message.get("id") == Some(&json!(1)))
        .expect("Codex returned no initialize response");
    assert!(
        initialized.get("error").is_none(),
        "Codex initialization failed: {initialized}"
    );

    writeln!(stdin, r#"{{"method":"initialized"}}"#).expect("write initialized notification");
    writeln!(
        stdin,
        r#"{{"id":2,"method":"model/list","params":{{"includeHidden":true,"limit":100}}}}"#
    )
    .expect("write model/list");
    stdin.flush().expect("flush model/list");
    let response = lines
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(&line).ok())
        .find(|message| message.get("id") == Some(&json!(2)))
        .expect("Codex returned no model/list response");
    drop(stdin);
    let _ = child.wait();
    response
}

#[test]
#[ignore = "requires the dev host: pnpm run dev:host:up"]
fn bootstrap_converges_a_pristine_dev_host() {
    for source in artifact_sources() {
        println!("artifact: {source}");
    }
    let fixture = fixture();

    let result = bootstrap::bootstrap(&fixture.state, &fixture.host_id).expect("bootstrap failed");
    println!("state={} steps={:?}", result.state, result.completed_steps);

    assert_eq!(result.os, "Linux");
    assert_eq!(result.arch, "amd64");
    assert!(result.agent_installed, "agent was not installed");
    assert!(
        result.docker_available,
        "the dev host reported no Docker; it should run its own dockerd"
    );
    assert!(
        result.systemd_user_available,
        "the dev host reported no user systemd"
    );
    assert!(
        result.linger_enabled,
        "lingering is off, so a detached session would not survive disconnect"
    );

    // Codex is installed by pinned_install, not by bootstrap, so a pristine
    // host is expected to report exactly that one gap and nothing else.
    let unexpected: Vec<&String> = result
        .blocked_reasons
        .iter()
        .filter(|reason| !reason.starts_with("nativeCodexUnavailable:"))
        .collect();
    assert!(
        unexpected.is_empty(),
        "unexpected blocked reasons: {unexpected:?} (repairs: {:?})",
        result.repair_commands
    );

    assert!(
        result
            .completed_steps
            .contains(&"credentials.boundaryReady".to_string()),
        "the boundary key was never provisioned: {:?}",
        result.completed_steps
    );
}

#[test]
#[ignore = "requires the dev host: pnpm run dev:host:up"]
fn bootstrap_is_idempotent_on_an_already_converged_host() {
    let fixture = fixture();

    let first = bootstrap::bootstrap(&fixture.state, &fixture.host_id).expect("first bootstrap");
    assert!(first.agent_installed);

    let second = bootstrap::bootstrap(&fixture.state, &fixture.host_id).expect("second bootstrap");
    assert!(second.agent_installed);
    assert!(
        !second.completed_steps.contains(&"agent.installed".into()),
        "the second run reinstalled an agent whose digest already matched: {:?}",
        second.completed_steps
    );
    let unexpected: Vec<&String> = second
        .blocked_reasons
        .iter()
        .filter(|reason| !reason.starts_with("nativeCodexUnavailable:"))
        .collect();
    assert!(unexpected.is_empty(), "second run blocked: {unexpected:?}");
}

#[test]
#[ignore = "pushes the pinned Codex (~250 MB) over SSH; run after a payload staging"]
fn pinned_codex_install_lands_the_manifest_version() {
    let status = pinned_install::release_status();
    assert!(
        status.ready,
        "no verified remote payload in this build ({}): stage one with \
         `pnpm run remote:stage` and rebuild. trust={}",
        status.detail, status.trust
    );
    let manifest = pinned_install::load_verified_manifest().expect("verified manifest");
    println!(
        "payload {} pins Codex {}",
        status.release_version.as_deref().unwrap_or("?"),
        manifest.codex.pinned_version
    );

    let fixture = fixture();
    bootstrap::bootstrap(&fixture.state, &fixture.host_id).expect("bootstrap before codex install");

    let installed = pinned_install::install_pinned_codex(
        &fixture.state,
        &fixture.host_id,
        "remote-dev-host-e2e-codex",
    )
    .expect("pinned codex install failed");
    println!("install result: {installed}");

    let aggregate = RemoteHostManager::aggregate_status(&fixture.state, &fixture.host_id)
        .expect("aggregate status");
    let reported = serde_json::to_value(&aggregate).expect("serialize aggregate status");
    let version = reported
        .pointer("/agent/nativeCodex/codexVersion")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    assert!(
        version.contains(&manifest.codex.pinned_version),
        "host reports Codex {version:?}, manifest pins {}",
        manifest.codex.pinned_version
    );

    // The control-plane half of docs/remote-acceptance.md §2. A Codex that is
    // merely installed proves little: what makes a remote session survive the
    // Desktop going away is that the native daemon owns it, is durable, and is
    // restart-safe. The data-plane half needs configured providers and stays
    // in the live suites.
    for (pointer, expected) in [
        ("/agent/nativeCodex/daemonOwner", json!("codexCliDaemon")),
        ("/agent/nativeCodex/daemonRunning", json!(true)),
        ("/agent/nativeCodex/durable", json!(true)),
        ("/agent/nativeCodex/restartSafe", json!(true)),
        ("/agent/nativeCodex/remoteControlEnabled", json!(true)),
        (
            "/agent/nativeCodex/sessionAuthority",
            json!("codexNativeDaemon"),
        ),
    ] {
        assert_eq!(
            reported.pointer(pointer),
            Some(&expected),
            "{pointer} is {:?}, expected {expected}",
            reported.pointer(pointer)
        );
    }

    // §10.3: `codex.verifyInstallation` is what the Remote Manager screen
    // reads to say whether the installed Codex is the pinned one, and it has
    // to agree with the manifest rather than merely report *something*.
    // Checked against the manifest's own `compatibleRange` because that is
    // the range the product passes; a version outside it must come back
    // `compatible=false`, not be quietly accepted.
    let client = vellum_lib::remote::RemoteAgentClient::new(
        RemoteHostManager::resolve_target(&fixture.state, &fixture.host_id)
            .expect("resolve the dev host"),
    );
    let verified = client
        .codex_verify_installation(Some(&manifest.codex.compatible_range))
        .expect("verifyInstallation");
    println!("verifyInstallation: {verified}");
    assert_eq!(verified.get("present"), Some(&json!(true)));
    assert_eq!(verified.get("compatible"), Some(&json!(true)));
    assert!(
        verified
            .get("version")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|version| version.contains(&manifest.codex.pinned_version)),
        "verifyInstallation reports {:?}, manifest pins {}",
        verified.get("version"),
        manifest.codex.pinned_version
    );
    assert_eq!(
        verified.get("sha256").and_then(serde_json::Value::as_str),
        installed
            .pointer("/sha256")
            .and_then(serde_json::Value::as_str),
        "verifyInstallation disagrees with the install about which bytes are on disk"
    );

    // §10.3: `services.reconcile` rewrites a drifted unit and enables what is
    // not enabled -- and running it again must be a no-op. An unconditional
    // rewrite would restart the daemon on a host that needed nothing, which
    // is the same session-dropping failure §9 forbids elsewhere.
    let reconciled = client
        .services_reconcile("remote-dev-host-e2e-reconcile")
        .expect("services.reconcile");
    println!("reconcile: {reconciled}");
    assert_eq!(
        reconciled.get("changed"),
        Some(&json!(false)),
        "reconcile changed a unit the install had just written correctly"
    );
    assert_eq!(reconciled.get("desired"), Some(&json!(true)));
}

/// One deployable route on a host with no provider accounts.
///
/// Deployment is a configuration operation, not a request: `apply` writes
/// config, pushes credential material, starts the proxy and adopts the native
/// daemon, and never sends a token to `base_url`. So an unroutable URL is the
/// honest choice here -- it deploys exactly like a real one and cannot spend
/// anyone's quota if some future step starts making requests.
///
/// The seeded providers are disabled rather than worked around. Official would
/// block the plan on `desktopOfficialAccountMissing`, and Grok's apply path
/// reaches for a real signed-in account until the host has qualified a
/// detached one. Neither exists on a disposable container, and neither is what
/// this case is about.
fn configure_one_deployable_route(state: &AppState) {
    for route in state.routes() {
        assert!(
            state.set_route_enabled(&route.id, false),
            "could not disable the seeded route {}",
            route.id
        );
    }
    state.create_route(
        vellum_lib::model::CreateRouteInput {
            name: "dev-host-lane".into(),
            base_url: "https://dev-host-lane.invalid/v1".into(),
            model: "dev-host-model".into(),
            wire: vellum_lib::model::WireFormat::Chat,
            streaming: true,
            reasoning: false,
            server_side_resume: false,
            provider_kind: Some(vellum_lib::model::ProviderKind::OpenAiCompatible),
            api_key: None,
            models: Some(vec!["dev-host-model".into()]),
            selected_models: Some(vec!["dev-host-model".into()]),
            context_window: Some(131_072),
            model_capabilities: vec![vellum_lib::model::ModelCapability {
                model: "dev-host-model".into(),
                tool_calling: Some(true),
                probe_version: Some(vellum_lib::probe::HARNESS_PROBE_VERSION),
                ..Default::default()
            }],
            catalog_scope: None,
        },
        vellum_lib::model::ProviderKind::OpenAiCompatible,
        vellum_lib::model::AuthKind::Bearer,
    );
    let route_id = state
        .routes()
        .into_iter()
        .find(|route| route.name == "dev-host-lane")
        .expect("the deployable route was created")
        .id;
    vellum_lib::credentials::save(&state.data_root(), &route_id, "dev-host-lane-secret")
        .expect("store the route credential");
}

/// Deploy, then ask whether deploying again would change anything.
///
/// `docs/remote-acceptance.md` sections 9 and 10.4 are stated as properties of
/// a second apply -- `configChanged=false`, `catalogChanged=false`,
/// `restartRequired=false`, `observedRevision` caught up, and a drift-free
/// native daemon left alone -- and until now every one of them was checked by
/// a person reading a screen. All of them are machine-checkable against a host
/// that costs ten seconds to make.
///
/// It is also where the desktop/remote parity claim is checked end to end.
/// `deployment_desktop_parity` proves Desktop and the plan agree in process;
/// the second plan's `configChanged=false` proves the *host* persisted,
/// canonicalised and hashed the very configuration Desktop computed -- the
/// same comparison, but with a real agent, a real file and a real container
/// in the middle.
///
/// Runs after `pinned_codex_install_lands_the_manifest_version` (libtest sorts
/// by name, and `remote_` follows `pinned_`), because adopting the native
/// daemon needs a Codex to adopt.
#[tokio::test]
#[ignore = "needs the dev host and a staged payload: pnpm run remote:e2e:full"]
async fn remote_deployment_converges_and_a_second_apply_changes_nothing() {
    let status = pinned_install::release_status();
    assert!(
        status.ready,
        "no verified remote payload in this build ({}): stage one with \
         `pnpm run remote:stage` and rebuild. trust={}",
        status.detail, status.trust
    );

    let fixture = fixture();
    configure_one_deployable_route(&fixture.state);
    bootstrap::bootstrap(&fixture.state, &fixture.host_id).expect("bootstrap before deploying");

    let selection = || RemoteModelSelection {
        catalog_ids: vec![],
        policy: RemotePolicyOverrides::default(),
    };
    let first = deployment::plan(&fixture.state, &fixture.host_id, selection())
        .expect("planning must succeed");
    assert!(
        first.blocked_reasons.is_empty(),
        "the first plan is blocked: {:?}",
        first.blocked_reasons
    );
    assert!(
        first.drift.config_changed,
        "a host that has never been configured must report config drift"
    );
    println!(
        "plan {} rev {} -> {} selecting {:?}",
        first.plan_id, first.observed_revision, first.desired_revision, first.selected_catalog_ids
    );

    let applied = deployment::apply(&fixture.state, &fixture.host_id, &first.plan_id)
        .await
        .expect("apply failed");
    println!("apply steps: {:?}", applied.completed_steps);
    for step in [
        "credentials.boundaryReady",
        "credentials.ready",
        "proxy.configured",
        "proxy.ready",
        "nativeAdopt.applied",
        "nativeDaemon.restarted",
    ] {
        assert!(
            applied.completed_steps.iter().any(|done| done == step),
            "apply never reached `{step}`: {:?}",
            applied.completed_steps
        );
    }
    assert_eq!(applied.state, "nativeActive");

    // The regression seen on a GPU dev host: Vellum wrote and hashed the full
    // catalog, but one third-party entry lacked a field required by the
    // installed Codex schema. Codex rejected the whole file and silently
    // fell back to its built-in model list. Exercise the real pinned binary,
    // not a duplicate schema validator in Vellum.
    let model_list = native_codex_model_list();
    assert!(
        model_list.get("error").is_none(),
        "pinned Codex rejected the deployed catalog: {model_list}"
    );
    let listed = model_list
        .pointer("/result/data")
        .and_then(serde_json::Value::as_array)
        .expect("model/list result.data");
    for selected in &first.selected_catalog_ids {
        assert!(
            listed.iter().any(|model| {
                model.get("id").and_then(serde_json::Value::as_str) == Some(selected.as_str())
                    || model.get("model").and_then(serde_json::Value::as_str)
                        == Some(selected.as_str())
            }),
            "Codex fell back instead of loading selected model {selected:?}: {model_list}"
        );
    }

    // Section 10.4: the recorded observation catches up to what was asked for.
    let recorded = desired_state::load(&fixture.state.data_root(), &fixture.host_id)
        .expect("read the desired state");
    assert_eq!(
        recorded.observed_revision, first.desired_revision,
        "observedRevision did not catch up to the revision that was applied"
    );

    // Section 9, plus the parity claim: re-planning against the host's own
    // reported config and catalog hashes must find nothing to do.
    // `config_changed` compares the host's persisted, agent-canonicalised
    // hash to the one Desktop computed, so this failing means the two ends
    // encode the same configuration differently -- not merely that something
    // still needs applying.
    let second = deployment::plan(&fixture.state, &fixture.host_id, selection())
        .expect("re-planning must succeed");
    assert!(
        second.blocked_reasons.is_empty(),
        "the second plan is blocked: {:?}",
        second.blocked_reasons
    );
    assert_eq!(
        second.drift.remote_config_hash.as_deref(),
        Some(second.drift.desired_config_hash.as_str()),
        "the host persisted a different configuration than Desktop planned"
    );
    assert!(
        !second.drift.config_changed,
        "config still reads as changed after a successful apply: {:?}",
        second.drift
    );
    assert!(
        !second.drift.catalog_changed,
        "catalog still reads as changed after a successful apply: {:?}",
        second.drift
    );
    assert!(
        !second.restart_required,
        "a converged host still asks for a restart"
    );
    // 10.4: `Reapply desired state` re-plans from the persisted selection, and
    // an unchanged desired config has to come back with the same `planHash` or
    // the UI cannot tell "already applied" from "something to do".
    //
    // Deliberately compared against the *second* plan rather than the first.
    // `planHash` covers the config hash resolved against the host's own
    // identity -- install id and image version -- and on the first plan there
    // was no install to resolve them from, so it necessarily differs. That is
    // the fingerprint working, not drifting.
    let reapplied = deployment::reapply_desired_state(&fixture.state, &fixture.host_id)
        .expect("reapply must re-plan");
    assert_eq!(
        reapplied.plan_hash, second.plan_hash,
        "a reapply of the persisted selection is not recognisable as the same \
         desired config"
    );
    assert_eq!(
        reapplied.selected_catalog_ids, second.selected_catalog_ids,
        "the reapply planned a different model set than was applied"
    );
    assert!(
        !reapplied.restart_required,
        "a reapply of an already-applied desired state asks for a restart"
    );

    // Section 9: applying an unchanged plan must leave the running proxy and
    // the native daemon alone. A restart here is not merely wasteful -- it
    // drops whatever session the host was carrying.
    let again = deployment::apply(&fixture.state, &fixture.host_id, &second.plan_id)
        .await
        .expect("the second apply failed");
    println!("second apply steps: {:?}", again.completed_steps);
    for step in ["proxy.alreadyReady", "nativeAdopt.alreadyActive"] {
        assert!(
            again.completed_steps.iter().any(|done| done == step),
            "the second apply did work it should have skipped -- expected \
             `{step}` in {:?}",
            again.completed_steps
        );
    }
    for step in [
        "proxy.stoppedForConfiguration",
        "proxy.configured",
        "nativeDaemon.restarted",
    ] {
        assert!(
            !again.completed_steps.iter().any(|done| done == step),
            "the second apply performed `{step}` on a host with no drift: {:?}",
            again.completed_steps
        );
    }

    // The control plane is still what section 2 requires afterwards, so the
    // no-op path is proven not to have quietly torn something down.
    let aggregate = RemoteHostManager::aggregate_status(&fixture.state, &fixture.host_id)
        .expect("aggregate status");
    let reported = serde_json::to_value(&aggregate).expect("serialize aggregate status");
    for (pointer, expected) in [
        ("/agent/proxy/running", json!(true)),
        ("/agent/proxy/ready", json!(true)),
        ("/agent/nativeCodex/daemonOwner", json!("codexCliDaemon")),
        ("/agent/nativeCodex/durable", json!(true)),
        ("/agent/nativeCodex/restartSafe", json!(true)),
    ] {
        assert_eq!(
            reported.pointer(pointer),
            Some(&expected),
            "{pointer} is {:?}, expected {expected}",
            reported.pointer(pointer)
        );
    }
}

/// docs/remote-acceptance.md §11: a host whose native lease is already active
/// but whose boundary key this Desktop has never confirmed.
///
/// That is not a contrived state. It is what a host looks like to a Desktop
/// that is new, restored from backup, or reinstalled -- and to any host whose
/// lease predates boundary-key provisioning, or whose earlier deployment died
/// partway. The Agent's `resolve_boundary_key_for_daemon_lifecycle` refuses to
/// start a daemon that could never authenticate, correctly, so the whole
/// repair depends on Desktop re-provisioning a matching key *before* the first
/// daemon-lifecycle RPC.
///
/// §11 states what must not be required of the user to get out of it: no
/// deleting the lease, no rebuilding `~/.codex`, no creating a key by hand,
/// no clearing existing sessions. Retrying the operation has to be enough.
///
/// The precondition is built the way a person would arrive at it rather than
/// by editing anything on the host: a brand-new Desktop data root, pointed at
/// the host the previous case left converged and running. A fresh root has its
/// own per-host key and no sync marker, so provisioning has to push a new key
/// and restart the daemon to deliver it -- which is the exact path that used
/// to fail closed on `BoundaryNativeRestartIdentityMissing`.
///
/// Sorts after `remote_deployment_...` ("rep" > "rem"), because it needs the
/// active lease that case leaves behind.
#[test]
#[ignore = "needs the dev host and a staged payload: pnpm run remote:e2e:full"]
fn repairs_a_legacy_lease_whose_boundary_key_was_never_confirmed() {
    let fixture = fixture();
    let target = RemoteHostManager::resolve_target(&fixture.state, &fixture.host_id)
        .expect("resolve the dev host");
    let client = vellum_lib::remote::RemoteAgentClient::new(target);

    // Assert the precondition really is §11's, rather than assuming the
    // previous case left it. A lease that is not active, or a daemon that is
    // not running, would make this pass for the wrong reason.
    let before = client.host_status().expect("host status");
    let lease_before = before
        .get("managedProfiles")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .find(|profile| {
            profile
                .pointer("/profile/profileId")
                .and_then(serde_json::Value::as_str)
                == Some("codex-app-native")
        })
        .cloned()
        .expect("the previous case must leave a managed native profile");
    assert_eq!(
        lease_before.pointer("/lease/state"),
        Some(&json!("active")),
        "§11 is about an *active* lease; this host has {:?}",
        lease_before.pointer("/lease/state")
    );
    assert_eq!(
        before.pointer("/nativeCodex/daemonRunning"),
        Some(&json!(true)),
        "the daemon must already be running, or nothing needs restarting to \
         pick up a new key and the repair path is never exercised"
    );
    let lease_id = lease_before
        .pointer("/lease/leaseId")
        .and_then(serde_json::Value::as_str)
        .expect("lease id")
        .to_string();
    let codex_home = before
        .pointer("/nativeCodex/codexHome")
        .and_then(serde_json::Value::as_str)
        .expect("codex home")
        .to_string();

    // This Desktop has never confirmed a key for this host: that is what makes
    // the state a repair rather than a no-op.
    let key =
        vellum_lib::proxy::ensure_remote_boundary_key(&fixture.state.data_root(), &fixture.host_id)
            .expect("this Desktop's per-host key");
    for consumer in [
        vellum_lib::proxy::BoundaryKeyConsumer::Proxy,
        vellum_lib::proxy::BoundaryKeyConsumer::NativeCodex,
    ] {
        assert!(
            !vellum_lib::proxy::remote_boundary_key_confirmed_synced(
                &fixture.state.data_root(),
                &fixture.host_id,
                consumer,
                &key,
                None,
            )
            .expect("read the sync marker"),
            "{consumer:?} is already confirmed synced, so this is not the \
             unconfirmed state §11 describes"
        );
    }

    // §11.1: retrying the operation is the whole remedy. Nothing below deletes
    // a lease, writes a key, or touches `~/.codex`.
    let repaired = bootstrap::bootstrap(&fixture.state, &fixture.host_id)
        .expect("the retry must get past the daemon-lifecycle step");
    println!("repair steps: {:?}", repaired.completed_steps);
    assert!(
        repaired
            .completed_steps
            .contains(&"credentials.boundaryReady".to_string()),
        "the key was never provisioned: {:?}",
        repaired.completed_steps
    );

    // §11.2: the repaired host, and the things that must have survived it.
    let after = client.host_status().expect("host status after the repair");
    for (pointer, expected) in [
        ("/proxy/running", json!(true)),
        ("/proxy/ready", json!(true)),
        ("/nativeCodex/daemonOwner", json!("codexCliDaemon")),
        ("/nativeCodex/daemonRunning", json!(true)),
        ("/nativeCodex/durable", json!(true)),
        ("/nativeCodex/restartSafe", json!(true)),
    ] {
        assert_eq!(
            after.pointer(pointer),
            Some(&expected),
            "{pointer} is {:?} after the repair",
            after.pointer(pointer)
        );
    }
    let lease_after = after
        .get("managedProfiles")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .find(|profile| {
            profile
                .pointer("/profile/profileId")
                .and_then(serde_json::Value::as_str)
                == Some("codex-app-native")
        })
        .cloned()
        .expect("the managed profile must survive the repair");
    assert_eq!(lease_after.pointer("/lease/state"), Some(&json!("active")));
    // The lease id and CODEX_HOME are the evidence for "no deleting the lease,
    // no rebuilding ~/.codex": a repair that tore either down would show a new
    // id or a new path here, and the user's existing sessions would be gone
    // with them.
    assert_eq!(
        lease_after
            .pointer("/lease/leaseId")
            .and_then(serde_json::Value::as_str),
        Some(lease_id.as_str()),
        "the repair replaced the lease instead of repairing it"
    );
    assert_eq!(
        after
            .pointer("/nativeCodex/codexHome")
            .and_then(serde_json::Value::as_str),
        Some(codex_home.as_str()),
        "the repair moved CODEX_HOME, so it did not repair in place"
    );
    let credential = client
        .credential_status(vellum_proxy_runtime::BOUNDARY_CREDENTIAL_ID)
        .expect("credential status");
    assert_eq!(
        credential.get("present"),
        Some(&json!(true)),
        "the boundary credential is still absent after the repair: {credential}"
    );

    // §11.3: doing it again rotates nothing and restarts nothing. Desktop's
    // stored per-host key is the sole authority, so a second pass must reuse
    // it rather than mint a fresh one -- and with both consumers now confirmed
    // against the running instances, neither may be bounced.
    let stored_before = vellum_lib::credentials::load(
        &fixture.state.data_root(),
        &vellum_lib::proxy::remote_boundary_credential_id(&fixture.host_id),
    )
    .expect("read the stored key");
    let outcome = vellum_lib::remote::provision_remote_boundary_key(
        &client,
        &fixture.state.data_root(),
        &fixture.host_id,
        "remote-dev-host-e2e-reprovision",
    )
    .expect("a second provisioning pass must succeed");
    assert_eq!(
        outcome,
        vellum_lib::remote::boundary_key::BoundaryKeyProvisionOutcome::default(),
        "a converged host was restarted again: {outcome:?}"
    );
    let stored_after = vellum_lib::credentials::load(
        &fixture.state.data_root(),
        &vellum_lib::proxy::remote_boundary_credential_id(&fixture.host_id),
    )
    .expect("read the stored key");
    assert!(stored_before.is_some(), "no per-host key was stored at all");
    assert_eq!(
        stored_before, stored_after,
        "the boundary key was rotated by a pass that had nothing to repair"
    );
}

/// The stable Official selector means "use the control account already in
/// the Codex process". It deliberately has no secret file on the host. The
/// old readiness check treated every credential reference as a file-backed
/// provider key, so a valid deployment stayed red forever.
#[test]
#[ignore = "needs the configured dev host: pnpm run remote:e2e:full"]
fn credentials_accept_selected_official_control_account_without_an_override() {
    let fixture = fixture();
    let client = vellum_lib::remote::RemoteAgentClient::new(
        RemoteHostManager::resolve_target(&fixture.state, &fixture.host_id)
            .expect("resolve the dev host"),
    );
    let config = format!(
        r#"schema_version = 2
listen = "0.0.0.0:15721"
data_dir = "/var/lib/vellum/data"
history_dir = "/var/lib/vellum/history"
log_dir = "/var/log/vellum"
credentials_dir = "/run/secrets"
require_secrets = true
strict_upstream = true

[inbound_access]
credentialId = "__vellum_proxy_boundary__"

[identity]
install_id = "remote-e2e"
host_id = "{}"
image_version = "remote-e2e"
config_hash = "pending"

[[models]]
route_id = "official"
catalog_id = "gpt-6-astra"
upstream_model = "gpt-6-astra"
credential_id = "official-selected"

[execution_environment]
platform = "linux"
shell = "bash"
supportsAndAnd = true
hasUnixUtilities = true
pathStyle = "posix"
ampersandSemantics = "posix-background"
"#,
        fixture.host_id
    );
    client
        .proxy_configure("remote-dev-host-e2e-official-selector", &config)
        .expect("configure the stable Official selector");

    let status = client.host_status().expect("host status");
    assert_eq!(
        status.pointer("/configuration/credentialRefs"),
        Some(&json!(["official-selected"])),
        "the stable Official selector was not persisted: {status}"
    );
    assert_eq!(
        status.pointer("/configuration/credentialsReady"),
        Some(&json!(true)),
        "the Agent demanded a nonexistent secret file for the inherited control account: {status}"
    );
}

/// The staged payload is what every other test installs from, so a stale or
/// half-written one should fail here with a readable message rather than
/// somewhere inside an SSH transfer.
#[test]
fn the_staged_payload_matches_the_hash_compiled_into_this_binary() {
    let status = pinned_install::release_status();
    if !status.ready {
        println!(
            "no verified payload (trust={}, detail={}); \
             bootstrap tests still run, pinned Codex install does not",
            status.trust, status.detail
        );
        return;
    }
    let manifest = pinned_install::load_verified_manifest().expect("verified manifest");
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources/remote");
    for relative in [
        "linux-amd64/vellum-remote-agent",
        "linux-amd64/vellum-remote-broker",
        "linux-amd64/codex",
        "linux-amd64/proxy-image.tar",
    ] {
        assert!(
            root.join(relative).is_file(),
            "manifest {} declares {relative}, which is not staged",
            manifest.release_version
        );
    }
}
