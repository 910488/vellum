//! Headless Vellum proxy daemon.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use tokio::sync::oneshot;
use tracing_subscriber::EnvFilter;
use vellum_proxy_runtime::runtime::bind_and_serve_static;
use vellum_proxy_runtime::{serve_proxy, ProxyRuntimeConfig, ProxyRuntimeState, StaticProxyState};

#[derive(Debug, Parser)]
#[command(
    name = "vellum-proxy-daemon",
    version,
    about = "Headless Vellum model proxy"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Serve the headless proxy.
    Serve {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        listen: Option<SocketAddr>,
    },
    /// Validate configuration without starting the listener.
    ValidateConfig {
        #[arg(long)]
        config: PathBuf,
    },
    /// Print version metadata.
    Version,
    /// Run local diagnostics against a config.
    Doctor {
        #[arg(long)]
        config: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();
    let cli = Cli::parse();
    match cli.command {
        Commands::Serve { config, listen } => {
            let mut cfg = ProxyRuntimeConfig::load(&config)?;
            if let Some(root) = std::env::var_os("VELLUM_REMOTE_STATE_ROOT") {
                resolve_native_paths(&mut cfg, &std::path::PathBuf::from(root));
            }
            if let Some(listen) = listen {
                cfg.listen = listen;
            }
            cfg.validate()?;
            cfg.ensure_dirs()?;
            let state = StaticProxyState::from_config(cfg)?;
            // Listener readiness is marked inside bind_and_serve_static.
            let (shutdown_tx, shutdown_rx) = oneshot::channel();
            install_shutdown_handler(shutdown_tx);
            tracing::info!(
                listen = %state.config().listen,
                install_id = %state.identity().install_id,
                "starting vellum-proxy-daemon"
            );
            // Mark ready after bind in helper.
            bind_and_serve_static(state, shutdown_rx).await?;
        }
        Commands::ValidateConfig { config } => {
            let cfg = ProxyRuntimeConfig::load(&config)?;
            cfg.validate()?;
            println!("ok");
        }
        Commands::Version => {
            println!(
                "{}",
                serde_json::json!({
                    "proxy_runtime": vellum_proxy_runtime::PROXY_RUNTIME_VERSION,
                    "daemon": env!("CARGO_PKG_VERSION"),
                })
            );
        }
        Commands::Doctor { config } => {
            let cfg = ProxyRuntimeConfig::load(&config)?;
            cfg.validate()?;
            let mut state = StaticProxyState::from_config(cfg)?;
            state.mark_listener_ready();
            let diag = state.diagnostics();
            println!("{}", serde_json::to_string_pretty(&diag)?);
            if !diag.ready {
                std::process::exit(2);
            }
        }
    }
    Ok(())
}

/// The persisted config is shared with legacy Docker installs. Resolve only
/// its fixed container mount points; leave model/credential references and the
/// persisted configuration identity intact during native migration.
fn resolve_native_paths(config: &mut ProxyRuntimeConfig, root: &std::path::Path) {
    if config.data_dir == std::path::Path::new("/var/lib/vellum/data") {
        config.data_dir = root.join("proxy/data");
    }
    if config.history_dir == std::path::Path::new("/var/lib/vellum/history") {
        config.history_dir = root.join("proxy/history");
    }
    if config.log_dir == std::path::Path::new("/var/log/vellum") {
        config.log_dir = root.join("proxy/logs");
    }
    if config.credentials_dir.as_deref() == Some(std::path::Path::new("/run/secrets")) {
        config.credentials_dir = Some(root.join("secrets"));
    }
    if config.model_catalog_path.as_deref()
        == Some(std::path::Path::new("/etc/vellum/model-catalog.json"))
    {
        config.model_catalog_path = Some(root.join("proxy/config/model-catalog.json"));
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .json()
        .init();
}

fn install_shutdown_handler(tx: oneshot::Sender<()>) {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let mut sigterm = signal(SignalKind::terminate()).expect("sigterm handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = sigterm.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        let _ = tx.send(());
        tracing::info!("shutdown signal received; draining proxy");
    });
}

// Keep serve_proxy import usable for future direct-listener paths.
#[allow(dead_code)]
async fn _serve_bound(
    state: Arc<StaticProxyState>,
    listener: tokio::net::TcpListener,
    access_policy: vellum_proxy_runtime::InboundAccessPolicy,
) {
    let (tx, rx) = oneshot::channel();
    let _ = tx;
    let _ = serve_proxy(state, listener, access_policy, rx).await;
}

#[cfg(test)]
mod native_path_tests {
    use super::*;

    #[test]
    fn container_mounts_resolve_under_native_state_root_without_changing_identity() {
        let mut config = ProxyRuntimeConfig {
            data_dir: "/var/lib/vellum/data".into(),
            history_dir: "/var/lib/vellum/history".into(),
            log_dir: "/var/log/vellum".into(),
            credentials_dir: Some("/run/secrets".into()),
            model_catalog_path: Some("/etc/vellum/model-catalog.json".into()),
            ..ProxyRuntimeConfig::default()
        };
        let identity = config.identity.clone();
        let root = std::path::Path::new("/home/user/.local/share/vellum-remote");
        resolve_native_paths(&mut config, root);
        assert_eq!(config.data_dir, root.join("proxy/data"));
        assert_eq!(config.history_dir, root.join("proxy/history"));
        assert_eq!(config.log_dir, root.join("proxy/logs"));
        assert_eq!(config.credentials_dir, Some(root.join("secrets")));
        assert_eq!(
            config.model_catalog_path,
            Some(root.join("proxy/config/model-catalog.json"))
        );
        assert_eq!(config.identity.config_hash, identity.config_hash);
        config.data_dir = root.join("custom-data");
        resolve_native_paths(&mut config, root);
        assert_eq!(config.data_dir, root.join("custom-data"));
    }
}
