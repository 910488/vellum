//! Vellum Remote Broker daemon entrypoint.

use std::path::PathBuf;

use vellum_remote_broker::{BrokerConfig, RemoteBroker};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .or_else(|| dirs::config_dir().map(|root| root.join("vellum").join("remote.toml")))
        .unwrap_or_else(|| PathBuf::from("remote.toml"));

    let config = BrokerConfig::load_or_default(&config_path)?;
    let broker = RemoteBroker::start(config).await?;
    wait_for_shutdown_signal().await?;
    broker.shutdown().await?;
    Ok(())
}

async fn wait_for_shutdown_signal() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate())?;
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await?;
        Ok(())
    }
}
