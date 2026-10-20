//! Folders of recent Claude Desktop Code sessions, so the card can offer a
//! choice instead of a typed path. Reads Desktop's own session index and keeps
//! only `cwd`, the archive flag and the activity time; titles, permissions and
//! transcripts are never read into Vellum.
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

const LIMIT: usize = 12;
/// Session index entries are a few KB; anything larger is not one of them.
const MAX_ENTRY_BYTES: u64 = 256 * 1024;

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RecentProject {
    pub path: String,
    pub sessions: u32,
    pub last_activity_at: Option<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionEntry {
    cwd: Option<String>,
    #[serde(default)]
    is_archived: bool,
    last_activity_at: Option<i64>,
}

pub fn recent_projects() -> Vec<RecentProject> {
    dirs::config_dir()
        .map(|dir| collect(&dir.join("Claude").join("claude-code-sessions")))
        .unwrap_or_default()
}

/// `<root>/<account>/<org>/local_<session>.json`, as Desktop lays it out.
fn collect(root: &Path) -> Vec<RecentProject> {
    let mut projects: HashMap<String, RecentProject> = HashMap::new();
    for entry in children(root)
        .flat_map(|account| children(&account))
        .flat_map(|org| children(&org))
    {
        let is_entry = entry
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("local_") && name.ends_with(".json"));
        let small = std::fs::metadata(&entry).is_ok_and(|meta| meta.len() <= MAX_ENTRY_BYTES);
        if !is_entry || !small {
            continue;
        }
        let Some(session) = std::fs::read(&entry)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<SessionEntry>(&bytes).ok())
        else {
            continue;
        };
        let Some(cwd) = session.cwd.filter(|cwd| !cwd.trim().is_empty()) else {
            continue;
        };
        if session.is_archived {
            continue;
        }
        let project = projects.entry(cwd.clone()).or_insert(RecentProject {
            path: cwd,
            sessions: 0,
            last_activity_at: None,
        });
        project.sessions += 1;
        project.last_activity_at = project.last_activity_at.max(session.last_activity_at);
    }
    let mut projects: Vec<_> = projects
        .into_values()
        .filter(|project| Path::new(&project.path).is_dir())
        .collect();
    projects.sort_by(|a, b| {
        b.last_activity_at
            .cmp(&a.last_activity_at)
            .then_with(|| a.path.cmp(&b.path))
    });
    projects.truncate(LIMIT);
    projects
}

fn children(dir: &Path) -> impl Iterator<Item = std::path::PathBuf> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn session(root: &Path, org: &str, id: &str, value: serde_json::Value) {
        let dir = root.join("account").join(org);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("local_{id}.json")), value.to_string()).unwrap();
    }

    #[test]
    fn groups_live_folders_by_recent_activity() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sessions");
        let alpha = temp.path().join("alpha");
        let beta = temp.path().join("beta");
        std::fs::create_dir_all(&alpha).unwrap();
        std::fs::create_dir_all(&beta).unwrap();
        let (alpha, beta) = (alpha.to_string_lossy(), beta.to_string_lossy());
        session(
            &root,
            "org",
            "1",
            json!({"cwd": alpha, "lastActivityAt": 10, "title": "x"}),
        );
        session(
            &root,
            "org",
            "2",
            json!({"cwd": alpha, "lastActivityAt": 30}),
        );
        session(
            &root,
            "org2",
            "3",
            json!({"cwd": beta, "lastActivityAt": 20}),
        );
        session(
            &root,
            "org",
            "4",
            json!({"cwd": beta, "lastActivityAt": 99, "isArchived": true}),
        );
        session(
            &root,
            "org",
            "5",
            json!({"cwd": temp.path().join("gone"), "lastActivityAt": 50}),
        );
        session(&root, "org", "6", json!({"lastActivityAt": 60}));
        std::fs::write(root.join("account/org/local_7.json"), "not json").unwrap();
        std::fs::write(
            root.join("account/org/other.json"),
            json!({"cwd": alpha}).to_string(),
        )
        .unwrap();

        assert_eq!(
            collect(&root),
            vec![
                RecentProject {
                    path: alpha.into(),
                    sessions: 2,
                    last_activity_at: Some(30)
                },
                RecentProject {
                    path: beta.into(),
                    sessions: 1,
                    last_activity_at: Some(20)
                },
            ]
        );
    }

    #[test]
    fn missing_index_is_empty() {
        let temp = tempfile::tempdir().unwrap();
        assert!(collect(&temp.path().join("absent")).is_empty());
    }
}
