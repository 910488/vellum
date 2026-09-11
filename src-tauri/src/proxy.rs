//! Desktop proxy entrypoint after the M11 strangler cutover.
//!
//! Production contains only the shared-runtime service adapter. The previous
//! Desktop provider pipeline (`proxy_legacy.rs`) was deleted with the Vellum
//! Canonical surface it implemented; only the pure protocol-replay helpers in
//! `proxy_replay.rs` survive it, and those have no provider dispatch path.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::sync::oneshot;

use crate::error::{AppError, AppResult};
use crate::state::AppState;
use vellum_proxy_runtime::{BoundaryKey, InboundAccessPolicy, BOUNDARY_CREDENTIAL_ID};

#[path = "proxy_replay.rs"]
mod replay;

pub(crate) use replay::{
    replay_compaction_materialization, replay_grok_to_official_compaction,
    replay_official_canonical_compaction, replay_server_side_canonical_compaction,
    replay_websocket_shape, replay_zstd_rebuild,
};

pub async fn run_local_proxy(
    state: AppState,
    address: SocketAddr,
    shutdown: oneshot::Receiver<()>,
) -> AppResult<()> {
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .map_err(|error| AppError::Message(format!("無法監聽本機代理 {address}：{error}")))?;
    let boundary_key = ensure_boundary_key(&state.data_root())?;
    serve_local_proxy(state, listener, boundary_key, shutdown).await
}

/// Load this install's proxy boundary key, generating one on first start.
///
/// The key is deliberately stable across restarts: it has to keep matching the
/// header Vellum wrote into the user's Codex config, and rotating it silently
/// would break a running Codex session with an authentication failure it has no
/// way to explain. It is stored through the same encrypted credential store as
/// every other secret, under a reserved ID.
pub fn ensure_boundary_key(data_root: &std::path::Path) -> AppResult<BoundaryKey> {
    if let Some(stored) = crate::credentials::load(data_root, BOUNDARY_CREDENTIAL_ID)? {
        match BoundaryKey::parse(&stored) {
            Ok(key) => return Ok(key),
            Err(error) => {
                // An unusable stored key authenticates nothing, so there is no
                // session to protect by keeping it. Replacing it is the only
                // way forward, and the Codex config is rewritten on this same
                // start, so the new key reaches Codex with it.
                log::warn!("[Proxy] 已儲存的 boundary key 無法使用，將重新產生：{error}");
            }
        }
    }
    let key = BoundaryKey::generate().map_err(AppError::Message)?;
    crate::credentials::save(data_root, BOUNDARY_CREDENTIAL_ID, key.expose_for_storage())?;
    Ok(key)
}

/// Local storage ID for a remote host's boundary key.
///
/// Every remote host gets its own key -- one host's key must not admit a caller
/// to another host's proxy -- so Desktop keeps them under separate IDs. The host
/// ID is hashed rather than interpolated: the agent restricts credential IDs to
/// `[A-Za-z0-9._-]`, and a host name is not bound by that.
///
/// On the host itself the key lands under the plain [`BOUNDARY_CREDENTIAL_ID`],
/// which is what that host's `proxy.toml` names.
pub fn remote_boundary_credential_id(host_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = hex::encode(Sha256::digest(host_id.as_bytes()));
    format!("{BOUNDARY_CREDENTIAL_ID}remote-{}", &digest[..16])
}

/// The boundary key for one remote host, generated on first deployment and
/// stable afterwards for the same reason the local one is: the remote proxy
/// keeps serving sessions across restarts, and a silently rotated key would
/// lock out the Codex config already pointing at it.
pub fn ensure_remote_boundary_key(
    data_root: &std::path::Path,
    host_id: &str,
) -> AppResult<BoundaryKey> {
    let credential_id = remote_boundary_credential_id(host_id);
    if let Some(stored) = crate::credentials::load(data_root, &credential_id)? {
        match BoundaryKey::parse(&stored) {
            Ok(key) => return Ok(key),
            Err(error) => {
                log::warn!(
                    "[Proxy] 遠端主機 {host_id} 的 boundary key 無法使用，將重新產生：{error}"
                );
            }
        }
    }
    let key = BoundaryKey::generate().map_err(AppError::Message)?;
    crate::credentials::save(data_root, &credential_id, key.expose_for_storage())?;
    Ok(key)
}

/// One boundary-key consumer that only ever reads the key at its own
/// process/container start -- never live -- so a fresh write only reaches it
/// via an explicit restart of that specific consumer.
///
/// The remote Proxy's `InboundAccessPolicy::Authenticated` holds a `key:
/// BoundaryKey` field captured once at construction, checked directly in
/// `InboundAccessPolicy::check` -- not re-fetched from the credential store
/// per request. Native Codex is the same shape one level further out: the
/// key reaches it as an environment variable captured once at daemon spawn.
/// Both need their own independent "confirmed synced" marker, since either
/// can converge (or fail to) on a different schedule than the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundaryKeyConsumer {
    Proxy,
    NativeCodex,
}

impl BoundaryKeyConsumer {
    fn marker_tag(self) -> &'static str {
        match self {
            Self::Proxy => "proxy-sync",
            Self::NativeCodex => "native-sync",
        }
    }
}

/// Local marker ID for "the boundary-key hash last confirmed delivered to
/// this remote host's `consumer`" -- Desktop-only bookkeeping, never sent to
/// the Agent. Kept in the same encrypted store as the key itself but under
/// its own hashed ID (distinct per consumer, and distinct from `-remote-`)
/// so none of these can collide or be mistaken for one another.
fn remote_boundary_sync_marker_id(host_id: &str, consumer: BoundaryKeyConsumer) -> String {
    use sha2::{Digest, Sha256};
    let digest = hex::encode(Sha256::digest(host_id.as_bytes()));
    format!(
        "{BOUNDARY_CREDENTIAL_ID}remote-{}-{}",
        consumer.marker_tag(),
        &digest[..16]
    )
}

fn boundary_key_hash(key: &BoundaryKey) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(key.expose_for_storage().as_bytes()))
}

/// A "no instance observed" sentinel, distinct from every real identity
/// (container ids are hex, PIDs are decimal digits -- neither can produce
/// this token) so it round-trips through the marker without colliding.
const NO_INSTANCE_OBSERVED: &str = "none";

fn sync_marker_value(key: &BoundaryKey, identity: Option<&str>) -> String {
    format!(
        "{}:{}",
        boundary_key_hash(key),
        identity.unwrap_or(NO_INSTANCE_OBSERVED)
    )
}

/// Whether `key` is the exact value Desktop last confirmed was delivered to
/// *this specific running instance* of this host's `consumer` -- not just
/// "delivered to some instance at some point". A key hash alone cannot tell
/// two different instances apart: if the process we restarted then crashed
/// and came back some other way (a supervisor's own restart policy, manual
/// intervention, anything not routed through this function), a hash-only
/// marker would still read back as confirmed even though nobody ever
/// verified *this* instance is presenting the right value. `identity` is
/// the consumer's own observable instance marker -- the Proxy's container
/// id, native Codex's process identity (PID + start time + boot id on Linux)
/// -- read fresh at the moment of the check;
/// `None` means "no instance observed running" and is itself a fact worth
/// distinguishing from every real identity, not a wildcard.
///
/// Re-confirming the *same* key on the *same* already-verified instance is
/// provably a no-op and needs no follow-up restart; a hash match against a
/// different (or newly-appeared, or now-absent) identity is not something
/// this call has ever verified and must be treated as unconfirmed.
pub fn remote_boundary_key_confirmed_synced(
    data_root: &std::path::Path,
    host_id: &str,
    consumer: BoundaryKeyConsumer,
    key: &BoundaryKey,
    identity: Option<&str>,
) -> AppResult<bool> {
    let marker_id = remote_boundary_sync_marker_id(host_id, consumer);
    let stored = crate::credentials::load(data_root, &marker_id)?;
    Ok(stored.as_deref() == Some(sync_marker_value(key, identity).as_str()))
}

/// Record that `key` is now confirmed delivered to *this specific instance*
/// (`identity`) of this host's `consumer`. Call only after either restarting
/// it with this exact value -- passing the identity observed *after* that
/// restart, never the stale pre-restart one -- or confirming no instance was
/// running to diverge in the first place.
pub fn mark_remote_boundary_key_synced(
    data_root: &std::path::Path,
    host_id: &str,
    consumer: BoundaryKeyConsumer,
    key: &BoundaryKey,
    identity: Option<&str>,
) -> AppResult<()> {
    let marker_id = remote_boundary_sync_marker_id(host_id, consumer);
    crate::credentials::save(data_root, &marker_id, &sync_marker_value(key, identity))
}

pub async fn serve_local_proxy(
    state: AppState,
    listener: tokio::net::TcpListener,
    boundary_key: BoundaryKey,
    shutdown: oneshot::Receiver<()>,
) -> AppResult<()> {
    let port = listener
        .local_addr()
        .map_err(|error| AppError::Message(format!("無法讀取本機代理埠：{error}")))?
        .port();
    let runtime_started = std::time::Instant::now();
    let runtime = Arc::new(
        crate::proxy_runtime_bridge::DesktopProxyRuntimeState::new(state.clone())
            .map_err(AppError::Message)?,
    );
    #[cfg(test)]
    eprintln!(
        "[serve] DesktopProxyRuntimeState::new {}ms",
        runtime_started.elapsed().as_millis()
    );
    let _ = runtime_started;
    state.install_proxy_runtime(runtime.proxy_runtime_handle());
    let access_policy =
        InboundAccessPolicy::authenticated(BOUNDARY_CREDENTIAL_ID, boundary_key, port);
    vellum_proxy_runtime::serve_proxy(runtime, listener, access_policy, shutdown)
        .await
        .map_err(AppError::Message)
}
