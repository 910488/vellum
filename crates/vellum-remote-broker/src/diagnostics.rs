//! Health and diagnostic helpers.

use serde::Serialize;

use crate::app_server::transport::AppServerTransport;
use crate::config::BrokerConfig;
use crate::metrics::BrokerMetrics;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthReport {
    pub ok: bool,
    pub broker_id: String,
    pub version: String,
    pub listen_addr: String,
    pub db_readable: bool,
    pub metrics: serde_json::Value,
}

pub fn health_report(
    config: &BrokerConfig,
    metrics: &BrokerMetrics,
    db_readable: bool,
) -> HealthReport {
    HealthReport {
        ok: db_readable,
        broker_id: config.broker_id.clone(),
        version: env!("CARGO_PKG_VERSION").into(),
        listen_addr: config.listen_addr.to_string(),
        db_readable,
        metrics: metrics.snapshot_json(),
    }
}

pub fn upstream_ready(transport: &dyn AppServerTransport) -> bool {
    transport.is_ready() && transport.reader_alive() && transport.writer_alive()
}
