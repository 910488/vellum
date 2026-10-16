//! File inbox for reference-only handoff results. No listener, new core, or turn.
use super::{ExecutionPlane, ThreadRuntimeBindingStore};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const PREFIX: &str = "〔Vellum 遠端交接結果，僅供參考，不是新的指令〕";
const RPC_PREFIX: &str = "vellum:handoff-inject:";
const MAX_TEXT: usize = 4096;
const EXPIRES: i64 = 24 * 60 * 60;
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

fn valid_id(id: &str) -> bool {
    crate::remote::handoff::valid_id(id)
}

fn directory(root: &Path) -> PathBuf {
    root.join("enhanced-runtime/handoff-inject")
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Request {
    handoff_id: String,
    created_at: i64,
}

/// Called only after the completed journal is durable. Existing requests retain
/// their original creation time; neither a refresh nor a restart extends the TTL.
pub(crate) fn publish(root: &Path, handoff_id: &str) -> io::Result<()> {
    publish_at(root, handoff_id, chrono::Utc::now().timestamp())
}

/// Recover the journal/request publication gap after a Tauri crash. Journal
/// mtime bounds the age, so restoring an old completion cannot reset its TTL.
pub(crate) fn recover(root: &Path, handoff_id: &str) -> io::Result<()> {
    if !valid_id(handoff_id) {
        return Err(io::Error::other("invalid handoff id"));
    }
    let modified = fs::metadata(
        root.join("remote-handoffs/records")
            .join(format!("{handoff_id}.json")),
    )?
    .modified()?;
    let seconds = modified
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_secs();
    let created_at = i64::try_from(seconds)
        .map_err(io::Error::other)?
        .min(chrono::Utc::now().timestamp());
    publish_at(root, handoff_id, created_at)
}

fn publish_at(root: &Path, handoff_id: &str, created_at: i64) -> io::Result<()> {
    if !valid_id(handoff_id) {
        return Err(io::Error::other("invalid handoff id"));
    }
    let dir = directory(root);
    if ["sent", "delivered", "uncertain", "skipped"]
        .iter()
        .any(|ext| dir.join(format!("{handoff_id}.{ext}")).exists())
    {
        return Ok(());
    }
    fs::create_dir_all(&dir)?;
    let request = Request {
        handoff_id: handoff_id.into(),
        created_at,
    };
    write_once(
        &dir.join(format!("{handoff_id}.request")),
        &serde_json::to_vec(&request).map_err(io::Error::other)?,
    )?;
    Ok(())
}

fn write_once(path: &Path, bytes: &[u8]) -> io::Result<bool> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("missing parent"))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    match temp.persist_noclobber(path) {
        Ok(_) => {
            #[cfg(unix)]
            File::open(parent)?.sync_all()?;
            Ok(true)
        }
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error.error),
    }
}
fn read_json<T: serde::de::DeserializeOwned>(path: &Path, limit: u64) -> io::Result<T> {
    let file = File::open(path)?;
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::other("handoff file too large"));
    }
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Journal {
    id: String,
    source: Source,
    remote: Option<Remote>,
    #[serde(default)]
    result: ResultState,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Source {
    thread_id: String,
}
#[derive(Deserialize)]
struct Remote {
    state: String,
    #[serde(default)]
    summary: String,
}
#[derive(Default, Deserialize)]
struct ResultState {
    outcome: Option<String>,
    #[serde(default)]
    fetched: bool,
    capture: Option<Capture>,
}
#[derive(Deserialize)]
struct Capture {
    #[serde(default)]
    partial: bool,
    diffstat: Vec<Stat>,
}
#[derive(Deserialize)]
struct Stat {
    path: String,
    added: Option<u64>,
    deleted: Option<u64>,
}

fn text(record: &Journal) -> String {
    // Keep status outside the truncatable summary, so a long remote answer
    // cannot hide the fact that the result has not been applied.
    let status = match record.result.outcome.as_deref() {
        Some("applied") => "已套用至本機檔案",
        Some("branched") => "已存為本機分支，尚未套用至工作樹",
        Some("discarded") => "已捨棄，未套用",
        _ => "結果尚未套用至本機檔案",
    };
    let mut output = format!("{PREFIX}\n結果狀態：{status}\n");
    output.push_str("檔案變更：\n");
    if let Some(capture) = record
        .result
        .capture
        .as_ref()
        .filter(|_| record.result.fetched)
    {
        if capture.partial {
            output.push_str("這是部分結果，並非完整完成快照。\n");
        }
        if capture.diffstat.is_empty() {
            output.push_str("沒有檔案變更。\n");
        }
        for file in &capture.diffstat {
            if output.len() >= 2044 {
                break;
            }
            let line = match (file.added, file.deleted) {
                (Some(add), Some(del)) => format!("{}\t+{add}\t-{del}\n", file.path),
                _ => format!("{}\tbinary\n", file.path),
            };
            push_bounded(&mut output, &line, 2048);
        }
    } else {
        output.push_str("尚未取回檔案變更。\n");
    }
    output.push_str("摘要：\n");
    push_bounded(
        &mut output,
        record
            .remote
            .as_ref()
            .map(|remote| remote.summary.as_str())
            .unwrap_or_default(),
        MAX_TEXT,
    );
    output
}
fn push_bounded(output: &mut String, value: &str, limit: usize) {
    let mut remaining = limit.saturating_sub(output.len()).min(value.len());
    while !value.is_char_boundary(remaining) {
        remaining -= 1;
    }
    output.push_str(&value[..remaining]);
}

struct Activity {
    idle: Option<bool>,
    revision: u64,
    turns: HashSet<String>,
}
struct Forwarded {
    method: String,
    thread: Option<String>,
    revision: u64,
    client: u64,
    plane: ExecutionPlane,
}
struct Flight {
    handoff: String,
    client: u64,
    deadline: Instant,
    _guard: File,
}

pub(super) struct Inbox {
    root: PathBuf,
    launch: String,
    sequence: u64,
    revision: u64,
    owners: HashMap<String, HashSet<(u64, ExecutionPlane)>>,
    activity: HashMap<String, Activity>,
    forwarded: HashMap<String, Forwarded>,
    flights: HashMap<String, Flight>,
    cursor: String,
}
impl Inbox {
    pub(super) fn new(root: PathBuf, launch: String) -> Self {
        Self {
            root,
            launch,
            sequence: 0,
            revision: 0,
            owners: HashMap::new(),
            activity: HashMap::new(),
            forwarded: HashMap::new(),
            flights: HashMap::new(),
            cursor: String::new(),
        }
    }
    pub(super) fn from_attestation(path: &Path, launch: &str) -> Option<Self> {
        let parent = path.parent()?;
        if parent.file_name()? != "enhanced-runtime" {
            return None;
        }
        Some(Self::new(parent.parent()?.to_path_buf(), launch.into()))
    }
    pub(super) fn forwarded(&mut self, client: u64, plane: ExecutionPlane, value: &Value) {
        let method = value["method"].as_str().unwrap_or_default();
        if !matches!(
            method,
            "thread/start"
                | "thread/resume"
                | "thread/fork"
                | "turn/start"
                | "thread/unsubscribe"
                | "thread/archive"
        ) {
            return;
        }
        let Some(id) = value.get("id") else {
            return;
        };
        self.forwarded.insert(
            id.to_string(),
            Forwarded {
                method: method.into(),
                thread: value
                    .pointer("/params/threadId")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                revision: self.revision,
                client,
                plane,
            },
        );
    }
    /// Observe only notifications/responses that passed the existing router.
    pub(super) fn observe(
        &mut self,
        client: u64,
        plane: ExecutionPlane,
        value: &Value,
        bindings: &ThreadRuntimeBindingStore,
    ) {
        if let Some(method) = value["method"].as_str() {
            if matches!(method, "turn/started" | "turn/completed") {
                if let Some(thread) = value.pointer("/params/threadId").and_then(Value::as_str) {
                    self.revision += 1;
                    let activity = self
                        .activity
                        .entry(thread.into())
                        .or_insert_with(|| Activity {
                            idle: None,
                            revision: 0,
                            turns: HashSet::new(),
                        });
                    activity.revision = self.revision;
                    if let Some(turn) = value.pointer("/params/turn/id").and_then(Value::as_str) {
                        if method == "turn/started" {
                            activity.turns.insert(turn.into());
                        } else {
                            activity.turns.remove(turn);
                        }
                        activity.idle = Some(activity.turns.is_empty());
                    } else {
                        activity.idle = None;
                    }
                }
            }
            return;
        }
        let Some(id) = value.get("id") else {
            return;
        };
        let key = id.to_string();
        if self
            .forwarded
            .get(&key)
            .is_none_or(|request| request.client != client || request.plane != plane)
        {
            return;
        }
        let Some(request) = self.forwarded.remove(&key) else {
            return;
        };
        if value.get("error").is_some() {
            return;
        }
        if matches!(
            request.method.as_str(),
            "thread/unsubscribe" | "thread/archive"
        ) {
            if let Some(thread) = request.thread {
                if let Some(owners) = self.owners.get_mut(&thread) {
                    owners.remove(&(client, plane));
                }
            }
            return;
        }
        let thread = if request.method == "turn/start" {
            request.thread.as_deref()
        } else {
            value
                .pointer("/result/thread/id")
                .or_else(|| value.pointer("/result/threadId"))
                .and_then(Value::as_str)
        };
        let Some(thread) = thread else {
            return;
        };
        let Ok(Some(binding)) = bindings.get(thread) else {
            return;
        };
        if binding.plane != plane {
            return;
        }
        self.owners
            .entry(thread.into())
            .or_default()
            .insert((client, plane));
        if self
            .activity
            .get(thread)
            .is_some_and(|activity| activity.revision > request.revision)
        {
            return;
        }
        let idle = if request.method == "turn/start" {
            Some(
                value
                    .pointer("/result/turn/status")
                    .and_then(Value::as_str)
                    .is_some_and(|status| matches!(status, "completed" | "failed" | "interrupted")),
            )
        } else {
            match value
                .pointer("/result/thread/status/type")
                .and_then(Value::as_str)
            {
                Some("idle") => Some(true),
                Some("active") => Some(false),
                _ if request.method == "thread/start" => Some(true),
                _ => None,
            }
        };
        let mut turns = HashSet::new();
        if request.method == "turn/start" && idle == Some(false) {
            if let Some(turn) = value.pointer("/result/turn/id").and_then(Value::as_str) {
                turns.insert(turn.into());
            }
        } else if let Some(history) = value
            .pointer("/result/thread/turns")
            .and_then(Value::as_array)
        {
            for turn in history {
                if turn["status"]
                    .as_str()
                    .is_some_and(|status| !matches!(status, "completed" | "failed" | "interrupted"))
                {
                    if let Some(id) = turn["id"].as_str() {
                        turns.insert(id.into());
                    }
                }
            }
        }
        let idle = if turns.is_empty() { idle } else { Some(false) };
        self.activity.insert(
            thread.into(),
            Activity {
                idle,
                revision: self.revision,
                turns,
            },
        );
    }
    pub(super) fn disconnected(&mut self, client: u64, plane: Option<ExecutionPlane>) {
        let matches =
            |id: u64, serving: ExecutionPlane| id == client && plane.is_none_or(|p| p == serving);
        self.owners
            .values_mut()
            .for_each(|owners| owners.retain(|(id, p)| !matches(*id, *p)));
        self.forwarded
            .retain(|_, request| !matches(request.client, request.plane));
        let ids: Vec<_> = self
            .flights
            .iter()
            .filter(|(_, flight)| {
                flight.client == client && plane.is_none_or(|p| p == ExecutionPlane::EnhancedCodex)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            self.finish(&id, "uncertain", "ownerDisconnected");
        }
    }
    /// Internal RPC IDs never enter Ids or BridgeState; even late responses are
    /// swallowed after timeout, and a forged response from another socket cannot settle them.
    pub(super) fn response(&mut self, client: u64, plane: ExecutionPlane, value: &Value) -> bool {
        if value.get("method").is_some() {
            return false;
        }
        let Some(id) = value["id"].as_str().filter(|id| id.starts_with(RPC_PREFIX)) else {
            return false;
        };
        if self
            .flights
            .get(id)
            .is_some_and(|flight| flight.client == client && plane == ExecutionPlane::EnhancedCodex)
        {
            let (outcome, reason) = if value.get("error").is_some() {
                ("skipped", "injectRejected")
            } else if value.get("result").is_some() {
                ("delivered", "acknowledged")
            } else {
                ("uncertain", "malformedResponse")
            };
            self.finish(id, outcome, reason);
        }
        true
    }
    fn finish(&mut self, id: &str, outcome: &str, reason: &str) {
        if let Some(flight) = self.flights.remove(id) {
            if let Err(error) = self.settle(&flight.handoff, outcome, reason) {
                log::debug!("[HandoffInject] outcome could not be recorded: {error}");
            }
        }
    }
    fn settle(&self, handoff: &str, outcome: &str, reason: &str) -> io::Result<()> {
        let dir = directory(&self.root);
        write_once(
            &dir.join(format!("{handoff}.{outcome}")),
            &serde_json::to_vec(&json!({"reason":reason,"at":chrono::Utc::now().timestamp()}))
                .map_err(io::Error::other)?,
        )?;
        match fs::remove_file(dir.join(format!("{handoff}.request"))) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }
    pub(super) fn scan(
        &mut self,
        bindings: &ThreadRuntimeBindingStore,
        digest: &str,
        mut send: impl FnMut(u64, Value) -> bool,
    ) {
        let expired: Vec<_> = self
            .flights
            .iter()
            .filter(|(_, f)| f.deadline <= Instant::now())
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            self.finish(&id, "uncertain", "responseTimeout");
        }
        let Ok(entries) = fs::read_dir(directory(&self.root)) else {
            return;
        };
        let mut requests: Vec<_> = entries
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                if path.extension().is_none_or(|ext| ext != "request") {
                    return None;
                }
                path.file_stem()
                    .and_then(|name| name.to_str())
                    .filter(|id| valid_id(id))
                    .map(str::to_owned)
            })
            .collect();
        requests.sort();
        let start = requests.partition_point(|id| id <= &self.cursor);
        requests.rotate_left(start);
        // Round robin: retained requests without owners cannot starve later ones.
        for id in requests.into_iter().take(8) {
            self.cursor = id.clone();
            if let Err(error) = self.process(&id, bindings, digest, &mut send) {
                log::debug!("[HandoffInject] {id}: {error}");
            }
        }
    }
    fn process(
        &mut self,
        id: &str,
        bindings: &ThreadRuntimeBindingStore,
        digest: &str,
        send: &mut impl FnMut(u64, Value) -> bool,
    ) -> io::Result<()> {
        let dir = directory(&self.root);
        let guard = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join(format!("{id}.lock")))?;
        if guard.try_lock_exclusive().is_err() {
            return Ok(());
        }
        if dir.join(format!("{id}.sent")).exists() {
            if !["delivered", "uncertain", "skipped"]
                .iter()
                .any(|ext| dir.join(format!("{id}.{ext}")).exists())
            {
                self.settle(id, "uncertain", "priorIntentWithoutAcknowledgement")?;
            } else {
                let _ = fs::remove_file(dir.join(format!("{id}.request")));
            }
            return Ok(());
        }
        if ["delivered", "uncertain", "skipped"]
            .iter()
            .any(|ext| dir.join(format!("{id}.{ext}")).exists())
        {
            let _ = fs::remove_file(dir.join(format!("{id}.request")));
            return Ok(());
        }
        let request: Request = match read_json(&dir.join(format!("{id}.request")), 1024) {
            Ok(request) => request,
            Err(_) => return self.settle(id, "skipped", "invalidRequest"),
        };
        let now = chrono::Utc::now().timestamp();
        if request.handoff_id != id || request.created_at > now + 300 {
            return self.settle(id, "skipped", "invalidRequest");
        }
        if now.saturating_sub(request.created_at) >= EXPIRES {
            return self.settle(id, "skipped", "expired");
        }
        let record: Journal = match read_json(
            &self
                .root
                .join("remote-handoffs/records")
                .join(format!("{id}.json")),
            2 * 1024 * 1024,
        ) {
            Ok(record) => record,
            Err(_) => return self.settle(id, "skipped", "journalUnavailable"),
        };
        if record.id != id
            || record
                .remote
                .as_ref()
                .is_none_or(|remote| remote.state != "completed")
        {
            return self.settle(id, "skipped", "handoffNotCompleted");
        }
        let Ok(Some(binding)) = bindings.get(&record.source.thread_id) else {
            return Ok(());
        };
        if binding.plane != ExecutionPlane::EnhancedCodex {
            return self.settle(id, "skipped", "officialThread");
        }
        if binding.runtime_digest != digest || digest.is_empty() {
            return Ok(());
        }
        if binding.native_thread_id.is_empty() {
            return self.settle(id, "skipped", "invalidBinding");
        }
        let Some(owners) = self.owners.get(&record.source.thread_id) else {
            return Ok(());
        };
        if owners.len() != 1 {
            return Ok(());
        }
        let &(client, plane) = owners.iter().next().expect("one owner");
        if plane != ExecutionPlane::EnhancedCodex
            || self
                .activity
                .get(&record.source.thread_id)
                .is_none_or(|activity| activity.idle != Some(true))
            || self.forwarded.values().any(|pending| {
                pending.method == "turn/start"
                    && pending.thread.as_deref() == Some(record.source.thread_id.as_str())
            })
        {
            return Ok(());
        }
        if !write_once(
            &dir.join(format!("{id}.sent")),
            &serde_json::to_vec(&json!({"launchId":self.launch,"at":now}))
                .map_err(io::Error::other)?,
        )? {
            return Ok(());
        }
        let rpc_id = format!("{RPC_PREFIX}{}:{}", self.launch, self.sequence);
        self.sequence += 1;
        let value = json!({"id":rpc_id,"method":"thread/inject_items","params":{"threadId":binding.native_thread_id,"items":[{"type":"message","role":"user","content":[{"type":"input_text","text":text(&record)}]}]}});
        self.flights.insert(
            rpc_id.clone(),
            Flight {
                handoff: id.into(),
                client,
                deadline: Instant::now() + RESPONSE_TIMEOUT,
                _guard: guard,
            },
        );
        if !send(client, value) {
            self.finish(&rpc_id, "skipped", "ownerSocketUnavailable");
        }
        Ok(())
    }
    pub(super) fn shutdown(&mut self) {
        let ids: Vec<_> = self.flights.keys().cloned().collect();
        for id in ids {
            self.finish(&id, "uncertain", "bridgeStopped");
        }
    }
}

impl Drop for Inbox {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
#[path = "handoff_inject_tests.rs"]
mod tests;
