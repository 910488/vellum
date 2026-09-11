//! Top-level broker orchestration.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{broadcast, RwLock};
use vellum_remote_protocol::SubscriptionMode;

use crate::app_server::adapter::AppServerAdapter;
use crate::app_server::compatibility::{self, CompatibilityState};
use crate::app_server::runtime::ResolvedCodexRuntime;
use crate::app_server::supervisor::UpstreamSupervisor;
use crate::app_server::transport::AppServerTransport;
use crate::app_server::unix_ws::UnixWsAppServerTransport;
use crate::approval_registry::ApprovalRegistry;
use crate::config::BrokerConfig;
use crate::db::BrokerDatabase;
use crate::event_store::EventStore;
use crate::gateway::{self, GatewayState};
use crate::lease_manager::LeaseManager;
use crate::metrics::BrokerMetrics;
use crate::pairing::PairingService;
use crate::recovery::reconcile_processing_commands;
use crate::session_registry::SessionRegistry;
use crate::snapshot_store::SnapshotStore;
use crate::thread_actor::{ThreadActor, ThreadActorHandle};

pub struct RemoteBroker {
    pub config: BrokerConfig,
    pub metrics: Arc<BrokerMetrics>,
    shutdown: broadcast::Sender<()>,
    join: tokio::task::JoinHandle<()>,
}

impl RemoteBroker {
    pub async fn start(config: BrokerConfig) -> Result<Self, Box<dyn std::error::Error>> {
        if config.local_only && !config.listen_addr.ip().is_loopback() {
            return Err("local_only=true requires loopback listen_addr".into());
        }
        config.ensure_dirs()?;

        // Single shared DB + versioned migrations. Stores never re-run migrations.
        let database = BrokerDatabase::open(&config.db_path())?;
        let shared = database.connection();
        let store = EventStore::from_shared(shared.clone());
        let snapshots = SnapshotStore::from_shared(shared.clone());
        let approvals = ApprovalRegistry::from_shared(shared.clone());
        let leases = LeaseManager::from_shared(shared, config.writer_lease_ttl_secs);

        let marked = reconcile_processing_commands(&store)?;
        if marked > 0 {
            log::warn!("marked {marked} processing command(s) as indeterminate on startup");
        }
        let metrics = Arc::new(BrokerMetrics::default());

        let (upstream, identity) = connect_upstream(&config).await?;
        match compatibility::evaluate(&config, &identity) {
            Ok(CompatibilityState::Compatible) => {}
            Ok(CompatibilityState::Incompatible) | Err(_) => {
                return Err(format!(
                    "upstream incompatible: version={} allowed={:?}",
                    identity.version, config.allowed_versions
                )
                .into());
            }
        }
        metrics
            .upstream_epoch
            .store(upstream.epoch(), std::sync::atomic::Ordering::Relaxed);
        let _ = store.ensure_epoch(upstream.epoch(), Some(&identity.version));

        let actors = Arc::new(RwLock::new(HashMap::<String, ThreadActorHandle>::new()));
        UpstreamSupervisor::spawn(upstream.clone(), actors.clone());

        // Rebuild only threads that need an active upstream subscription.
        for (thread_id, status, _seq) in store.list_threads()? {
            let needs_recovery = matches!(
                status.as_str(),
                "running" | "waiting_for_approval" | "indeterminate" | "loading" | "orphaned"
            );
            if !needs_recovery {
                continue;
            }
            let handle = RemoteBroker::ensure_thread_actor(
                actors.clone(),
                thread_id.clone(),
                store.clone(),
                snapshots.clone(),
                approvals.clone(),
                leases.clone(),
                upstream.clone(),
                metrics.clone(),
            )
            .await;
            let _ = handle
                .sender()
                .send(crate::thread_actor::ThreadActorMessage::RecoverOnStartup);
        }

        let sessions = SessionRegistry::default();
        let pairing = Arc::new(PairingService::default());
        let (shutdown, _) = broadcast::channel(1);

        let state = GatewayState {
            config: config.clone(),
            store,
            snapshots,
            approvals,
            leases,
            upstream: upstream.clone(),
            metrics: metrics.clone(),
            sessions,
            pairing,
            actors: actors.clone(),
        };

        let join = gateway::spawn_gateway(state, shutdown.subscribe()).await?;
        Ok(Self {
            config,
            metrics,
            shutdown,
            join,
        })
    }

    pub async fn wait_for_shutdown(self) -> Result<(), Box<dyn std::error::Error>> {
        tokio::signal::ctrl_c().await?;
        self.shutdown().await
    }

    pub async fn shutdown(self) -> Result<(), Box<dyn std::error::Error>> {
        let _ = self.shutdown.send(());
        let _ = self.join.await;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn ensure_thread_actor(
        actors: Arc<RwLock<HashMap<String, ThreadActorHandle>>>,
        thread_id: String,
        store: EventStore,
        snapshots: SnapshotStore,
        approvals: ApprovalRegistry,
        leases: LeaseManager,
        upstream: Arc<dyn AppServerTransport>,
        metrics: Arc<BrokerMetrics>,
    ) -> ThreadActorHandle {
        if let Some(existing) = actors.read().await.get(&thread_id).cloned() {
            return existing;
        }
        let handle = ThreadActor::spawn(
            thread_id.clone(),
            store,
            snapshots,
            approvals,
            leases,
            upstream,
            metrics,
        );
        actors.write().await.insert(thread_id, handle.clone());
        handle
    }
}

pub async fn get_or_spawn_actor(state: &GatewayState, thread_id: &str) -> ThreadActorHandle {
    RemoteBroker::ensure_thread_actor(
        state.actors.clone(),
        thread_id.to_string(),
        state.store.clone(),
        state.snapshots.clone(),
        state.approvals.clone(),
        state.leases.clone(),
        state.upstream.clone(),
        state.metrics.clone(),
    )
    .await
}

pub async fn default_subscribe_mode() -> SubscriptionMode {
    SubscriptionMode::Observer
}

async fn connect_upstream(
    config: &BrokerConfig,
) -> Result<
    (
        Arc<dyn AppServerTransport>,
        crate::app_server::transport::ServerIdentity,
    ),
    Box<dyn std::error::Error>,
> {
    if std::env::var("VELLUM_REMOTE_USE_FAKE_UPSTREAM")
        .ok()
        .as_deref()
        == Some("1")
    {
        log::warn!("using AppServerAdapter because VELLUM_REMOTE_USE_FAKE_UPSTREAM=1");
        let adapter = Arc::new(AppServerAdapter::new(1));
        let identity = adapter.initialize().await?;
        return Ok((adapter, identity));
    }

    // Resolve binary version before connecting. Official initialize response
    // does not include a version field.
    let runtime = match ResolvedCodexRuntime::resolve(config) {
        Ok(runtime) => runtime,
        Err(error) => {
            if std::env::var("VELLUM_REMOTE_ALLOW_FAKE_FALLBACK")
                .ok()
                .as_deref()
                == Some("1")
            {
                log::warn!(
                    "codex runtime unavailable ({error}); using fake upstream due to VELLUM_REMOTE_ALLOW_FAKE_FALLBACK=1"
                );
                let adapter = Arc::new(AppServerAdapter::new(1));
                let identity = adapter.initialize().await?;
                return Ok((adapter, identity));
            }
            return Err(error.into());
        }
    };

    match UnixWsAppServerTransport::connect_resolved(&runtime).await {
        Ok(transport) => {
            let identity = transport.initialize().await?;
            Ok((transport, identity))
        }
        Err(error) => {
            if std::env::var("VELLUM_REMOTE_ALLOW_FAKE_FALLBACK")
                .ok()
                .as_deref()
                == Some("1")
            {
                log::warn!(
                    "unix transport unavailable ({error}); using fake upstream due to VELLUM_REMOTE_ALLOW_FAKE_FALLBACK=1"
                );
                let adapter = Arc::new(AppServerAdapter::new(1));
                let identity = adapter.initialize().await?;
                Ok((adapter, identity))
            } else {
                Err(error.into())
            }
        }
    }
}
