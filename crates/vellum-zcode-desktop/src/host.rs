use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::Utc;
use tokio::time::timeout;
use uuid::Uuid;

use crate::binding::{ZcodeBindingStore, ZcodeThreadBinding};
use crate::channel::{ControlClient, ControlEvent};
use crate::discovery::{require_newest, require_qualified, TapListenRecord};
use crate::protocol::{
    ArtifactPin, BindParams, BindResult, HelloResult, StatusResult, TurnCancelParams,
    TurnCompletedEvent, TurnStartParams, TurnStartResult,
};
use crate::ZcodeDesktopError;

pub struct ZcodeDesktopHost {
    client: ControlClient,
    bindings: ZcodeBindingStore,
    hello: HelloResult,
    listen: TapListenRecord,
}

impl ZcodeDesktopHost {
    pub async fn connect(
        log_dir: impl AsRef<Path>,
        bindings_path: impl AsRef<Path>,
        pin: Option<&ArtifactPin>,
    ) -> Result<Self, ZcodeDesktopError> {
        let listen = match pin {
            Some(pin) => require_qualified(log_dir.as_ref(), &pin.cjs_sha256)?,
            None => require_newest(log_dir.as_ref())?,
        };
        let client = connect_pipe(&listen.pipe).await?;
        let hello = client.hello(pin).await?;
        let bindings = ZcodeBindingStore::open(bindings_path)?;
        Ok(Self {
            client,
            bindings,
            hello,
            listen,
        })
    }

    pub fn from_parts(
        client: ControlClient,
        bindings: ZcodeBindingStore,
        hello: HelloResult,
        listen: TapListenRecord,
    ) -> Self {
        Self {
            client,
            bindings,
            hello,
            listen,
        }
    }

    pub fn hello(&self) -> &HelloResult {
        &self.hello
    }

    pub fn listen(&self) -> &TapListenRecord {
        &self.listen
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<ControlEvent> {
        self.client.subscribe()
    }

    pub fn binding(
        &self,
        vellum_thread_id: &str,
    ) -> Result<Option<ZcodeThreadBinding>, ZcodeDesktopError> {
        self.bindings.get(vellum_thread_id)
    }

    pub async fn bind_thread(
        &self,
        vellum_thread_id: &str,
        session_id: Option<&str>,
        workspace: &str,
    ) -> Result<BindResult, ZcodeDesktopError> {
        if let Some(existing) = self.bindings.get(vellum_thread_id)? {
            if let Some(wanted) = session_id {
                if existing.zcode_session_id != wanted {
                    return Err(ZcodeDesktopError::ThreadAlreadyBound);
                }
            }
            if !self
                .hello
                .sessions
                .iter()
                .any(|session| session == &existing.zcode_session_id)
                && session_id.is_none()
            {
                return Err(ZcodeDesktopError::SessionNotFound(
                    existing.zcode_session_id,
                ));
            }
            let bound = self
                .client
                .bind(BindParams {
                    vellum_thread_id: vellum_thread_id.to_owned(),
                    session_id: Some(existing.zcode_session_id.clone()),
                })
                .await?;
            return Ok(bound);
        }
        let bound = self
            .client
            .bind(BindParams {
                vellum_thread_id: vellum_thread_id.to_owned(),
                session_id: session_id.map(str::to_owned),
            })
            .await?;
        let now = Utc::now();
        self.bindings.upsert(&ZcodeThreadBinding {
            vellum_thread_id: vellum_thread_id.to_owned(),
            zcode_session_id: bound.session_id.clone(),
            workspace: workspace.to_owned(),
            artifact_sha256: self.hello.artifact.cjs_sha256.clone(),
            runtime_instance_id: self.listen.pid.to_string(),
            created_at: now,
            last_seen_at: now,
        })?;
        Ok(bound)
    }

    pub async fn start_turn(
        &self,
        vellum_thread_id: &str,
        content: &str,
        timeout_ms: Option<u64>,
    ) -> Result<TurnStartResult, ZcodeDesktopError> {
        let _binding = self
            .bindings
            .get(vellum_thread_id)?
            .ok_or_else(|| ZcodeDesktopError::SessionNotFound(vellum_thread_id.to_owned()))?;
        let vellum_turn_id = Uuid::new_v4().to_string();
        self.client
            .start_turn(TurnStartParams {
                vellum_thread_id: vellum_thread_id.to_owned(),
                vellum_turn_id,
                content: content.to_owned(),
                timeout_ms,
            })
            .await
    }

    pub async fn start_turn_with_id(
        &self,
        vellum_thread_id: &str,
        vellum_turn_id: &str,
        content: &str,
        timeout_ms: Option<u64>,
    ) -> Result<TurnStartResult, ZcodeDesktopError> {
        self.client
            .start_turn(TurnStartParams {
                vellum_thread_id: vellum_thread_id.to_owned(),
                vellum_turn_id: vellum_turn_id.to_owned(),
                content: content.to_owned(),
                timeout_ms,
            })
            .await
    }

    pub async fn cancel_turn(
        &self,
        vellum_thread_id: &str,
        vellum_turn_id: &str,
    ) -> Result<(), ZcodeDesktopError> {
        self.client
            .cancel_turn(TurnCancelParams {
                vellum_thread_id: vellum_thread_id.to_owned(),
                vellum_turn_id: vellum_turn_id.to_owned(),
            })
            .await
    }

    pub async fn wait_turn(
        &self,
        vellum_turn_id: &str,
        deadline: Duration,
    ) -> Result<TurnCompletedEvent, ZcodeDesktopError> {
        let mut events = self.client.subscribe();
        timeout(deadline, async {
            loop {
                match events.recv().await {
                    Ok(ControlEvent::TurnCompleted(done))
                        if done.vellum_turn_id == vellum_turn_id =>
                    {
                        return Ok(done);
                    }
                    Ok(_) => continue,
                    Err(_) => return Err(ZcodeDesktopError::Closed),
                }
            }
        })
        .await
        .map_err(|_| ZcodeDesktopError::Timeout(vellum_turn_id.to_owned()))?
    }

    pub async fn status(&self) -> Result<StatusResult, ZcodeDesktopError> {
        self.client.status().await
    }
}

async fn connect_pipe(pipe: &str) -> Result<ControlClient, ZcodeDesktopError> {
    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ClientOptions;
        let client = ClientOptions::new().open(pipe)?;
        let (reader, writer) = tokio::io::split(client);
        Ok(ControlClient::from_rw(reader, writer))
    }
    #[cfg(unix)]
    {
        let stream = tokio::net::UnixStream::connect(pipe).await?;
        let (reader, writer) = stream.into_split();
        Ok(ControlClient::from_rw(reader, writer))
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = pipe;
        Err(ZcodeDesktopError::TapUnavailable)
    }
}

pub fn default_log_dir() -> PathBuf {
    std::env::temp_dir().join("zcode-host-tap")
}
