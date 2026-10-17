//! Read-only discovery of local Codex threads; never open auth/config secrets.
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use super::handoff_git::{git_path, Git};
use crate::error::{AppError, AppResult};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalThread {
    pub thread_id: String,
    pub name: String,
    pub repo: PathBuf,
    pub branch: String,
    pub updated_at: i64,
    pub model: Option<String>,
    pub model_provider: Option<String>,
    pub effort: Option<String>,
    pub active: Option<bool>,
    pub handed_off: bool,
    /// Populated only when this thread is selected, never for the picker.
    #[serde(default)]
    pub draft: Option<BriefDraft>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BriefDraft {
    pub task: String,
    pub original_request: String,
    pub last_progress: String,
}

fn bounded_text(text: &str, limit: usize) -> String {
    let mut chars = text.trim().chars();
    let mut result: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        result.push('…');
    }
    result
}

fn message_text(payload: &Value, limit: usize) -> String {
    // response_item text blocks, not tool outputs or hidden reasoning.
    let mut text = String::new();
    for block in payload["content"].as_array().into_iter().flatten() {
        if matches!(
            block["type"].as_str(),
            Some("input_text" | "output_text" | "text")
        ) {
            if let Some(part) = block["text"].as_str() {
                if !text.is_empty() {
                    text.push('\n');
                }
                let remaining = limit.saturating_sub(text.chars().count()) + 1;
                text.extend(part.chars().take(remaining));
                if text.chars().count() > limit {
                    break;
                }
            }
        }
    }
    bounded_text(&text, limit)
}

fn error(message: impl Into<String>) -> AppError {
    AppError::Message(message.into())
}

fn injected_user_message(text: &str) -> bool {
    let text = text.trim_start().to_ascii_lowercase();
    // Older rollouts encode injected context as user messages. Recognize a
    // whole wrapper, not arbitrary user text that happens to mention markup.
    if let Some((tag, _)) = text.strip_prefix('<').and_then(|v| v.split_once('>')) {
        if !tag.is_empty()
            && tag.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
            && text.trim_end().ends_with(&format!("</{tag}>"))
        {
            return true;
        }
    }
    [
        "<environment_context",
        "<user_instructions",
        "<instructions>",
        "<agents_md",
        "<agents.md",
        "<recommended_plugins",
        "<external_codex_apps_open_page",
        "# agents.md",
    ]
    .iter()
    .any(|prefix| text.starts_with(prefix))
}

pub(crate) fn discover(home: &Path, data_root: &Path) -> AppResult<Vec<LocalThread>> {
    discover_with(home, data_root, None)
}

pub(crate) fn select(home: &Path, data_root: &Path, id: &str) -> AppResult<Vec<LocalThread>> {
    discover_with(home, data_root, Some(id))
}

fn discover_with(
    home: &Path,
    data_root: &Path,
    selected: Option<&str>,
) -> AppResult<Vec<LocalThread>> {
    if !home.exists() {
        return Ok(Vec::new());
    }
    let mut databases = fs::read_dir(home)
        .map_err(|e| error(e.to_string()))?
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("state_") && name.ends_with(".sqlite"))
        })
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    databases.sort_by_key(|path| fs::metadata(path).and_then(|m| m.modified()).ok());
    let mut threads = BTreeMap::new();
    let mut repositories = BTreeMap::<PathBuf, Option<PathBuf>>::new();
    // The newest state DB is authoritative, including its archive decisions.
    // Do not resurrect an archived row from an older database version.
    for database in databases.into_iter().rev().take(1) {
        let connection = Connection::open_with_flags(
            database,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| error(format!("HandoffThreadIndexUnavailable: {e}")))?;
        connection
            .busy_timeout(std::time::Duration::from_secs(1))
            .map_err(|e| error(e.to_string()))?;
        let columns = "id, cwd, COALESCE(NULLIF(name, ''), title), rollout_path, model_provider, updated_at, git_branch";
        let query = |columns: &str| {
            format!(
                "SELECT {columns} FROM threads WHERE archived=0 ORDER BY updated_at DESC LIMIT 100"
            )
        };
        let mut statement = connection
            .prepare(&query(columns))
            .or_else(|_| connection.prepare(&query(&columns.replace("git_branch", "NULL"))))
            .or_else(|_| {
                connection.prepare(&query(
                    &columns.replace("COALESCE(NULLIF(name, ''), title)", "title"),
                ))
            })
            .or_else(|_| {
                connection.prepare(&query(
                    &columns
                        .replace("COALESCE(NULLIF(name, ''), title)", "title")
                        .replace("git_branch", "NULL"),
                ))
            })
            .map_err(|e| error(format!("HandoffThreadIndexUnsupported: {e}")))?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            })
            .map_err(|e| error(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| error(e.to_string()))?;
        // Release the SQLite reader before invoking Git or reading a rollout.
        drop(statement);
        drop(connection);
        for row in rows {
            let (id, cwd, title, rollout, provider, updated, branch) = row;
            if !super::handoff::valid_id(&id) {
                continue;
            }
            if selected.is_some_and(|wanted| !crate::codex::conversation_key_matches(wanted, &id)) {
                continue;
            }
            let cwd = PathBuf::from(cwd);
            if !cwd.is_dir() {
                continue;
            }
            let repo = repositories.entry(cwd.clone()).or_insert_with(|| {
                let top = Git::new(&cwd, data_root)
                    .run(&["rev-parse", "--show-toplevel"], None)
                    .ok()?;
                fs::canonicalize(top.trim())
                    .ok()
                    .map(|path| git_path(&path))
            });
            let Some(repo) = repo.clone() else {
                continue;
            };
            let mut thread = LocalThread {
                thread_id: id,
                name: title,
                repo,
                branch: branch.unwrap_or_default(),
                updated_at: updated,
                model: None,
                model_provider: Some(provider),
                effort: None,
                active: None,
                handed_off: false,
                draft: None,
            };
            // Missing/unreadable rollout remains visible but cannot pass preflight.
            if selected.is_some() {
                let _ = read_settings(home, Path::new(&rollout), &cwd, &mut thread);
            }
            threads.insert(thread.thread_id.clone(), thread);
        }
    }
    let mut result = threads.into_values().collect::<Vec<_>>();
    result.sort_by_key(|thread| std::cmp::Reverse(thread.updated_at));
    result.truncate(100);
    Ok(result)
}

fn read_settings(
    home: &Path,
    rollout: &Path,
    cwd: &Path,
    thread: &mut LocalThread,
) -> AppResult<()> {
    let home = fs::canonicalize(home).map_err(|e| error(e.to_string()))?;
    let rollout = fs::canonicalize(rollout).map_err(|e| error(e.to_string()))?;
    if !rollout.starts_with(home.join("sessions")) {
        return Err(error("HandoffRolloutPathInvalid"));
    }
    let file = fs::File::open(rollout).map_err(|e| error(e.to_string()))?;
    if file.metadata().map_err(|e| error(e.to_string()))?.len() > 128 * 1024 * 1024 {
        return Err(error("HandoffRolloutTooLarge"));
    }
    let mut identity = false;
    let mut observed_cwd = None;
    let mut model = None;
    let mut effort = None;
    let mut provider = thread.model_provider.clone();
    let mut active = true;
    let mut first_user = String::new();
    let mut last_user = String::new();
    let mut first_completed_user = String::new();
    let mut last_completed_user = String::new();
    let mut last_agent = String::new();
    for line in BufReader::new(file).lines() {
        let line = line.map_err(|e| error(e.to_string()))?;
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let payload = &value["payload"];
        if value["type"] == "event_msg"
            && payload["type"] == "item_completed"
            && payload["item"]["type"] == "UserMessage"
        {
            let text = message_text(&payload["item"], 4000);
            if !text.is_empty() {
                if first_completed_user.is_empty() {
                    first_completed_user = text.clone();
                }
                last_completed_user = text;
            }
        }
        let (role, text) = match value["type"].as_str() {
            Some("response_item") if payload["type"] == "message" => {
                let role = payload["role"].as_str().unwrap_or_default();
                (
                    role,
                    message_text(payload, if role == "assistant" { 6000 } else { 4000 }),
                )
            }
            Some("event_msg") if payload["type"] == "user_message" => (
                "user",
                bounded_text(payload["message"].as_str().unwrap_or_default(), 4000),
            ),
            Some("event_msg") if payload["type"] == "agent_message" => (
                "assistant",
                bounded_text(payload["message"].as_str().unwrap_or_default(), 6000),
            ),
            _ => ("", String::new()),
        };
        if !text.is_empty() {
            match role {
                "user" if !injected_user_message(&text) => {
                    if first_user.is_empty() {
                        first_user = text.clone();
                    }
                    last_user = text;
                }
                "assistant" => last_agent = text,
                _ => {}
            }
        }
        match value["type"].as_str() {
            Some("session_meta") => {
                identity = payload["id"].as_str() == Some(thread.thread_id.as_str());
                observed_cwd = payload["cwd"].as_str().map(PathBuf::from);
                if let Some(value) = payload["model_provider"].as_str() {
                    provider = Some(value.to_owned());
                }
            }
            Some("turn_context") => {
                if let Some(path) = payload["cwd"].as_str() {
                    observed_cwd = Some(PathBuf::from(path));
                }
                model = payload["model"].as_str().map(str::to_owned);
                effort = payload["effort"]
                    .as_str()
                    .or_else(|| {
                        payload
                            .pointer("/collaboration_mode/settings/reasoning_effort")
                            .and_then(Value::as_str)
                    })
                    .map(str::to_owned);
                if let Some(value) = payload["model_provider"].as_str() {
                    provider = Some(value.to_owned());
                }
                active = true;
            }
            Some("event_msg") => match payload["type"].as_str() {
                Some("task_started" | "turn_started") => active = true,
                Some("task_complete" | "turn_complete" | "turn_aborted") => active = false,
                _ => {}
            },
            _ => {}
        }
    }
    if !identity
        || observed_cwd
            .as_deref()
            .and_then(|path| fs::canonicalize(path).ok())
            != Some(fs::canonicalize(cwd).map_err(|e| error(e.to_string()))?)
    {
        return Err(error("HandoffThreadIdentityMismatch"));
    }
    thread.model = model;
    thread.effort = effort;
    thread.model_provider = provider;
    thread.active = Some(active);
    let (task, original_request) = if first_completed_user.is_empty() {
        (last_user, first_user)
    } else {
        (last_completed_user, first_completed_user)
    };
    thread.draft = (!task.is_empty()).then_some(BriefDraft {
        task,
        original_request,
        last_progress: last_agent,
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovery_reads_index_without_writing_and_keeps_only_git_threads() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("codex");
        let repo = temp.path().join("repo");
        fs::create_dir_all(home.join("sessions")).unwrap();
        fs::create_dir(&repo).unwrap();
        let git = Git::new(&repo, temp.path());
        git.run(&["init"], None).unwrap();
        fs::write(repo.join("file"), "base\n").unwrap();
        git.run(&["add", "."], None).unwrap();
        git.run(&["commit", "-m", "base"], None).unwrap();
        let rollout = home.join("sessions/rollout.jsonl");
        let events = [
            serde_json::json!({"type": "session_meta", "payload": {"id": "thread-1", "cwd": repo, "model_provider": "vellum"}}),
            serde_json::json!({"type": "event_msg", "payload": {"type": "user_message", "message": "Original request"}}),
            serde_json::json!({"type": "event_msg", "payload": {"type": "user_message", "message": "Finish the CLI and tests"}}),
            serde_json::json!({"type": "event_msg", "payload": {"type": "agent_message", "message": "Parser complete; CLI remains"}}),
            serde_json::json!({"type": "turn_context", "payload": {"model": "model", "effort": "high"}}),
            serde_json::json!({"type": "event_msg", "payload": {"type": "task_complete"}}),
        ];
        fs::write(
            &rollout,
            events
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        let db = home.join("state_5.sqlite");
        {
            let connection = Connection::open(&db).unwrap();
            connection.execute_batch("CREATE TABLE threads(id TEXT, cwd TEXT, title TEXT, rollout_path TEXT, model_provider TEXT, updated_at INTEGER, archived INTEGER);").unwrap();
            connection
                .execute(
                    "INSERT INTO threads VALUES (?1, ?2, ?3, ?4, 'vellum', 10, 0)",
                    rusqlite::params![
                        "thread-1",
                        repo.to_str().unwrap(),
                        "My task",
                        rollout.to_str().unwrap()
                    ],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO threads VALUES ('non-git', ?1, 'Other', ?2, 'vellum', 20, 0)",
                    rusqlite::params![home.to_str().unwrap(), rollout.to_str().unwrap()],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO threads VALUES ('archived', ?1, 'Archived', ?2, 'vellum', 30, 1)",
                    rusqlite::params![repo.to_str().unwrap(), rollout.to_str().unwrap()],
                )
                .unwrap();
        }
        let before_db = fs::read(&db).unwrap();
        let before_rollout = fs::read(&rollout).unwrap();
        let before_index = fs::read(repo.join(".git/index")).unwrap();
        let result = discover(&home, temp.path()).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].thread_id, "thread-1");
        assert_eq!(result[0].name, "My task");
        assert!(result[0].model.is_none());
        assert!(result[0].active.is_none());
        assert!(result[0].draft.is_none());
        let selected = select(&home, temp.path(), "thread-1").unwrap();
        assert_eq!(selected[0].model.as_deref(), Some("model"));
        assert_eq!(selected[0].active, Some(false));
        let draft = selected[0].draft.as_ref().unwrap();
        assert_eq!(draft.task, "Finish the CLI and tests");
        assert_eq!(draft.original_request, "Original request");
        assert_eq!(draft.last_progress, "Parser complete; CLI remains");
        assert_eq!(fs::read(db).unwrap(), before_db);
        assert_eq!(fs::read(rollout).unwrap(), before_rollout);
        assert_eq!(fs::read(repo.join(".git/index")).unwrap(), before_index);
        // The picker must still work when a rollout is absent/unreadable.
        fs::remove_file(home.join("sessions/rollout.jsonl")).unwrap();
        let metadata_only = discover(&home, temp.path()).unwrap();
        assert_eq!(metadata_only.len(), 1);
        assert!(metadata_only[0].active.is_none());
        assert!(select(&home, temp.path(), "thread-1").unwrap()[0]
            .model
            .is_none());
    }
    #[test]
    fn rollout_settings_follow_last_turn_and_refuse_foreign_identity() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("sessions")).unwrap();
        let rollout = temp.path().join("sessions/rollout.jsonl");
        let mut thread = LocalThread {
            thread_id: "thread-1".into(),
            name: "Task".into(),
            repo: temp.path().into(),
            branch: "main".into(),
            updated_at: 1,
            model: None,
            model_provider: Some("old".into()),
            effort: None,
            active: None,
            handed_off: false,
            draft: None,
        };
        let records = [
            serde_json::json!({"type": "session_meta", "payload": {"id": "thread-1", "cwd": temp.path(), "model_provider": "vellum"}}),
            serde_json::json!({"type": "turn_context", "payload": {"model": "old", "effort": "low"}}),
            serde_json::json!({"type": "turn_context", "payload": {"model": "selected", "effort": "high"}}),
            serde_json::json!({"type": "event_msg", "payload": {"type": "task_complete"}}),
        ];
        fs::write(
            &rollout,
            records
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        read_settings(temp.path(), &rollout, temp.path(), &mut thread).unwrap();
        assert_eq!(thread.model.as_deref(), Some("selected"));
        assert_eq!(thread.effort.as_deref(), Some("high"));
        assert_eq!(thread.model_provider.as_deref(), Some("vellum"));
        assert_eq!(thread.active, Some(false));
        thread.thread_id = "other-thread".into();
        assert!(read_settings(temp.path(), &rollout, temp.path(), &mut thread).is_err());
    }
    #[test]
    fn draft_extracts_response_messages_and_bounds_unicode_excerpts() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("sessions")).unwrap();
        let rollout = temp.path().join("sessions/selected.jsonl");
        let mut thread = LocalThread {
            thread_id: "selected".into(),
            name: "Task".into(),
            repo: temp.path().into(),
            branch: "main".into(),
            updated_at: 1,
            model: None,
            model_provider: None,
            effort: None,
            active: None,
            handed_off: false,
            draft: None,
        };
        let events = [
            serde_json::json!({"type":"session_meta","payload":{"id":"selected","cwd":temp.path()}}),
            serde_json::json!({"type":"event_msg","payload":{"type":"user_message","message":"<recommended_plugins>Plugin context</recommended_plugins>"}}),
            serde_json::json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<external_codex_apps_open_page>Page context</external_codex_apps_open_page>"}]}}),
            serde_json::json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>\n<cwd>C:/private/home</cwd>\n</environment_context>"}]}}),
            serde_json::json!({"type":"event_msg","payload":{"type":"user_message","message":"<user_instructions>Automatic instructions</user_instructions>"}}),
            serde_json::json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions for C:/repo\n<INSTRUCTIONS>Project rules</INSTRUCTIONS>"}]}}),
            serde_json::json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"First request"}]}}),
            serde_json::json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"任務".repeat(4000)}]}}),
            serde_json::json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"進度".repeat(5000)}]}}),
            serde_json::json!({"type":"response_item","payload":{"type":"function_call_output","output":"Not an agent message"}}),
            serde_json::json!({"type":"event_msg","payload":{"type":"user_message","message":"  <environment_context>refreshed context</environment_context>"}}),
        ];
        fs::write(
            &rollout,
            events
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        read_settings(temp.path(), &rollout, temp.path(), &mut thread).unwrap();
        let draft = thread.draft.as_ref().unwrap();
        assert_eq!(draft.original_request, "First request");
        assert_eq!(draft.task.chars().count(), 4001);
        assert_eq!(draft.last_progress.chars().count(), 6001);
        assert!(draft.task.ends_with('…'));
        assert!(draft.last_progress.ends_with('…'));
        assert!(!draft.last_progress.contains("Not an agent"));
        let mut rows = events.to_vec();
        rows.push(serde_json::json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","content":[{"type":"input_text","text":"Actual first user task"}]}}}));
        rows.push(serde_json::json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","content":[{"type":"text","text":"Actual latest task"},{"type":"input_text","text":"Verify it"}]}}}));
        fs::write(
            &rollout,
            rows.iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        read_settings(temp.path(), &rollout, temp.path(), &mut thread).unwrap();
        let draft = thread.draft.as_ref().unwrap();
        assert_eq!(draft.original_request, "Actual first user task");
        assert_eq!(draft.task, "Actual latest task\nVerify it");
        assert!(injected_user_message(
            "<future_context>unknown future injection</future_context>"
        ));
        assert!(!injected_user_message(
            "<example>Use this as my task, with further text"
        ));
    }
}
