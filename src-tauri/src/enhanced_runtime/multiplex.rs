//! Multiple frontends, one pair of native task owners. Never replay a write.
use super::*;
use crate::enhanced_runtime::observations::RuntimeObservations;
use fs2::FileExt;
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::sync::mpsc as channel;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};

type Sender = channel::Sender<Value>;
enum Event {
    Client(u64, Value),
    Child(u64, ExecutionPlane, Value),
    Close(u64),
    SocketClosed(u64, ExecutionPlane),
    Relay(Value),
    RelayClosed,
}

/// Request IDs are scoped to a connection generation. Server request IDs remain
/// scoped by BridgeState and may only be answered through their source client.
#[derive(Default)]
struct Ids {
    next: u64,
    requests: HashMap<String, (u64, Value, String)>,
    approvals: HashMap<String, u64>,
}
impl Ids {
    fn request(&mut self, client: u64, value: &mut Value) -> Result<(), BridgeError> {
        if value.get("method").is_none() {
            let key = id_key(&value["id"]);
            if self.approvals.get(&key) != Some(&client) {
                return Err(BridgeError::Protocol(
                    "server response belongs to another connection".into(),
                ));
            }
            self.approvals.remove(&key);
        } else if let Some(id) = value.get("id").cloned() {
            let key = format!("vellum:client:{client}:{}", self.next);
            self.next += 1;
            self.requests.insert(
                id_key(&Value::String(key.clone())),
                (
                    client,
                    id,
                    value["method"].as_str().unwrap_or_default().into(),
                ),
            );
            value["id"] = Value::String(key);
        }
        Ok(())
    }
    fn response(&mut self, client: u64, value: &mut Value) -> Result<Option<String>, BridgeError> {
        if value.get("method").is_some() {
            if value.get("id").is_some() {
                self.approvals.insert(id_key(&value["id"]), client);
            }
            return Ok(None);
        }
        let key = id_key(&value["id"]);
        let Some((owner, original, method)) = self.requests.get(&key) else {
            return Err(BridgeError::Protocol("unsolicited child response".into()));
        };
        if *owner != client {
            return Err(BridgeError::Protocol("response connection mismatch".into()));
        }
        value["id"] = original.clone();
        let method = method.clone();
        self.requests.remove(&key);
        Ok(Some(method))
    }

    fn disconnect(&mut self, client: u64) {
        self.requests.retain(|_, (owner, _, _)| *owner != client);
        self.approvals.retain(|_, owner| *owner != client);
    }
}

/// The native App Server publishes the current Remote Control status as part
/// of its initialize notifications. The relay is intentionally quiet until it
/// is spoken to, so the bridge has to request and cache that initial status;
/// otherwise a freshly started Desktop never learns that it must restore its
/// enabled preference and the relay remains alive but offline.
struct RemoteStatusBridge {
    bootstrap_id: String,
    current: Option<Value>,
    delivered: Option<Value>,
    desktop_initialized: bool,
}

impl RemoteStatusBridge {
    fn new(launch_id: &str) -> Self {
        Self {
            bootstrap_id: format!("vellum:relay:status:{launch_id}"),
            current: None,
            delivered: None,
            desktop_initialized: false,
        }
    }

    fn bootstrap_request(&self) -> Value {
        json!({
            "kind": "control",
            "message": {
                "id": self.bootstrap_id,
                "method": "remoteControl/status/read",
                "params": {}
            }
        })
    }

    fn on_control_response(&mut self, message: &Value) -> (bool, Option<Value>) {
        if message.get("id").and_then(Value::as_str) != Some(self.bootstrap_id.as_str()) {
            return (false, None);
        }
        let notification = message
            .get("result")
            .cloned()
            .and_then(|status| self.on_status(status));
        (true, notification)
    }

    fn on_status(&mut self, status: Value) -> Option<Value> {
        self.current = Some(status);
        self.notification_if_ready()
    }

    fn on_desktop_initialized(&mut self) -> Option<Value> {
        self.desktop_initialized = true;
        self.notification_if_ready()
    }

    fn notification_if_ready(&mut self) -> Option<Value> {
        let status = self.current.as_ref()?;
        if !self.desktop_initialized || self.delivered.as_ref() == Some(status) {
            return None;
        }
        self.delivered = Some(status.clone());
        Some(json!({
            "method": "remoteControl/status/changed",
            "params": status
        }))
    }
}

pub(super) fn run(config: BridgeConfig) -> Result<(), BridgeError> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(serve(config))
}

struct Core {
    process: tokio::process::Child,
    endpoint: String,
    bearer: String,
}
impl Core {
    async fn start(config: &BridgeConfig, plane: ExecutionPlane) -> Result<Self, BridgeError> {
        let manifest = &config.manifest;
        let identity = match plane {
            ExecutionPlane::OfficialCodex => &manifest.official,
            ExecutionPlane::EnhancedCodex => &manifest.enhanced,
        };
        let mut entropy = [0u8; 32];
        getrandom::fill(&mut entropy)
            .map_err(|_| BridgeError::Protocol("cannot create transport credential".into()))?;
        let bearer = hex::encode(entropy);
        use sha2::Digest;
        let hash = hex::encode(sha2::Sha256::digest(bearer.as_bytes()));
        let mut command = tokio::process::Command::new(&identity.executable);
        let mut args = Vec::new();
        let mut skip = false;
        for arg in &config.child_args {
            if skip {
                skip = false;
                continue;
            }
            if arg == "--listen" {
                skip = true;
                continue;
            }
            if arg == "--stdio" || arg == "--remote-control" || arg.starts_with("--listen=") {
                continue;
            }
            args.push(arg);
        }
        command
            .args(args)
            .args([
                "--listen",
                "ws://127.0.0.1:0",
                "--ws-auth",
                "capability-token",
                "--ws-token-sha256",
                &hash,
            ])
            .env_remove("CODEX_CLI_PATH")
            .env_remove(super::super::launch_manifest::LAUNCH_MANIFEST_ENV)
            .env("CODEX_INTERNAL_APP_SERVER_REMOTE_CONTROL_DISABLED", "1")
            .env("NO_COLOR", "1")
            .env("CODEX_HOME", &identity.codex_home)
            .env(PLANE_ENV, plane.as_str())
            .env(DIGEST_ENV, &identity.runtime_digest)
            .env(LAUNCH_ID_ENV, &manifest.launch_id)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if plane == ExecutionPlane::EnhancedCodex {
            command
                .env(ENHANCED_COMMIT_ENV, &manifest.enhanced_commit)
                .env(
                    FEATURE_PROFILE_ENV,
                    serde_json::to_string(&manifest.feature_profile)?,
                );
        }
        #[cfg(windows)]
        command.creation_flags(0x08000000);
        let mut process = command.spawn()?;
        let mut lines = tokio::io::BufReader::new(process.stderr.take().unwrap()).lines();
        let endpoint = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            while let Some(line) = lines.next_line().await? {
                if let Some((_, address)) = line.split_once("listening on: ") {
                    let parsed = url::Url::parse(address.trim()).map_err(std::io::Error::other)?;
                    if parsed.host_str() == Some("127.0.0.1")
                        && parsed.scheme() == "ws"
                        && parsed.port().is_some()
                    {
                        return Ok::<_, std::io::Error>(address.trim().to_owned());
                    }
                }
            }
            Err(std::io::Error::other(
                "core exited before authenticated listener was ready",
            ))
        })
        .await
        .map_err(|_| BridgeError::Protocol("core listener startup timed out".into()))??;
        // Drain diagnostics without persisting prompts or credential-bearing messages.
        tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
        Ok(Self {
            process,
            endpoint,
            bearer,
        })
    }

    async fn connect(
        &self,
        client: u64,
        plane: ExecutionPlane,
        events: channel::Sender<Event>,
    ) -> Result<Sender, BridgeError> {
        let mut request = self
            .endpoint
            .clone()
            .into_client_request()
            .map_err(std::io::Error::other)?;
        request.headers_mut().insert(
            "Authorization",
            format!("Bearer {}", self.bearer)
                .parse()
                .map_err(std::io::Error::other)?,
        );
        let (socket, _) = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            tokio_tungstenite::connect_async(request),
        )
        .await
        .map_err(|_| std::io::Error::other("core handshake timed out"))?
        .map_err(std::io::Error::other)?;
        let (mut write, mut read) = socket.split();
        let (tx, mut rx) = channel::channel::<Value>(128);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    value = rx.recv() => {
                        let Some(value) = value else { break };
                        if write.send(Message::Text(value.to_string().into())).await.is_err() { break; }
                    }
                    frame = read.next() => match frame {
                        Some(Ok(Message::Text(text))) => {
                            let Ok(value) = serde_json::from_str(&text) else { break };
                            if events.send(Event::Child(client,plane,value)).await.is_err() { return; }
                        }
                        Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                        _ => {}
                    }
                }
            }
            let _ = events.send(Event::SocketClosed(client, plane)).await;
        });
        Ok(tx)
    }
}

async fn deliver(
    client: u64,
    value: Value,
    out: &mut tokio::io::Stdout,
    relay: &mut tokio::process::ChildStdin,
) -> Result<(), BridgeError> {
    let envelope = if client == 0 {
        value
    } else {
        json!({"kind":"message","connection":client-1,"message":value})
    };
    let mut bytes = serde_json::to_vec(&envelope)?;
    bytes.push(b'\n');
    if client == 0 {
        out.write_all(&bytes).await?;
        out.flush().await?;
    } else {
        relay.write_all(&bytes).await?;
        relay.flush().await?;
    }
    Ok(())
}

/// Only failures in the five Remote Control acceptance stages should make the
/// Enhanced page warn about the connection. Mobile clients also probe optional
/// App Server capabilities (for example `fs/readFile`); an expected -32600 for
/// one of those probes does not mean handshake, history, streaming or control
/// is broken.
fn is_remote_acceptance_method(method: &str) -> bool {
    matches!(
        method,
        "initialize"
            | "thread/list"
            | "thread/read"
            | "thread/resume"
            | "turn/start"
            | "turn/steer"
            | "turn/interrupt"
    )
}

async fn serve(config: BridgeConfig) -> Result<(), BridgeError> {
    let m = &config.manifest;
    let relay_identity = m
        .relay
        .as_ref()
        .ok_or_else(|| BridgeError::Protocol("relay not configured".into()))?;
    // Kernel-held lock: a stale file is never interpreted as a live owner.
    let owner_path = m.official.codex_home.join("vellum-relay-owner.lock");
    let owner = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&owner_path)?;
    owner
        .try_lock_exclusive()
        .map_err(|_| BridgeError::Protocol("Remote Control owner conflict".into()))?;
    let mut official = Core::start(&config, ExecutionPlane::OfficialCodex).await?;
    let mut enhanced = Core::start(&config, ExecutionPlane::EnhancedCodex).await?;
    let mut attestation = AttestationWriter::new(
        m.attestation_path.clone(),
        m.launch_id.clone(),
        m.model_provider_map_sha256.clone(),
        m.binding_db.clone(),
        ChildAttestation {
            binary_sha256: m.official.artifact_sha256.clone(),
            runtime_digest: m.official.runtime_digest.clone(),
            ..Default::default()
        },
        ChildAttestation {
            binary_sha256: m.enhanced.artifact_sha256.clone(),
            runtime_digest: m.enhanced.runtime_digest.clone(),
            ..Default::default()
        },
    );
    attestation.set_child_pid(
        ExecutionPlane::OfficialCodex,
        official.process.id().unwrap(),
    );
    attestation.set_child_pid(
        ExecutionPlane::EnhancedCodex,
        enhanced.process.id().unwrap(),
    );
    attestation.flush().map_err(std::io::Error::other)?;
    let mut state = BridgeState::new(
        ThreadRuntimeBindingStore::open(&m.binding_db)?,
        TrustedProviderSet::new(
            m.official_provider_ids.clone(),
            m.third_party_provider_ids.clone(),
        ),
        m.official.runtime_digest.clone(),
        m.enhanced.runtime_digest.clone(),
        config.model_provider_map.clone(),
        attestation,
        QualificationJournal::new(m.qualification_journal_path.clone(), m.launch_id.clone()),
    );
    let mut command = tokio::process::Command::new(&relay_identity.executable);
    command
        .env("CODEX_HOME", &m.official.codex_home)
        .env_remove("CODEX_CLI_PATH")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Relay startup/auth failures must reach Desktop's existing log
        // capture instead of leaving only an unexplained offline process.
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let mut relay = command.spawn()?;
    let mut relay_in = relay.stdin.take().unwrap();
    let mut relay_lines = tokio::io::BufReader::new(relay.stdout.take().unwrap()).lines();
    let mut remote_status = RemoteStatusBridge::new(&m.launch_id);
    let mut bootstrap = serde_json::to_vec(&remote_status.bootstrap_request())?;
    bootstrap.push(b'\n');
    relay_in.write_all(&bootstrap).await?;
    relay_in.flush().await?;
    let (tx, mut rx) = channel::channel(128);
    let relay_tx = tx.clone();
    tokio::spawn(async move {
        while let Ok(Some(line)) = relay_lines.next_line().await {
            let Ok(value) = serde_json::from_str(&line) else {
                break;
            };
            if relay_tx.send(Event::Relay(value)).await.is_err() {
                return;
            }
        }
        let _ = relay_tx.send(Event::RelayClosed).await;
    });
    let stdin_tx = tx.clone();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            let Ok(value) = serde_json::from_str(&line) else {
                break;
            };
            if stdin_tx.blocking_send(Event::Client(0, value)).is_err() {
                return;
            }
        }
        let _ = stdin_tx.blocking_send(Event::Close(0));
    });
    let mut sockets: HashMap<(u64, ExecutionPlane), Sender> = HashMap::new();
    let mut clients = HashSet::from([0_u64]);
    let mut failed_sockets = HashSet::new();
    let mut ids = Ids::default();
    let mut out = tokio::io::stdout();
    let mut observations = RuntimeObservations::new(m.launch_id.clone());
    observations.remote.owner_pid = relay.id();
    let observation_path = m.attestation_path.with_file_name("observations.json");
    let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(5));
    loop {
        let event = tokio::select! {
            event = rx.recv() => { let Some(event) = event else { break }; event }
            _ = heartbeat.tick() => {
                for (plane,core) in [(ExecutionPlane::OfficialCodex,&mut official),(ExecutionPlane::EnhancedCodex,&mut enhanced)] {
                    if core.process.try_wait()?.is_some() { state.mark_child_exited(plane,"core exited"); }
                }
                let _ = observations.write(&observation_path);
                continue;
            }
        };
        let event = match event {
            Event::Relay(value) => {
                let client = value["connection"]
                    .as_u64()
                    .unwrap_or_default()
                    .saturating_add(1);
                match value["kind"].as_str() {
                    Some("open") => {
                        clients.insert(client);
                        continue;
                    }
                    Some("message") => Event::Client(client, value["message"].clone()),
                    Some("close") => Event::Close(client),
                    Some("control") => {
                        let mut message = value["message"].clone();
                        let (bootstrap, notification) = remote_status.on_control_response(&message);
                        if bootstrap {
                            observations.remote.state = message
                                .pointer("/result/status")
                                .and_then(Value::as_str)
                                .unwrap_or("unknown")
                                .into();
                            if let Some(notification) = notification {
                                deliver(0, notification, &mut out, &mut relay_in).await?;
                            }
                            continue;
                        }
                        if ids.response(0, &mut message).is_ok() {
                            deliver(0, message, &mut out, &mut relay_in).await?;
                        }
                        continue;
                    }
                    Some("status") => {
                        observations.remote.state = value
                            .pointer("/status/status")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                            .into();
                        if let Some(notification) = remote_status.on_status(value["status"].clone())
                        {
                            deliver(0, notification, &mut out, &mut relay_in).await?;
                        }
                        continue;
                    }
                    Some("ready") => {
                        observations.remote.state = "transportReady".into();
                        continue;
                    }
                    _ => continue,
                }
            }
            other => other,
        };
        let (client, actions) = match event {
            Event::Client(client, mut value) => {
                if !clients.contains(&client) {
                    continue;
                }
                let original = value.clone();
                // Only the authenticated Desktop control channel may manage pairings.
                if client != 0
                    && crate::enhanced_runtime::contracts::is_remote_control(
                        value["method"].as_str().unwrap_or_default(),
                    )
                {
                    deliver(client,json!({"id":value["id"],"error":{"code":-32600,"message":"Remote Control management requires Desktop"}}),&mut out,&mut relay_in).await?;
                    continue;
                }
                if client != 0 && value["method"] == "initialize" {
                    observations.remote.clients.insert(
                        client.to_string(),
                        value
                            .pointer("/params/clientInfo/version")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                            .chars()
                            .take(64)
                            .collect(),
                    );
                }
                let actions = ids
                    .request(client, &mut value)
                    .and_then(|()| state.on_client_line(&value.to_string()));
                match actions {
                    Ok(actions) => (client, actions),
                    Err(error) => {
                        deliver(
                            client,
                            client_error(&original.to_string(), &error),
                            &mut out,
                            &mut relay_in,
                        )
                        .await?;
                        continue;
                    }
                }
            }
            Event::Child(client, plane, value) => {
                if !clients.contains(&client) {
                    continue;
                }
                observations.observe(plane.as_str(), &value);
                if client != 0 && value["method"] == "item/agentMessage/delta" {
                    observations.remote.stream_observed = true;
                }
                match state.on_child_line(plane, &value.to_string()) {
                    Ok(actions) => (client, actions),
                    Err(_) => {
                        observations.remote.last_failure_stage = Some("protocol".into());
                        continue;
                    }
                }
            }
            Event::Close(0) => break,
            Event::Close(client) => {
                clients.remove(&client);
                sockets.retain(|(id, _), _| *id != client);
                failed_sockets.retain(|(id, _)| *id != client);
                ids.disconnect(client);
                observations.remote.clients.remove(&client.to_string());
                continue;
            }
            Event::SocketClosed(client, plane) => {
                if !clients.contains(&client) {
                    continue;
                }
                sockets.remove(&(client, plane));
                failed_sockets.insert((client, plane));
                observations.remote.last_failure_stage = Some("coreConnection".into());
                // No reconnect/replay of an uncertain write.
                deliver(client,json!({"method":"vellum/runtimeFailed","params":{"plane":plane.as_str(),"message":"Core connection closed; request outcome may be unknown"}}),&mut out,&mut relay_in).await?;
                continue;
            }
            Event::RelayClosed => {
                for client in clients.iter().copied().filter(|client| *client != 0) {
                    ids.disconnect(client);
                }
                clients.retain(|client| *client == 0);
                sockets.retain(|(client, _), _| *client == 0);
                observations.remote.clients.clear();
                observations.remote.state = "failed".into();
                observations.remote.last_failure_stage = Some("relayProcess".into());
                continue;
            }
            Event::Relay(_) => unreachable!(),
        };
        for action in actions {
            match action {
                BridgeAction::ToChild(plane, value) => {
                    if failed_sockets.contains(&(client, plane))
                        || (!sockets.contains_key(&(client, plane))
                            && value["method"] != "initialize")
                    {
                        let mut error = json!({"id":value["id"],"error":{"code":-32074,"message":"Core connection unavailable; reconnect and initialize before retrying"}});
                        if value.get("id").is_some() && ids.response(client, &mut error).is_ok() {
                            deliver(client, error, &mut out, &mut relay_in).await?;
                        }
                        continue;
                    }
                    if let std::collections::hash_map::Entry::Vacant(entry) =
                        sockets.entry((client, plane))
                    {
                        let core = match plane {
                            ExecutionPlane::OfficialCodex => &official,
                            ExecutionPlane::EnhancedCodex => &enhanced,
                        };
                        match core.connect(client, plane, tx.clone()).await {
                            Ok(socket) => {
                                entry.insert(socket);
                            }
                            Err(_) => {
                                failed_sockets.insert((client, plane));
                                let mut error = json!({"id":value["id"],"error":{"code":-32074,"message":"Authenticated core connection failed"}});
                                if ids.response(client, &mut error).is_ok() {
                                    deliver(client, error, &mut out, &mut relay_in).await?;
                                }
                                continue;
                            }
                        }
                    }
                    if !matches!(
                        tokio::time::timeout(
                            std::time::Duration::from_secs(5),
                            sockets[&(client, plane)].send(value)
                        )
                        .await,
                        Ok(Ok(()))
                    ) {
                        sockets.remove(&(client, plane));
                        failed_sockets.insert((client, plane));
                        observations.remote.last_failure_stage = Some("backpressure".into());
                        deliver(client,json!({"method":"vellum/runtimeFailed","params":{"plane":plane.as_str(),"message":"Core transport stalled; outstanding operation results may be unknown"}}),&mut out,&mut relay_in).await?;
                    }
                }
                BridgeAction::ToRelay(value) => {
                    let bytes = format!("{}\n", json!({"kind":"control","message":value}));
                    relay_in.write_all(bytes.as_bytes()).await?;
                }
                BridgeAction::ToClient(mut value) => {
                    if let Ok(method) = ids.response(client, &mut value) {
                        let initialized = client == 0
                            && method.as_deref() == Some("initialize")
                            && value.get("error").is_none();
                        if client != 0 {
                            if value.get("error").is_some() {
                                if method.as_deref().is_some_and(is_remote_acceptance_method) {
                                    observations.remote.last_failure_stage = method.clone();
                                    observations.remote.last_error_code =
                                        value.pointer("/error/code").and_then(Value::as_i64);
                                }
                            } else {
                                if method.as_ref()
                                    == observations.remote.last_failure_stage.as_ref()
                                {
                                    observations.remote.last_failure_stage = None;
                                    observations.remote.last_error_code = None;
                                }
                                match method.as_deref() {
                                    Some("initialize") => {
                                        observations.remote.handshake_observed = true
                                    }
                                    Some("thread/list") => observations.remote.list_observed = true,
                                    Some("thread/read" | "thread/resume") => {
                                        observations.remote.history_observed = true
                                    }
                                    Some("turn/start" | "turn/steer" | "turn/interrupt") => {
                                        observations.remote.control_observed = true
                                    }
                                    _ => {}
                                }
                            }
                        }
                        deliver(client, value, &mut out, &mut relay_in).await?;
                        if initialized {
                            if let Some(notification) = remote_status.on_desktop_initialized() {
                                deliver(0, notification, &mut out, &mut relay_in).await?;
                            }
                        }
                    }
                }
            }
        }
    }
    let _ = relay.kill().await;
    let _ = official.process.kill().await;
    let _ = enhanced.process.kill().await;
    state.attestation.mark_stopped();
    let _ = state.attestation.flush();
    observations.remote.state = "stopped".into();
    let _ = observations.write(&observation_path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn same_request_ids_on_two_clients_never_collide() {
        let mut ids = Ids::default();
        let mut desktop = json!({"id":1,"method":"thread/resume"});
        let mut mobile = desktop.clone();
        ids.request(0, &mut desktop).unwrap();
        ids.request(1, &mut mobile).unwrap();
        assert_ne!(desktop["id"], mobile["id"]);
        let mut response = json!({"id":mobile["id"],"result":{}});
        assert!(ids.response(0, &mut response).is_err());
        ids.response(1, &mut response).unwrap();
        assert_eq!(response["id"], 1);
    }

    #[test]
    fn wrong_client_cannot_consume_approval() {
        let mut ids = Ids::default();
        let mut request = json!({"id":"approval","method":"item/tool/call"});
        ids.response(0, &mut request).unwrap();
        let mut response = json!({"id":"approval","result":{}});
        assert!(ids.request(1, &mut response).is_err());
        assert!(ids.request(0, &mut response).is_ok());
        assert!(ids.request(0, &mut response).is_err());
    }

    #[test]
    fn disconnect_discards_old_generation_ids() {
        let mut ids = Ids::default();
        let mut request = json!({"id":1,"method":"turn/start"});
        ids.request(9, &mut request).unwrap();
        ids.disconnect(9);
        assert!(ids
            .response(9, &mut json!({"id":request["id"],"result":{}}))
            .is_err());
    }

    #[test]
    fn optional_mobile_capability_errors_are_not_remote_connection_failures() {
        assert!(!is_remote_acceptance_method("fs/readFile"));
        assert!(!is_remote_acceptance_method("fs/writeFile"));
        for method in [
            "initialize",
            "thread/list",
            "thread/read",
            "thread/resume",
            "turn/start",
            "turn/steer",
            "turn/interrupt",
        ] {
            assert!(is_remote_acceptance_method(method), "{method}");
        }
    }

    #[test]
    fn relay_bootstrap_status_is_delivered_after_desktop_initialize() {
        let mut status = RemoteStatusBridge::new("launch-test");
        assert_eq!(
            status.bootstrap_request()["message"]["method"],
            "remoteControl/status/read"
        );
        let response = json!({
            "id": "vellum:relay:status:launch-test",
            "result": {
                "status": "disabled",
                "serverName": "desktop",
                "installationId": "installation",
                "environmentId": null
            }
        });
        let (handled, notification) = status.on_control_response(&response);
        assert!(handled);
        assert_eq!(notification, None, "status must wait for initialize");

        let notification = status.on_desktop_initialized().unwrap();
        assert_eq!(notification["method"], "remoteControl/status/changed");
        assert_eq!(notification["params"]["status"], "disabled");
        assert_eq!(status.on_desktop_initialized(), None, "do not duplicate");
    }

    #[test]
    fn relay_status_changes_are_forwarded_after_initialize() {
        let mut status = RemoteStatusBridge::new("launch-test");
        assert_eq!(status.on_desktop_initialized(), None);
        let connected = json!({
            "status": "connected",
            "serverName": "desktop",
            "installationId": "installation",
            "environmentId": "environment"
        });
        assert_eq!(
            status.on_status(connected.clone()).unwrap()["params"],
            connected
        );
        assert_eq!(status.on_status(connected), None, "do not duplicate");
    }
}
