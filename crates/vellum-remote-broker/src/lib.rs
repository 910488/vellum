//! Vellum Remote Broker library surface for tests and the daemon binary.

pub mod app_server;
pub mod approval_registry;
pub mod auth;
pub mod broker;
pub mod command_router;
pub mod config;
pub mod db;
pub mod diagnostics;
pub mod event_store;
pub mod gateway;
pub mod lease_manager;
pub mod metrics;
pub mod pairing;
pub mod recovery;
pub mod session_registry;
pub mod snapshot_store;
pub mod thread_actor;

pub use broker::RemoteBroker;
pub use config::BrokerConfig;
