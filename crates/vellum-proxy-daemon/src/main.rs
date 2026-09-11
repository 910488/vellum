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
