//! Shared boundary-key provisioning for every Desktop-triggered Codex
//! daemon-lifecycle operation on a remote host.
//!
//! A legacy install can end up with an active native lease — `config.toml`
//! already carries `env_http_headers` pointing Codex at the remote proxy —
//! whose Agent secret store never received the boundary key, or lost it
//! (partial deployment, host reimage, Agent state reset). The Agent's own
//! `resolve_boundary_key_for_daemon_lifecycle` correctly fails closed in
//! that state rather than starting a daemon that can never authenticate,
//! which is exactly right — but with nothing on the Desktop side ever
//! re-provisioning the key, that host is stuck: every retry hits the same
//! fail-closed check. This module is the fix: every Desktop entry point
//! that can start, install, or restart the remote daemon calls
//! [`provision_remote_boundary_key`] first.
//!
//! Overwriting the credential file is not, by itself, enough. Both
//! consumers of that file only ever read it once, at their own
//! process/container start — never live:
//!
//! - The remote Proxy's `InboundAccessPolicy::Authenticated` holds a `key:
//!   BoundaryKey` field captured once at construction; `check()` compares
//!   directly against that field, never against the credential store.
//! - Native Codex gets the key as an environment variable captured once at
//!   daemon spawn — and critically, `codex.bootstrapNative` silently no-ops
//!   (never re-passes the key) whenever it finds a daemon already running,
//!   it just returns the current status.
//!
//! So a plain overwrite can split an already-running pair: whichever one
//! gets restarted next picks up the new value, the other keeps presenting
//! whatever it started with. [`provision_remote_boundary_key`] closes that
//! gap directly: it decides, for *both* consumers, whether a restart is
//! needed **before** acting on either one, and if a native-Codex turn is
//! active it blocks *both* restarts rather than racing one of them ahead of
//! that check — a proxy bounce would sever the very upstream connection the
//! turn is using just as surely as restarting the daemon itself would.

use crate::error::{AppError, AppResult};
use crate::proxy::BoundaryKeyConsumer;
use crate::remote::agent_client::RemoteAgentClient;
use crate::remote::restore::active_turn_present;
use serde_json::Value;
use std::path::Path;

/// Which of the two boundary-key consumers this call actually had to
/// restart. Callers that are *also* about to issue their own explicit
/// restart (`commands::restart_remote_native_codex`, in particular) use
/// this to skip a second, redundant one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BoundaryKeyProvisionOutcome {
    pub proxy_restarted: bool,
    pub native_codex_restarted: bool,
}

/// The native daemon's process identity, read from either shape the Agent
/// reports it in: `host.status` nests it under `nativeCodex`, while
/// `codex.discoverNative` and `codex.restartNative` return that same object
/// bare.
///
/// One function for both, because two of them is how this went wrong. The
/// nested-only reader was being handed a `discoverNative` result, where it
/// found nothing every time -- so `restart_native_and_confirm` reported
/// `BoundaryNativeRestartIdentityMissing` on any host whose daemon was
/// already running, and boundary provisioning failed closed there. The unit
/// tests only ever fed it the nested shape, so they passed throughout.
pub(crate) fn native_identity(status: &Value) -> Option<String> {
    let native = status.get("nativeCodex").unwrap_or(status);
    native
        .get("daemonIdentity")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            native
                .get("daemonPid")
                .and_then(Value::as_u64)
                .map(|pid| pid.to_string())
        })
}

fn proxy_identity(status: &Value) -> Option<String> {
    status
        .pointer("/proxy/containerId")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn is_pre_boundary_config_status_error(error: &AppError) -> bool {
    let message = error.to_string();
    message.contains("invalid persisted proxy config")
        && (message.contains("inbound_access") || message.contains("schema_version 1"))
}

/// Older Agents made `host.status` fail as a whole when the persisted proxy
/// config predated the mandatory boundary guard. That is exactly the state a
/// boundary-key rollout must be able to repair. Fall back only for that known
/// parse failure, using the independent lifecycle RPCs that do not load the
/// proxy config; every other status failure remains fatal.
fn boundary_consumer_status(client: &RemoteAgentClient) -> AppResult<Value> {
    match client.host_status() {
        Ok(status) => Ok(status),
        Err(error) if is_pre_boundary_config_status_error(&error) => {
            let proxy = client.proxy_status()?;
            let native_codex = client.codex_discover_native()?;
            Ok(serde_json::json!({
                "proxy": proxy,
                "nativeCodex": native_codex,
                "configuration": {
                    "present": true,
                    "valid": false,
                    "credentialsReady": false,
                    "loadError": error.to_string(),
                },
            }))
        }
        Err(error) => Err(error),
    }
}

fn lifecycle_consumer_identity(
    status: &Value,
    consumer: BoundaryKeyConsumer,
    require_running: bool,
) -> AppResult<Option<String>> {
    let (running, identity) = match consumer {
        BoundaryKeyConsumer::Proxy => (
            status
                .pointer("/proxy/running")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            proxy_identity(status),
        ),
        BoundaryKeyConsumer::NativeCodex => (
            status
                .pointer("/nativeCodex/daemonRunning")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            native_identity(status),
        ),
    };
    if !running {
        if require_running {
            return Err(AppError::Message(match consumer {
                BoundaryKeyConsumer::Proxy => {
                    "BoundaryProxyConfirmationNotRunning: proxy is not running after lifecycle success"
                        .into()
                }
                BoundaryKeyConsumer::NativeCodex => {
                    "BoundaryNativeConfirmationNotRunning: daemon is not running after lifecycle success"
                        .into()
                }
            }));
        }
        return Ok(None);
    }
    identity.map(Some).ok_or_else(|| {
        AppError::Message(match consumer {
            BoundaryKeyConsumer::Proxy => {
                "BoundaryProxyConfirmationIdentityMissing: proxy container identity unavailable"
                    .into()
            }
            BoundaryKeyConsumer::NativeCodex => {
                "BoundaryNativeConfirmationIdentityMissing: daemon identity unavailable".into()
            }
        })
    })
}

/// A restart is needed for exactly one reason: this exact key has not been
/// confirmed delivered to the instance currently running. Nothing running
/// has nothing to have drifted; whatever starts next reads the file we just
/// wrote.
fn restart_needed(already_confirmed_synced: bool, running: bool) -> bool {
    !already_confirmed_synced && running
}

fn mark_without_restart(
    already_confirmed_synced: bool,
    running: bool,
    restart_native: bool,
) -> bool {
    !already_confirmed_synced && (!running || restart_native)
}

fn key_fingerprint(key: &vellum_proxy_runtime::BoundaryKey) -> String {
    use sha2::{Digest, Sha256};
    // The operation journal must distinguish a restart requested after a key
    // rotation from one requested for the previous key. The digest is safe to
    // put in an operation id; the secret itself never leaves the credential
    // payload.
    hex::encode(Sha256::digest(key.expose_for_storage().as_bytes()))[..16].to_owned()
}

fn operation_fingerprint(operation_id: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(operation_id.as_bytes()))[..16].to_owned()
}

fn credential_operation_id(operation_id: &str) -> String {
    format!(
        "boundary-credential-op-{}-{}",
        operation_fingerprint(operation_id),
        ulid::Ulid::new()
    )
}

/// Boundary provisioning may discover a daemon-ownership blocker while
/// reconciling the native consumer. That is not a key failure and retrying the
/// same key write cannot fix it, so preserve the actionable ownership code at
/// the top level. Actual key/status/restart failures keep the stable boundary
/// provisioning prefix used by the UI and support bundles.
fn classify_provision_error(error: AppError) -> AppError {
    let message = error.to_string();
    if message.contains("nativeDaemonAppOwned") {
        AppError::Message(message)
    } else {
        AppError::Message(format!("RemoteBoundaryKeyProvisionFailed: {message}"))
    }
}

pub(crate) fn restart_operation_id(
    operation_id: &str,
    consumer: BoundaryKeyConsumer,
    key: &vellum_proxy_runtime::BoundaryKey,
) -> String {
    let tag = match consumer {
        BoundaryKeyConsumer::Proxy => "proxy",
        BoundaryKeyConsumer::NativeCodex => "native",
    };
    // Hashing the caller id keeps this derived id under the Agent's 128-byte
    // path-safe limit even when a UI supplies a long operation id, while the
    // key digest makes a post-rotation retry a new journal entry instead of a
    // replay of the old-key restart.
    format!(
        "boundary-sync-{tag}-op-{}-key-{}",
        operation_fingerprint(operation_id),
        key_fingerprint(key)
    )
}

fn proxy_result_identity(result: &Value) -> Option<String> {
    result
        .pointer("/detail/containerId")
        .or_else(|| result.get("containerId"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Whether two Docker container ids name the same container.
///
/// Not string equality, because the Agent reports the id at two lengths and
/// which one arrives depends on the command behind it: `proxy.restart` carries
/// the full 64-character id `docker run` prints, `host.status` the
/// 12-character short id from `docker ps`. Comparing those directly could
/// never match on any host, so a boundary-key sync against a running proxy
/// bounced it, compared, bounced it a second time, compared again, and failed
/// the whole operation with `BoundaryProxyRestartIdentityMismatch`.
///
/// One id being a prefix of the other is exactly what a short id is, and is
/// how Docker itself resolves a container reference. Empty is not an identity
/// and matches nothing, so a missing field can never read as agreement.
fn same_container(left: &str, right: &str) -> bool {
    !left.is_empty() && !right.is_empty() && (left.starts_with(right) || right.starts_with(left))
}

fn restart_proxy_and_confirm(
    client: &RemoteAgentClient,
    operation_id: &str,
    key: &vellum_proxy_runtime::BoundaryKey,
) -> AppResult<String> {
    let restart_id = restart_operation_id(operation_id, BoundaryKeyConsumer::Proxy, key);
    let result = client.proxy_restart(&restart_id)?;
    let observed = boundary_consumer_status(client)?;
    let observed_identity = proxy_identity(&observed).ok_or_else(|| {
        AppError::Message(
            "BoundaryProxyRestartIdentityMissing: proxy container identity unavailable".into(),
        )
    })?;
    if proxy_result_identity(&result)
        .is_some_and(|restarted| same_container(&restarted, &observed_identity))
    {
        return Ok(observed_identity);
    }
    let retry = client.proxy_restart(&format!("{restart_id}-retry-{}", ulid::Ulid::new()))?;
    let observed = boundary_consumer_status(client)?;
    let observed_identity = proxy_identity(&observed).ok_or_else(|| {
        AppError::Message(
            "BoundaryProxyRestartIdentityMissing: proxy container identity unavailable".into(),
        )
    })?;
    if !proxy_result_identity(&retry)
        .is_some_and(|restarted| same_container(&restarted, &observed_identity))
    {
        return Err(AppError::Message(
            "BoundaryProxyRestartIdentityMismatch: restart result does not describe the running proxy".into(),
        ));
    }
    Ok(observed_identity)
}

fn restart_native_and_confirm(
    client: &RemoteAgentClient,
    operation_id: &str,
    key: &vellum_proxy_runtime::BoundaryKey,
) -> AppResult<String> {
    let restart_id = restart_operation_id(operation_id, BoundaryKeyConsumer::NativeCodex, key);
    let result = client.codex_restart_native(&restart_id)?;
    let observed = client.codex_discover_native()?;
    let observed_identity = native_identity(&observed).ok_or_else(|| {
        AppError::Message(
            "BoundaryNativeRestartIdentityMissing: daemon identity unavailable".into(),
        )
    })?;
    if native_identity(&result).as_deref() == Some(observed_identity.as_str()) {
        return Ok(observed_identity);
    }
    let retry =
        client.codex_restart_native(&format!("{restart_id}-retry-{}", ulid::Ulid::new()))?;
    let observed = client.codex_discover_native()?;
    let observed_identity = native_identity(&observed).ok_or_else(|| {
        AppError::Message(
            "BoundaryNativeRestartIdentityMissing: daemon identity unavailable".into(),
        )
    })?;
    if native_identity(&retry).as_deref() != Some(observed_identity.as_str()) {
        return Err(AppError::Message(
            "BoundaryNativeRestartIdentityMismatch: restart result does not describe the running daemon".into(),
        ));
    }
    Ok(observed_identity)
}

/// Provision (or re-provision) this host's boundary key on the remote
/// Agent's secret store, unconditionally overwriting whatever is already
/// there, then reconciles the Proxy and native Codex with it if either is
/// already running and not yet confirmed to have this exact value.
///
/// Desktop's own encrypted per-host key
/// (`crate::proxy::ensure_remote_boundary_key`) is the sole authority — the
/// Agent only ever atomically stores, mounts, and reads it. This always
/// re-pushes it via `credential.put` rather than first checking
/// `credential.status`: status can only prove *a* file exists at the
/// reserved path, never that its bytes match what Desktop has, so trusting
/// it would leave exactly the partial-deployment case this function exists
/// to fix unrepaired. Idempotent and safe on every call — a healthy host
/// where the key already matches, on the same already-verified instance,
/// just gets the same bytes written again and no follow-up restart.
///
/// `credential.put`'s operation id carries a fresh nonce, never the
/// caller's `operation_id` verbatim: the Agent's own operation journal
/// replays a prior *successful* result for a repeated `(operation_id,
/// method, request fingerprint)` triple instead of re-executing it — and
/// some callers (deployment apply's `native-{plan_id}`, in particular)
/// reuse the same base operation id across retries of the same plan.
/// Reusing it here too would mean a retry that's supposed to repair a lost
/// or partial write could just replay an old "success" without ever
/// touching the file again. This is safe specifically *because*
/// `credential.put` is cheap and side-effect-free to repeat.
///
/// The coordinated restarts below are the opposite: repeating one has a
/// real cost (a restart interrupts whatever that consumer was doing), so
/// their operation ids stay derived from the caller's `operation_id`
/// (never a fresh nonce) — matching every other daemon-lifecycle call in
/// this codebase, so a retry of the *same* caller-level operation replays
/// or resumes through the Agent's journal instead of double-restarting.
///
/// Must run before the first daemon-lifecycle RPC on every path that can
/// reach one: Bootstrap, Install (pinned/dynamic sync), Repair, manual
/// Restart, and Deployment Apply.
///
/// Never surfaces the key itself. On failure, the original reason is
/// preserved inside a message carrying the stable `RemoteBoundaryKeyProvisionFailed`
/// prefix the UI matches on to show a dedicated message with retry and
/// diagnostic-bundle actions — the key value itself is never interpolated
/// into that message, an operation result, a log line, or argv (see
/// `RemoteAgentClient::rpc`, which pipes the request over stdin rather than
/// ever formatting it into an error string).
pub fn provision_remote_boundary_key(
    client: &RemoteAgentClient,
    data_root: &Path,
    host_id: &str,
    operation_id: &str,
) -> AppResult<BoundaryKeyProvisionOutcome> {
    provision_remote_boundary_boundary_key_with_options(
        client,
        data_root,
        host_id,
        operation_id,
        true,
        false,
    )
}

/// Variant for an installation flow that will restart native Codex itself
/// after replacing the binary. Proxy convergence is still performed here,
/// but native convergence is deferred to that caller's final restart.
pub fn provision_remote_boundary_key_without_native_restart(
    client: &RemoteAgentClient,
    data_root: &Path,
    host_id: &str,
    operation_id: &str,
) -> AppResult<BoundaryKeyProvisionOutcome> {
    provision_remote_boundary_boundary_key_with_options(
        client,
        data_root,
        host_id,
        operation_id,
        false,
        true,
    )
}

/// Confirm the consumers a caller has just successfully started, installed,
/// or restarted. Provisioning can only observe the pre-lifecycle state: when
/// a consumer was stopped it records the explicit no-instance marker, which
/// must be replaced with the new instance identity after the caller's RPC
/// brings that consumer up. `require_running` additionally rejects a replayed
/// lifecycle "success" when the required consumer is no longer running.
/// Without this postcondition, the next drift-free
/// apply would treat the newly started instance as unconfirmed and restart it
/// once more.
pub(crate) fn confirm_remote_boundary_key_consumers(
    client: &RemoteAgentClient,
    data_root: &Path,
    host_id: &str,
    consumers: &[BoundaryKeyConsumer],
    require_running: bool,
) -> AppResult<()> {
    (|| -> AppResult<()> {
        let key = crate::proxy::ensure_remote_boundary_key(data_root, host_id)?;
        let status = boundary_consumer_status(client)?;
        for consumer in consumers {
            let Some(identity) = lifecycle_consumer_identity(&status, *consumer, require_running)?
            else {
                continue;
            };
            crate::proxy::mark_remote_boundary_key_synced(
                data_root,
                host_id,
                *consumer,
                &key,
                Some(&identity),
            )?;
        }
        Ok(())
    })()
    .map_err(classify_provision_error)
}

fn provision_remote_boundary_boundary_key_with_options(
    client: &RemoteAgentClient,
    data_root: &Path,
    host_id: &str,
    operation_id: &str,
    restart_native: bool,
    guard_native_restart: bool,
) -> AppResult<BoundaryKeyProvisionOutcome> {
    (|| -> AppResult<BoundaryKeyProvisionOutcome> {
        let key = crate::proxy::ensure_remote_boundary_key(data_root, host_id)?;
        let status = boundary_consumer_status(client)?;
        let proxy_running = status
            .pointer("/proxy/running")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let native_running = status
            .pointer("/nativeCodex/daemonRunning")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let proxy_already_synced = crate::proxy::remote_boundary_key_confirmed_synced(
            data_root,
            host_id,
            BoundaryKeyConsumer::Proxy,
            &key,
            proxy_identity(&status).as_deref(),
        )?;
        let native_already_synced = crate::proxy::remote_boundary_key_confirmed_synced(
            data_root,
            host_id,
            BoundaryKeyConsumer::NativeCodex,
            &key,
            native_identity(&status).as_deref(),
        )?;

        let proxy_needs_restart = restart_needed(proxy_already_synced, proxy_running);
        let native_needs_restart = restart_native && restart_needed(native_already_synced, native_running);

        // Decide, before touching either consumer, whether a native-Codex
        // turn is currently in flight -- and if so, block both restarts,
        // not just the daemon's own. A proxy bounce severs the very
        // upstream connection that turn is using just as surely as
        // restarting the daemon would; there is no "safer" one to allow.
        // The dynamic Desktop sync path defers the native restart until after
        // its binary install, but it still has to guard that explicit restart
        // here, before staging/installing anything that will lead to it.
        if proxy_needs_restart
            || native_needs_restart
            || (guard_native_restart && native_running)
        {
            let sessions = client.codex_session_status(None)?;
            if active_turn_present(&status, &sessions) {
                return Err(AppError::Message(
                    "NativeCodexActiveTurnInProgress: wait for the current Codex turn to finish before repairing the boundary key"
                        .into(),
                ));
            }
        }

        client.credential_put(
            &credential_operation_id(operation_id),
            vellum_proxy_runtime::BOUNDARY_CREDENTIAL_ID,
            key.expose_for_storage(),
        )?;

        let proxy_restarted = if proxy_needs_restart {
            let restarted_identity = restart_proxy_and_confirm(client, operation_id, &key)?;
            crate::proxy::mark_remote_boundary_key_synced(
                data_root,
                host_id,
                BoundaryKeyConsumer::Proxy,
                &key,
                Some(&restarted_identity),
            )?;
            true
        } else {
            if !proxy_already_synced {
                crate::proxy::mark_remote_boundary_key_synced(
                    data_root,
                    host_id,
                    BoundaryKeyConsumer::Proxy,
                    &key,
                    proxy_identity(&status).as_deref(),
                )?;
            }
            false
        };

        let native_codex_restarted = if native_needs_restart {
            let restarted_identity = restart_native_and_confirm(client, operation_id, &key)?;
            crate::proxy::mark_remote_boundary_key_synced(
                data_root,
                host_id,
                BoundaryKeyConsumer::NativeCodex,
                &key,
                Some(&restarted_identity),
            )?;
            true
        } else {
            // A deferred-restart caller (currently Desktop's dynamic sync)
            // must not claim a running native daemon is synced before its own
            // post-install restart. It will record the fresh identity after
            // that restart completes. A stopped daemon is safe to mark with
            // the explicit no-instance sentinel because its next start reads
            // the credential we just wrote.
            if mark_without_restart(native_already_synced, native_running, restart_native) {
                crate::proxy::mark_remote_boundary_key_synced(
                    data_root,
                    host_id,
                    BoundaryKeyConsumer::NativeCodex,
                    &key,
                    native_identity(&status).as_deref(),
                )?;
            }
            false
        };

        Ok(BoundaryKeyProvisionOutcome {
            proxy_restarted,
            native_codex_restarted,
        })
    })()
    .map_err(classify_provision_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_host_provisions_the_identical_key_twice() {
        let temp = tempfile::tempdir().unwrap();
        let first = crate::proxy::ensure_remote_boundary_key(temp.path(), "host-a").unwrap();
        let second = crate::proxy::ensure_remote_boundary_key(temp.path(), "host-a").unwrap();
        assert_eq!(first.expose_for_storage(), second.expose_for_storage());
    }

    #[test]
    fn different_hosts_get_different_keys() {
        let temp = tempfile::tempdir().unwrap();
        let a = crate::proxy::ensure_remote_boundary_key(temp.path(), "host-a").unwrap();
        let b = crate::proxy::ensure_remote_boundary_key(temp.path(), "host-b").unwrap();
        assert_ne!(a.expose_for_storage(), b.expose_for_storage());
    }

    /// A failure anywhere in provisioning — local key read/generation or the
    /// remote `credential.put` call — must come back tagged with the stable
    /// code the UI matches on, with the original reason still legible.
    #[test]
    fn provisioning_failure_is_tagged_with_the_stable_error_code() {
        let temp = tempfile::tempdir().unwrap();
        let target = crate::remote::agent_client::ResolvedAgentTarget {
            host_id: "host-a".into(),
            ssh_destination: None,
            agent_bin: "definitely-not-a-real-agent-binary".into(),
            state_root: None,
        };
        let client = RemoteAgentClient::new(target);
        let error =
            provision_remote_boundary_key(&client, temp.path(), "host-a", "op-1").unwrap_err();
        let message = error.to_string();
        assert!(
            message.starts_with("RemoteBoundaryKeyProvisionFailed: "),
            "unexpected message: {message}"
        );
        assert!(
            message.len() > "RemoteBoundaryKeyProvisionFailed: ".len(),
            "original reason must survive: {message}"
        );
    }

    #[test]
    fn app_owned_daemon_is_not_misreported_as_a_boundary_key_failure() {
        let error = classify_provision_error(AppError::Message(
            "nativeDaemonAppOwned: stop it explicitly before taking over".into(),
        ));
        assert!(error.to_string().starts_with("nativeDaemonAppOwned:"));
        assert!(!error
            .to_string()
            .contains("RemoteBoundaryKeyProvisionFailed"));
    }

    #[test]
    fn restart_is_skipped_once_already_confirmed_synced_regardless_of_running_state() {
        assert!(!restart_needed(true, true));
    }

    #[test]
    fn restart_is_needed_for_an_unconfirmed_key_on_a_running_instance() {
        assert!(restart_needed(false, true));
    }

    #[test]
    fn restart_is_skipped_for_an_unconfirmed_key_with_nothing_running() {
        assert!(!restart_needed(false, false));
    }

    #[test]
    fn deferred_native_restart_does_not_mark_a_running_daemon_as_synced() {
        assert!(!mark_without_restart(false, true, false));
        assert!(mark_without_restart(false, false, false));
        assert!(mark_without_restart(false, true, true));
        assert!(!mark_without_restart(true, true, false));
    }

    #[test]
    fn native_identity_reads_the_daemon_pid_as_a_string() {
        let status = serde_json::json!({"nativeCodex": {"daemonPid": 4242}});
        assert_eq!(native_identity(&status).as_deref(), Some("4242"));
    }

    #[test]
    fn native_identity_prefers_process_identity_over_pid_fallback() {
        let status = serde_json::json!({
            "nativeCodex": {"daemonPid": 4242, "daemonIdentity": "pid-4242-start-9"}
        });
        assert_eq!(
            native_identity(&status).as_deref(),
            Some("pid-4242-start-9")
        );
    }

    /// The two lengths the Agent really reports, captured from one dev-host
    /// container: `docker run` via `proxy.restart` gives the full id,
    /// `docker ps` via `host.status` gives the short one.
    #[test]
    fn a_full_container_id_and_its_short_form_are_the_same_container() {
        let full = "260841161f6d8600568c252c0198f20171c57483b0ad9eca5e94dc1234ff19eb";
        let short = "260841161f6d";
        assert!(same_container(full, short));
        assert!(same_container(short, full));
        assert!(same_container(full, full));
    }

    #[test]
    fn two_different_containers_are_not_the_same_container() {
        assert!(!same_container(
            "260841161f6d8600568c252c0198f20171c57483b0ad9eca5e94dc1234ff19eb",
            "e8fa0c8025ad"
        ));
        // A missing identity is not agreement. Prefix matching must never let
        // an empty string stand in for "whatever is running".
        assert!(!same_container("", "260841161f6d"));
        assert!(!same_container("260841161f6d", ""));
        assert!(!same_container("", ""));
    }

    /// The shape `codex.discoverNative` and `codex.restartNative` actually
    /// return -- the native-Codex object with no `nativeCodex` wrapper. Every
    /// existing case here fed the nested `host.status` shape, which is why a
    /// reader that only understood that one looked correct for as long as
    /// nobody ran `restart_native_and_confirm` against a live daemon.
    #[test]
    fn native_identity_reads_the_bare_shape_the_daemon_rpcs_return() {
        let discovered = serde_json::json!({
            "codexHome": "/home/vellum/.codex",
            "daemonRunning": true,
            "daemonPid": 4243,
            "daemonIdentity": "pid-4243-start-56098-boot-aac434b6",
            "restartSafe": true,
        });
        assert_eq!(
            native_identity(&discovered).as_deref(),
            Some("pid-4243-start-56098-boot-aac434b6")
        );
        assert_eq!(
            native_identity(&serde_json::json!({"daemonPid": 4243})).as_deref(),
            Some("4243")
        );
    }

    /// A restart result and the status read back after it must compare equal
    /// when they describe the same daemon, whichever shape each arrived in.
    /// Reading them with two different extractors is what made
    /// `restart_native_and_confirm` fail closed on every already-running
    /// daemon.
    #[test]
    fn both_daemon_response_shapes_agree_on_the_same_daemon() {
        let restart_result = serde_json::json!({"daemonIdentity": "pid-7-start-1"});
        let host_status = serde_json::json!({"nativeCodex": {"daemonIdentity": "pid-7-start-1"}});
        assert_eq!(
            native_identity(&restart_result),
            native_identity(&host_status)
        );
    }

    #[test]
    fn a_new_key_gets_a_new_restart_journal_id() {
        let temp = tempfile::tempdir().unwrap();
        let first = crate::proxy::ensure_remote_boundary_key(temp.path(), "host-a").unwrap();
        let second = crate::proxy::ensure_remote_boundary_key(temp.path(), "host-b").unwrap();
        assert_ne!(
            restart_operation_id("op-1", BoundaryKeyConsumer::Proxy, &first),
            restart_operation_id("op-1", BoundaryKeyConsumer::Proxy, &second)
        );
    }

    #[test]
    fn same_key_keeps_restart_journal_id_stable_for_replay() {
        let temp = tempfile::tempdir().unwrap();
        let key = crate::proxy::ensure_remote_boundary_key(temp.path(), "host-a").unwrap();
        assert_eq!(
            restart_operation_id("op-1", BoundaryKeyConsumer::NativeCodex, &key),
            restart_operation_id("op-1", BoundaryKeyConsumer::NativeCodex, &key)
        );
    }

    #[test]
    fn derived_restart_journal_id_stays_path_safe_for_a_long_caller_id() {
        let temp = tempfile::tempdir().unwrap();
        let key = crate::proxy::ensure_remote_boundary_key(temp.path(), "host-a").unwrap();
        let operation_id = "x".repeat(512);
        let derived = restart_operation_id(&operation_id, BoundaryKeyConsumer::Proxy, &key);
        assert!(derived.len() <= 128);
        assert!(derived
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_".contains(character)));
    }

    #[test]
    fn native_identity_is_absent_when_status_omits_the_pid() {
        assert_eq!(native_identity(&serde_json::json!({})), None);
    }

    #[test]
    fn proxy_identity_reads_the_container_id() {
        let status = serde_json::json!({"proxy": {"containerId": "c123"}});
        assert_eq!(proxy_identity(&status).as_deref(), Some("c123"));
    }

    #[test]
    fn proxy_identity_is_absent_when_status_omits_the_container_id() {
        assert_eq!(proxy_identity(&serde_json::json!({})), None);
    }

    #[test]
    fn only_the_known_pre_boundary_config_failure_enables_legacy_status_fallback() {
        for message in [
            "agent failed: invalid persisted proxy config: missing field `inbound_access`",
            "agent failed: invalid persisted proxy config: unsupported schema_version 1; expected 2",
        ] {
            assert!(is_pre_boundary_config_status_error(&AppError::Message(
                message.into()
            )));
        }

        for message in [
            "agent failed: connection refused",
            "invalid persisted proxy config: missing field `models`",
            "permission denied reading proxy config",
        ] {
            assert!(!is_pre_boundary_config_status_error(&AppError::Message(
                message.into()
            )));
        }
    }

    #[test]
    fn required_post_lifecycle_consumer_must_be_running_with_an_identity() {
        let stopped = serde_json::json!({"nativeCodex": {"daemonRunning": false}});
        assert!(
            lifecycle_consumer_identity(&stopped, BoundaryKeyConsumer::NativeCodex, true)
                .unwrap_err()
                .to_string()
                .contains("BoundaryNativeConfirmationNotRunning")
        );

        let missing_identity = serde_json::json!({"proxy": {"running": true}});
        assert!(
            lifecycle_consumer_identity(&missing_identity, BoundaryKeyConsumer::Proxy, true)
                .unwrap_err()
                .to_string()
                .contains("BoundaryProxyConfirmationIdentityMissing")
        );

        let running = serde_json::json!({
            "nativeCodex": {"daemonRunning": true, "daemonIdentity": "pid-42-start-7"}
        });
        assert_eq!(
            lifecycle_consumer_identity(&running, BoundaryKeyConsumer::NativeCodex, true)
                .unwrap()
                .as_deref(),
            Some("pid-42-start-7")
        );
    }

    #[test]
    fn optional_post_lifecycle_consumer_may_remain_stopped() {
        let stopped = serde_json::json!({"proxy": {"running": false}});
        assert_eq!(
            lifecycle_consumer_identity(&stopped, BoundaryKeyConsumer::Proxy, false).unwrap(),
            None
        );
    }

    /// The same guard `restore.rs` and `bootstrap.rs` already use before
    /// touching a running daemon: an active turn -- by host-level flag or a
    /// per-thread marker -- must block a forced restart, not just a
    /// deliberate stop.
    #[test]
    fn active_turn_blocks_a_native_restart_that_would_otherwise_be_needed() {
        let status =
            serde_json::json!({"nativeCodex": {"daemonRunning": true, "activeTurn": true}});
        let sessions = serde_json::json!({"threads": []});
        assert!(restart_needed(false, true));
        assert!(active_turn_present(&status, &sessions));
    }

    #[test]
    fn active_turn_via_a_thread_marker_also_blocks_a_native_restart() {
        let status = serde_json::json!({"nativeCodex": {"daemonRunning": true}});
        let sessions = serde_json::json!({"threads": [{"active": true}]});
        assert!(restart_needed(false, true));
        assert!(active_turn_present(&status, &sessions));
    }

    /// A key that is confirmed synced against *no observed instance* (the
    /// consumer was not running when we last checked) must not read back as
    /// confirmed once some instance -- any instance, since it was never
    /// verified -- is running.
    #[test]
    fn a_key_confirmed_against_no_running_instance_is_unconfirmed_once_something_starts() {
        let temp = tempfile::tempdir().unwrap();
        let key = crate::proxy::ensure_remote_boundary_key(temp.path(), "host-a").unwrap();
        crate::proxy::mark_remote_boundary_key_synced(
            temp.path(),
            "host-a",
            BoundaryKeyConsumer::NativeCodex,
            &key,
            None,
        )
        .unwrap();
        assert!(crate::proxy::remote_boundary_key_confirmed_synced(
            temp.path(),
            "host-a",
            BoundaryKeyConsumer::NativeCodex,
            &key,
            None,
        )
        .unwrap());
        assert!(!crate::proxy::remote_boundary_key_confirmed_synced(
            temp.path(),
            "host-a",
            BoundaryKeyConsumer::NativeCodex,
            &key,
            Some("4242"),
        )
        .unwrap());
    }

    #[test]
    fn post_lifecycle_confirmation_prevents_a_drift_free_repeat_restart() {
        let temp = tempfile::tempdir().unwrap();
        let key = crate::proxy::ensure_remote_boundary_key(temp.path(), "host-a").unwrap();
        crate::proxy::mark_remote_boundary_key_synced(
            temp.path(),
            "host-a",
            BoundaryKeyConsumer::NativeCodex,
            &key,
            None,
        )
        .unwrap();
        assert!(restart_needed(false, true));

        crate::proxy::mark_remote_boundary_key_synced(
            temp.path(),
            "host-a",
            BoundaryKeyConsumer::NativeCodex,
            &key,
            Some("pid-42-start-7"),
        )
        .unwrap();
        let confirmed = crate::proxy::remote_boundary_key_confirmed_synced(
            temp.path(),
            "host-a",
            BoundaryKeyConsumer::NativeCodex,
            &key,
            Some("pid-42-start-7"),
        )
        .unwrap();
        assert!(confirmed);
        assert!(!restart_needed(confirmed, true));
    }

    /// A key confirmed synced against one instance (pid/container id) must
    /// not read back as confirmed once a *different* instance is observed
    /// -- a supervisor-driven restart outside this function's own restart
    /// call is exactly the case a hash-only marker would miss.
    #[test]
    fn a_key_confirmed_against_one_instance_is_unconfirmed_against_a_different_instance() {
        let temp = tempfile::tempdir().unwrap();
        let key = crate::proxy::ensure_remote_boundary_key(temp.path(), "host-a").unwrap();
        crate::proxy::mark_remote_boundary_key_synced(
            temp.path(),
            "host-a",
            BoundaryKeyConsumer::NativeCodex,
            &key,
            Some("4242"),
        )
        .unwrap();
        assert!(crate::proxy::remote_boundary_key_confirmed_synced(
            temp.path(),
            "host-a",
            BoundaryKeyConsumer::NativeCodex,
            &key,
            Some("4242"),
        )
        .unwrap());
        assert!(!crate::proxy::remote_boundary_key_confirmed_synced(
            temp.path(),
            "host-a",
            BoundaryKeyConsumer::NativeCodex,
            &key,
            Some("9999"),
        )
        .unwrap());
    }

    #[test]
    fn a_freshly_generated_key_is_not_yet_confirmed_synced_for_either_consumer() {
        let temp = tempfile::tempdir().unwrap();
        let key = crate::proxy::ensure_remote_boundary_key(temp.path(), "host-a").unwrap();
        for consumer in [BoundaryKeyConsumer::Proxy, BoundaryKeyConsumer::NativeCodex] {
            assert!(!crate::proxy::remote_boundary_key_confirmed_synced(
                temp.path(),
                "host-a",
                consumer,
                &key,
                Some("4242"),
            )
            .unwrap());
        }
    }

    #[test]
    fn marking_one_consumer_synced_never_marks_the_other() {
        let temp = tempfile::tempdir().unwrap();
        let key = crate::proxy::ensure_remote_boundary_key(temp.path(), "host-a").unwrap();
        crate::proxy::mark_remote_boundary_key_synced(
            temp.path(),
            "host-a",
            BoundaryKeyConsumer::Proxy,
            &key,
            Some("c123"),
        )
        .unwrap();
        assert!(crate::proxy::remote_boundary_key_confirmed_synced(
            temp.path(),
            "host-a",
            BoundaryKeyConsumer::Proxy,
            &key,
            Some("c123"),
        )
        .unwrap());
        assert!(!crate::proxy::remote_boundary_key_confirmed_synced(
            temp.path(),
            "host-a",
            BoundaryKeyConsumer::NativeCodex,
            &key,
            Some("c123"),
        )
        .unwrap());
    }

    /// The marker is per host: confirming one host's key must never make a
    /// different host's (still-unconfirmed) key read back as synced.
    #[test]
    fn the_sync_marker_never_crosses_hosts() {
        let temp = tempfile::tempdir().unwrap();
        let key_a = crate::proxy::ensure_remote_boundary_key(temp.path(), "host-a").unwrap();
        let key_b = crate::proxy::ensure_remote_boundary_key(temp.path(), "host-b").unwrap();
        crate::proxy::mark_remote_boundary_key_synced(
            temp.path(),
            "host-a",
            BoundaryKeyConsumer::NativeCodex,
            &key_a,
            Some("4242"),
        )
        .unwrap();
        assert!(!crate::proxy::remote_boundary_key_confirmed_synced(
            temp.path(),
            "host-b",
            BoundaryKeyConsumer::NativeCodex,
            &key_b,
            Some("4242"),
        )
        .unwrap());
    }
}
