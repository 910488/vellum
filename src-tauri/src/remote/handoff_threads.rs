//! Read-only discovery of local Codex threads; never open auth/config secrets.
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use super::handoff_git::Git;
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
    pub active: bool,
    pub handed_off: bool,
}

fn error(message: impl Into<String>) -> AppError {
    AppError::Message(message.into())
}

pub(crate) fn discover(home: &Path, data_root: &Path) -> AppResult<Vec<LocalThread>> {
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
        let modern = "SELECT id, cwd, COALESCE(NULLIF(name, ''), title), rollout_path, model_provider, updated_at FROM threads WHERE archived=0 ORDER BY updated_at DESC LIMIT 100";
        let legacy = "SELECT id, cwd, title, rollout_path, model_provider, updated_at FROM threads WHERE archived=0 ORDER BY updated_at DESC LIMIT 100";
        let mut statement = connection
            .prepare(modern)
            .or_else(|_| connection.prepare(legacy))
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
                ))
            })
            .map_err(|e| error(e.to_string()))?;
        for row in rows {
            let (id, cwd, title, rollout, provider, updated) =
                row.map_err(|e| error(e.to_string()))?;
            if !super::handoff::valid_id(&id) {
                continue;
            }
            let cwd = PathBuf::from(cwd);
            if !cwd.is_dir() {
                continue;
            }
            let provisional = Git::new(&cwd, data_root);
            let Ok(top) = provisional.run(&["rev-parse", "--show-toplevel"], None) else {
                continue;
            };
            let repo = fs::canonicalize(top.trim()).map_err(|e| error(e.to_string()))?;
            let git = Git::new(&repo, data_root);
            let branch = git
                .run(&["rev-parse", "--abbrev-ref", "HEAD"], None)
                .unwrap_or_else(|_| "unborn".into())
                .trim()
                .to_owned();
            let mut thread = LocalThread {
                thread_id: id,
                name: title,
                repo,
                branch,
                updated_at: updated,
                model: None,
                model_provider: Some(provider),
                effort: None,
                active: true,
                handed_off: false,
            };
            // Missing/unreadable rollout remains visible but cannot pass preflight.
            let _ = read_settings(home, Path::new(&rollout), &cwd, &mut thread);
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
    for line in BufReader::new(file).lines() {
        let line = line.map_err(|e| error(e.to_string()))?;
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let payload = &value["payload"];
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
    thread.active = active;
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
        assert_eq!(result[0].model.as_deref(), Some("model"));
        assert!(!result[0].active);
        assert_eq!(fs::read(db).unwrap(), before_db);
        assert_eq!(fs::read(rollout).unwrap(), before_rollout);
        assert_eq!(fs::read(repo.join(".git/index")).unwrap(), before_index);
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
            active: true,
            handed_off: false,
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
        assert!(!thread.active);
        thread.thread_id = "other-thread".into();
        assert!(read_settings(temp.path(), &rollout, temp.path(), &mut thread).is_err());
    }
}
