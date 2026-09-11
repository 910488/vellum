use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use crate::inbound::{BoundaryKey, InboundAccessPolicy};
use crate::server::build_headless_router;
use crate::state::{ProxyRuntimeState, StaticProxyState};

#[derive(Debug, Clone)]
pub struct ProxyServeOptions {
    pub listen: SocketAddr,
}

pub async fn serve_proxy_with_router(
    listener: TcpListener,
    app: Router,
    shutdown: oneshot::Receiver<()>,
) -> Result<(), String> {
    serve_proxy_with_router_until(listener, app, shutdown, None).await
}

/// Serve until `shutdown` fires, then stop accepting immediately. When
/// `on_stop` is set it runs as soon as shutdown is observed so the caller
/// can cancel the current generation's in-flight work instead of waiting
/// for a drain timeout. The serve future is dropped rather than drained:
/// in-flight handlers are cancelled instead of waiting out upstream I/O.
pub async fn serve_proxy_with_router_until(
    listener: TcpListener,
    app: Router,
    shutdown: oneshot::Receiver<()>,
    on_stop: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
) -> Result<(), String> {
    let server = axum::serve(listener, app);
    tokio::select! {
        result = server => result.map_err(|error| format!("proxy serve failed: {error}")),
        _ = shutdown => {
            if let Some(on_stop) = on_stop {
                on_stop();
            }
            Ok(())
        }
    }
}

pub async fn serve_proxy<S>(
    state: Arc<S>,
    listener: TcpListener,
    access_policy: InboundAccessPolicy,
    shutdown: oneshot::Receiver<()>,
) -> Result<(), String>
where
    S: ProxyRuntimeState,
{
    let on_stop = {
        let runtime = state.proxy_runtime();
        Some(std::sync::Arc::new(move || {
            let _ = runtime.cancel_all_in_flight_for_stop();
        }) as std::sync::Arc<dyn Fn() + Send + Sync>)
    };
    let app = build_headless_router(state, access_policy);
    serve_proxy_with_router_until(listener, app, shutdown, on_stop).await
}

/// Read this proxy's boundary key from the credential store named by its
/// config.
///
/// Fails rather than generating one: the key has to match what the Codex
/// provider header already carries, so a proxy that cannot find its key is
/// misconfigured, not free to pick a new one. Provisioning is the caller's job
/// — Desktop on first start, the Remote rollout when it uploads the secret.
pub async fn resolve_boundary_key<S>(state: &S) -> Result<BoundaryKey, String>
where
    S: ProxyRuntimeState + ?Sized,
{
    let credential_id = state.inbound_credential_id();
    let credentials = state.credentials().await;
    let raw = credentials
        .get_secret(&credential_id)
        .await
        .map_err(|error| format!("failed to read the proxy boundary key: {error}"))?
        .ok_or_else(|| {
            format!(
                "no proxy boundary key is provisioned under `{credential_id}`; \
                 the proxy will not start without one"
            )
        })?;
    BoundaryKey::parse(&raw)
        .map_err(|error| format!("stored proxy boundary key is unusable: {error}"))
}

pub async fn bind_and_serve_static(
    mut state: StaticProxyState,
    shutdown: oneshot::Receiver<()>,
) -> Result<(), String> {
    let listen = state.config().listen;
    state.config().ensure_dirs()?;
    let credential_id = state.config().inbound_access.credential_id.clone();
    // Resolved before the listener exists, so a missing key is a startup
    // failure and never a window where the port is open without a guard.
    let key = resolve_boundary_key(&state).await?;
    let listener = TcpListener::bind(listen)
        .await
        .map_err(|error| format!("failed to bind {listen}: {error}"))?;
    let port = listener
        .local_addr()
        .map_err(|error| format!("failed to read the bound address: {error}"))?
        .port();
    state.mark_listener_ready();
    let state = Arc::new(state);
    let access_policy = InboundAccessPolicy::authenticated(credential_id, key, port);
    serve_proxy(state, listener, access_policy, shutdown).await
}
