//! Official Remote device pairing for the daemon's fixed control account A.
//!
//! Official Remote requires Desktop and phone to use the same ChatGPT
//! account/workspace. Proxy execution accounts B are managed by
//! `official_account`; this module never changes daemon identity or auth.json.

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::Digest as _;

use crate::native_codex::NativeCodexRuntimeStatus;

/// Deliberately narrow view of Codex's pairing response. Unknown fields are
/// discarded so a future CLI cannot accidentally send an account id, email,
/// bearer token, or another long-lived value across the Desktop RPC boundary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct RemoteControlPairing {
    #[serde(default, alias = "pairing_code")]
    pairing_code: Option<String>,
    #[serde(default)]
    code: Option<String>,
    #[serde(default, alias = "user_code")]
    user_code: Option<String>,
    #[serde(default, alias = "expires_at")]
    expires_at: Option<String>,
    #[serde(default, alias = "verification_url")]
    verification_url: Option<String>,
    #[serde(default)]
    uri: Option<String>,
    #[serde(default, alias = "pair_uri")]
    pair_uri: Option<String>,
    #[serde(default, alias = "qr_uri")]
    qr_uri: Option<String>,
}

/// M35: minimum-viable runtime gate for `codex remote-control pair --json`.
/// Fails closed on any Codex build that doesn't demonstrably support it —
/// this never guesses at an unfamiliar CLI's behavior. `--help` is a
/// read-only, side-effect-free probe (unlike actually running `pair`), so
/// it is safe to run this on every pairing attempt rather than caching a
/// version-range assumption that could go stale.
fn remote_control_pair_json_supported(binary: &Path) -> bool {
    let mut command = Command::new(binary);
    command
        .args(["remote-control", "pair", "--help"])
        .stdin(Stdio::null());
    let output = run_with_timeout(command, Duration::from_secs(5));
    let Ok(output) = output else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let help_text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    help_text.to_lowercase().contains("--json")
}

/// `codex.remoteControlPairStart`: pairs a *device* (the phone) to
/// remote-control this daemon. Independent of `native_account`'s
/// account-discovery flow — a device can be paired without ever touching
/// which ChatGPT account is active, and vice versa.
pub fn remote_control_pair_start(
    native: &NativeCodexRuntimeStatus,
    expected_control_account_hash: &str,
) -> Result<serde_json::Value, String> {
    if expected_control_account_hash.len() != 64
        || !expected_control_account_hash
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("RemoteControlPairInvalidAccountHash".into());
    }
    let observed = crate::native_account::active_account_hash(Path::new(&native.codex_home))?
        .ok_or_else(|| "RemoteControlPairControlAccountUnavailable".to_string())?;
    if observed != expected_control_account_hash {
        return Err(format!(
            "RemoteControlPairAccountMismatch: expected sha256:{}, observed sha256:{}",
            &expected_control_account_hash[..12],
            &observed[..12]
        ));
    }
    let binary = native.codex_binary.as_deref().ok_or_else(|| {
        "RemoteControlPairUnsupported: no Codex binary discovered on this host".to_string()
    })?;
    let binary = Path::new(binary);
    if !remote_control_pair_json_supported(binary) {
        return Err(
            "RemoteControlPairUnsupported: this Codex runtime does not support \
             `remote-control pair --json`; sync the Desktop Codex runtime to this host first"
                .into(),
        );
    }
    let mut command = Command::new(binary);
    command
        .args(["remote-control", "pair", "--json"])
        .env("CODEX_HOME", &native.codex_home)
        .stdin(Stdio::null());
    let output = run_with_timeout(command, Duration::from_secs(15))
        .map_err(|error| format!("RemoteControlPairTimeout: {error}"))?;
    if !output.status.success() {
        // Pairing stderr can contain URLs or short-lived codes. Preserve a
        // correlation hash, not the raw body.
        let detail_hash = hex::encode(sha2::Sha256::digest(&output.stderr));
        return Err(format!(
            "RemoteControlPairFailed: `remote-control pair --json` exited {}; diagnostic sha256:{}",
            output.status,
            &detail_hash[..12]
        ));
    }
    let pairing: RemoteControlPairing = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("RemoteControlPairUnsupported: non-JSON output: {error}"))?;
    if pairing.pairing_code.is_none()
        && pairing.code.is_none()
        && pairing.user_code.is_none()
        && pairing.verification_url.is_none()
        && pairing.uri.is_none()
        && pairing.pair_uri.is_none()
        && pairing.qr_uri.is_none()
    {
        return Err(
            "RemoteControlPairUnsupported: pairing output has no recognized short-lived fields"
                .into(),
        );
    }
    serde_json::to_value(pairing)
        .map_err(|error| format!("RemoteControlPairUnsupported: encode pairing output: {error}"))
}

fn run_with_timeout(mut command: Command, timeout: Duration) -> Result<Output, String> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot start Codex helper: {error}"))?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                return child
                    .wait_with_output()
                    .map_err(|error| format!("cannot collect Codex helper output: {error}"));
            }
            Ok(None) if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("Codex helper exceeded {}s", timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(error) => return Err(format!("cannot wait for Codex helper: {error}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_json_gate_fails_closed_on_a_binary_that_does_not_exist() {
        assert!(!remote_control_pair_json_supported(Path::new(
            "/definitely/not/a/real/codex/binary"
        )));
    }

    #[test]
    fn pairing_shape_drops_identity_and_secret_fields() {
        let pairing: RemoteControlPairing = serde_json::from_value(serde_json::json!({
            "pairing_code": "PAIR-1234",
            "verification_url": "https://example.test/pair",
            "accountId": "acct-raw",
            "email": "person@example.test",
            "accessToken": "access-secret"
        }))
        .unwrap();
        let encoded = serde_json::to_string(&pairing).unwrap();
        assert!(encoded.contains("PAIR-1234"));
        assert!(encoded.contains("https://example.test/pair"));
        assert!(!encoded.contains("acct-raw"));
        assert!(!encoded.contains("person@example.test"));
        assert!(!encoded.contains("access-secret"));
    }
}
