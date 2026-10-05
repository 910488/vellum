//! Where Codex Desktop's backend relay is, as the bridge needs to know it.
//!
//! Desktop sends its own backend requests to the origin the app-server's
//! `account/read` names in `workspaceRouting.backendOrigin`, keeping each
//! request's path. The bridge sits on that response, so it can name the
//! proxy's relay instead and Desktop's usage check goes through Vellum.
//!
//! The bridge is started by Desktop, not by Vellum, so the relay's address
//! reaches it through a file: Vellum writes it once the relay serves and its
//! certificate is trusted, and removes it when the relay stops. The bridge
//! also checks the port answers before it rewrites anything. Whenever any
//! of that fails, the response goes to Desktop unchanged and Desktop talks to
//! chatgpt.com as it would without Vellum; a wrong rewrite would instead send
//! every backend call Desktop makes to a dead or untrusted address.

use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::atomic::write_atomic;

const ADVERTISEMENT_FILE: &str = "enhanced-runtime/desktop-backend-relay.json";
/// The only workspace backend the relay forwards to, so the only one it may
/// stand in for (`desktop_backend::PRODUCTION_BACKEND_ORIGIN`; spelled out
/// because this module stays independent of the proxy runtime).
const PRODUCTION_BACKEND_ORIGIN: &str = "https://chatgpt.com";
const PROBE_TIMEOUT: Duration = Duration::from_millis(300);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayAdvertisement {
    /// What Desktop is told, e.g. `https://localhost`.
    pub origin: String,
    pub port: u16,
    /// The certificate the relay serves, for whoever reads this file later.
    pub certificate_sha256: String,
}

pub fn advertisement_path(data_root: &Path) -> PathBuf {
    data_root.join(ADVERTISEMENT_FILE)
}

/// The same file, found from the launch manifest's attestation path, which
/// is all the bridge has to locate Vellum's data root.
pub fn advertisement_path_beside(attestation_path: &Path) -> Option<PathBuf> {
    let file_name = Path::new(ADVERTISEMENT_FILE).file_name()?;
    Some(attestation_path.parent()?.join(file_name))
}

pub fn advertise(data_root: &Path, advertisement: &RelayAdvertisement) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(advertisement).map_err(std::io::Error::other)?;
    write_atomic(&advertisement_path(data_root), &bytes)
}

pub fn withdraw(data_root: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(advertisement_path(data_root)) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// The relay origin to give Desktop, if the relay is advertised and answers.
pub fn live_relay_origin(path: &Path) -> Option<String> {
    let advertisement: RelayAdvertisement =
        serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    // A Vellum that died without withdrawing leaves the file behind.
    TcpStream::connect_timeout(
        &SocketAddr::from((Ipv4Addr::LOCALHOST, advertisement.port)),
        PROBE_TIMEOUT,
    )
    .ok()?;
    Some(advertisement.origin)
}

/// Points an `account/read` result's workspace routing at `relay_origin`.
///
/// Only the production backend is replaced; that is the one the relay
/// forwards to. Returns whether anything changed.
pub fn route_workspace_to_relay(result: &mut Value, relay_origin: &str) -> bool {
    let Some(origin) = result
        .get_mut("workspaceRouting")
        .and_then(|routing| routing.get_mut("backendOrigin"))
    else {
        return false;
    };
    if origin.as_str() != Some(PRODUCTION_BACKEND_ORIGIN) {
        return false;
    }
    *origin = Value::String(relay_origin.to_string());
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn account_read() -> Value {
        json!({
            "account": {"type": "chatgpt", "email": "user@example.com"},
            "workspaceRouting": {
                "chatgptAccountId": "acct",
                "backendOrigin": "https://chatgpt.com",
                "accountRoutingOverride": "NO_CONSTRAINT"
            }
        })
    }

    #[test]
    fn only_the_production_origin_is_replaced() {
        let mut result = account_read();
        assert!(route_workspace_to_relay(&mut result, "https://localhost"));
        assert_eq!(
            result["workspaceRouting"]["backendOrigin"],
            "https://localhost"
        );
        assert_eq!(result["workspaceRouting"]["chatgptAccountId"], "acct");

        let mut residency = account_read();
        residency["workspaceRouting"]["backendOrigin"] = json!("https://eu.chatgpt.com");
        assert!(!route_workspace_to_relay(
            &mut residency,
            "https://localhost"
        ));
        assert_eq!(
            residency["workspaceRouting"]["backendOrigin"],
            "https://eu.chatgpt.com"
        );

        let mut legacy = json!({"account": {"type": "chatgpt"}});
        assert!(!route_workspace_to_relay(&mut legacy, "https://localhost"));
        assert_eq!(legacy, json!({"account": {"type": "chatgpt"}}));
    }

    #[test]
    fn an_advertised_relay_counts_only_while_it_answers() {
        let root = tempfile::tempdir().unwrap();
        let path = advertisement_path(root.path());
        assert_eq!(live_relay_origin(&path), None);

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let advertise_port = |port| {
            advertise(
                root.path(),
                &RelayAdvertisement {
                    origin: "https://localhost:8000".into(),
                    port,
                    certificate_sha256: "00".into(),
                },
            )
            .unwrap()
        };
        advertise_port(listener.local_addr().unwrap().port());
        assert_eq!(
            live_relay_origin(&path).as_deref(),
            Some("https://localhost:8000")
        );

        // A dead relay. Not the port just closed: tests run in parallel, and
        // another one may take it; port 0 never accepts a connection.
        drop(listener);
        advertise_port(0);
        assert_eq!(live_relay_origin(&path), None);

        withdraw(root.path()).unwrap();
        withdraw(root.path()).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn the_bridge_finds_the_file_vellum_writes() {
        let root = Path::new("data-root");
        let attestation = root.join(super::super::launch_manifest::ATTESTATION_FILE);
        assert_eq!(
            advertisement_path_beside(&attestation).unwrap(),
            advertisement_path(root)
        );
    }
}
