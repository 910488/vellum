//! Handoff filesystem/Git boundary. Repositories and worktrees are exclusively
//! below AgentPaths/handoffs; no renderer-supplied paths or shell commands.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::Value;

use crate::handoff::{private_dir, valid_commit, Capture, Client, FileStat, Layout};

pub(crate) struct NativeClient {
    home: PathBuf,
}
impl NativeClient {
    pub fn new(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
        }
    }
}

impl Client for NativeClient {
    fn preflight(&self) -> Result<(), String> {
        let installed =
            crate::codex_package::inspect_installed(&self.home.join("packages/standalone/current"));
        if !installed.complete {
            return Err(format!(
                "codexRuntimeIncomplete: redeploy this host before handoff; missing {}",
                installed.missing.join(", ")
            ));
        }
        let native = crate::native_codex::discover_native()?;
        if !native.compatible
            || !native.daemon_running
            || !native.restart_safe
            || fs::canonicalize(&native.codex_home).map_err(|e| e.to_string())?
                != fs::canonicalize(&self.home).map_err(|e| e.to_string())?
        {
            return Err(
                "HandoffDaemonUnavailable: deploy a compatible managed Codex daemon first".into(),
            );
        }
        Ok(())
    }
    fn prepare_repo(&self, layout: &Layout) -> Result<(), String> {
        private_dir(&layout.root)?;
        private_dir(&layout.root.join("repos"))?;
        private_dir(&layout.root.join("work"))?;
        private_dir(&layout.root.join("empty-hooks"))?;
        if layout.repo.try_exists().map_err(|e| e.to_string())? {
            crate::update::reject_symlink(&layout.repo)?;
            if git(
                layout,
                &layout.repo,
                &["rev-parse", "--is-bare-repository"],
                None,
            )?
            .trim()
                != "true"
            {
                return Err("HandoffRepositoryInvalid: expected a managed bare repository".into());
            }
            if git(
                layout,
                &layout.repo,
                &["rev-parse", "--show-object-format"],
                None,
            )?
            .trim()
                != layout.object_format
            {
                return Err("HandoffRepositoryInvalid: snapshot object format changed".into());
            }
        } else {
            git(
                layout,
                &layout.root,
                &[
                    "init",
                    "--bare",
                    &format!("--object-format={}", layout.object_format),
                    "--",
                    posix(&layout.repo)?,
                ],
                None,
            )?;
            private_dir(&layout.repo)?;
        }
        Ok(())
    }
    fn prepare_worktree(&self, layout: &Layout, snapshot: &str) -> Result<(), String> {
        crate::update::reject_symlink(&layout.worktree)?;
        let observed = git(
            layout,
            &layout.repo,
            &[
                "rev-parse",
                "--verify",
                &format!("{}^{{commit}}", layout.snapshot_ref),
            ],
            None,
        )?;
        if observed.trim() != snapshot {
            return Err(
                "HandoffSnapshotMismatch: push the exact snapshot ref before starting".into(),
            );
        }
        if !layout.worktree.try_exists().map_err(|e| e.to_string())? {
            git(
                layout,
                &layout.repo,
                &[
                    "worktree",
                    "add",
                    "--detach",
                    "--",
                    posix(&layout.worktree)?,
                    snapshot,
                ],
                None,
            )?;
        }
        verify_worktree(layout)?;
        if git(layout, &layout.worktree, &["rev-parse", "HEAD"], None)?.trim() != snapshot {
            return Err("HandoffSnapshotMismatch: prepared worktree HEAD changed".into());
        }
        if !git(
            layout,
            &layout.worktree,
            &["status", "--porcelain=v1", "-z"],
            None,
        )?
        .is_empty()
        {
            return Err("HandoffSnapshotMismatch: worktree changed before the first turn".into());
        }
        Ok(())
    }
    fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let response = crate::native_session::ws_jsonrpc_call(&self.home, method, params)?;
        if let Some(error) = response.get("error") {
            return Err(format!("HandoffDaemonRpcError: {method}: {error}"));
        }
        response
            .get("result")
            .cloned()
            .ok_or_else(|| format!("HandoffResponseInvalid: {method} has no result"))
    }
    fn capture(
        &self,
        layout: &Layout,
        snapshot: &str,
        result_ref: &str,
        partial: bool,
    ) -> Result<Capture, String> {
        verify_worktree(layout)?;
        let untracked = git(
            layout,
            &layout.worktree,
            &["ls-files", "--others", "--exclude-standard", "-z"],
            None,
        )?;
        let excluded: Vec<_> = untracked
            .split('\0')
            .filter(|p| !p.is_empty() && cache_file(p))
            .collect();
        let probe = git_output(
            layout,
            &layout.repo,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{result_ref}^{{commit}}"),
            ],
            None,
        )?;
        let commit = if probe.status.success() {
            text(probe.stdout)?.trim().to_owned()
        } else if probe.status.code() == Some(1) {
            let temporary = tempfile::tempdir_in(&layout.root).map_err(|e| e.to_string())?;
            let index = temporary.path().join("index");
            git(
                layout,
                &layout.worktree,
                &["read-tree", "HEAD"],
                Some(&index),
            )?;
            git(
                layout,
                &layout.worktree,
                &["add", "-A", "--", "."],
                Some(&index),
            )?;
            for path in &excluded {
                git(
                    layout,
                    &layout.worktree,
                    &["update-index", "--force-remove", "--", path],
                    Some(&index),
                )?;
            }
            let tree = git(layout, &layout.worktree, &["write-tree"], Some(&index))?;
            let commit = git(
                layout,
                &layout.repo,
                &[
                    "commit-tree",
                    tree.trim(),
                    "-p",
                    snapshot,
                    "-m",
                    "Vellum remote handoff result",
                ],
                None,
            )?;
            git(
                layout,
                &layout.repo,
                &[
                    "update-ref",
                    result_ref,
                    commit.trim(),
                    &"0".repeat(snapshot.len()),
                ],
                None,
            )?;
            commit.trim().to_owned()
        } else {
            return Err(git_error(&probe));
        };
        if !valid_commit(&commit) {
            return Err("HandoffResultInvalid: invalid commit".into());
        }
        let parent = git(
            layout,
            &layout.repo,
            &["rev-parse", &format!("{commit}^1")],
            None,
        )?;
        if parent.trim() != snapshot {
            return Err(
                "HandoffResultInvalid: result does not descend directly from snapshot".into(),
            );
        }
        let stat = git_output(
            layout,
            &layout.repo,
            &[
                "diff",
                "--no-renames",
                "--numstat",
                "-z",
                snapshot,
                &commit,
                "--",
            ],
            None,
        )?;
        if !stat.status.success() {
            return Err(git_error(&stat));
        }
        Ok(Capture {
            excluded_cache_files: excluded.len(),
            result_ref: result_ref.into(),
            commit,
            snapshot_commit: snapshot.into(),
            partial,
            diffstat: parse_diffstat(&stat.stdout)?,
        })
    }
}

fn cache_file(path: &str) -> bool {
    path.split('/').any(|part| {
        matches!(
            part,
            "__pycache__" | "node_modules" | ".pytest_cache" | "target" | ".DS_Store"
        )
    }) || path.ends_with(".pyc")
}

fn posix(path: &Path) -> Result<&str, String> {
    path.to_str()
        .ok_or_else(|| "HandoffPathInvalid: non-UTF-8 managed path".into())
}
fn text(bytes: Vec<u8>) -> Result<String, String> {
    String::from_utf8(bytes).map_err(|_| "HandoffGitInvalidUtf8".into())
}
fn git_error(output: &Output) -> String {
    format!(
        "HandoffGitFailed: {}",
        String::from_utf8_lossy(&output.stderr)
    )
}

fn git_output(
    layout: &Layout,
    cwd: &Path,
    args: &[&str],
    index: Option<&Path>,
) -> Result<Output, String> {
    let mut command = Command::new("git");
    // Do not inherit Git routing, index overrides, global filters, helpers or
    // hooks from the Agent's SSH environment. No network transport is needed.
    for (name, _) in
        std::env::vars_os().filter(|(name, _)| name.to_string_lossy().starts_with("GIT_"))
    {
        command.env_remove(name);
    }
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_CONFIG_COUNT", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "Vellum Handoff")
        .env("GIT_AUTHOR_EMAIL", "handoff@vellum.invalid")
        .env("GIT_COMMITTER_NAME", "Vellum Handoff")
        .env("GIT_COMMITTER_EMAIL", "handoff@vellum.invalid")
        .arg("-c")
        .arg(format!(
            "core.hooksPath={}",
            layout.root.join("empty-hooks").display()
        ))
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "protocol.allow=never",
            "-C",
        ])
        .arg(cwd)
        .args(args)
        .stdin(Stdio::null());
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    command
        .output()
        .map_err(|e| format!("HandoffGitUnavailable: {e}"))
}
fn git(layout: &Layout, cwd: &Path, args: &[&str], index: Option<&Path>) -> Result<String, String> {
    let output = git_output(layout, cwd, args, index)?;
    if !output.status.success() {
        return Err(git_error(&output));
    }
    text(output.stdout)
}
fn verify_worktree(layout: &Layout) -> Result<(), String> {
    for directory in [
        layout.root.clone(),
        layout.root.join("repos"),
        layout.root.join("work"),
        layout.repo.clone(),
    ] {
        if !fs::symlink_metadata(directory).is_ok_and(|metadata| metadata.is_dir()) {
            return Err("HandoffPathUnsafe: missing or linked repository directory".into());
        }
    }
    crate::update::reject_symlink(&layout.worktree)?;
    let top = git(
        layout,
        &layout.worktree,
        &["rev-parse", "--show-toplevel"],
        None,
    )?;
    let common = git(
        layout,
        &layout.worktree,
        &["rev-parse", "--git-common-dir"],
        None,
    )?;
    let common = layout.worktree.join(common.trim());
    if fs::canonicalize(top.trim()).map_err(|e| e.to_string())?
        != fs::canonicalize(&layout.worktree).map_err(|e| e.to_string())?
        || fs::canonicalize(common).map_err(|e| e.to_string())?
            != fs::canonicalize(&layout.repo).map_err(|e| e.to_string())?
    {
        return Err("HandoffWorktreeOwnershipMismatch".into());
    }
    Ok(())
}

fn parse_diffstat(bytes: &[u8]) -> Result<Vec<FileStat>, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "HandoffGitInvalidUtf8")?;
    text.split('\0')
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            let mut fields = entry.splitn(3, '\t');
            let count = |value: Option<&str>| -> Result<Option<u64>, String> {
                match value {
                    Some("-") => Ok(None),
                    Some(value) => value
                        .parse()
                        .map(Some)
                        .map_err(|_| "HandoffDiffstatInvalid".into()),
                    None => Err("HandoffDiffstatInvalid".into()),
                }
            };
            let added = count(fields.next())?;
            let deleted = count(fields.next())?;
            let path = fields
                .next()
                .filter(|p| !p.is_empty())
                .ok_or("HandoffDiffstatInvalid")?;
            Ok(FileStat {
                path: path.into(),
                added,
                deleted,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn native_client_uses_daemon_websocket_handshake_and_preserves_rpc_errors() {
        use futures_util::{SinkExt, StreamExt};
        use serde_json::json;
        use tokio_tungstenite::tungstenite::Message;
        // Keep the Unix socket below the platform pathname length limit.
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        fs::create_dir(temp.path().join("app-server-control")).unwrap();
        let listener = tokio::net::UnixListener::bind(
            temp.path()
                .join("app-server-control/app-server-control.sock"),
        )
        .unwrap();
        let server = tokio::spawn(async move {
            for expected in ["thread/start", "turn/start"] {
                let (stream, _) = listener.accept().await.unwrap();
                let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                let init: Value =
                    serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap())
                        .unwrap();
                assert_eq!(init["method"], "initialize");
                assert_eq!(init["params"]["capabilities"]["experimentalApi"], true);
                socket
                    .send(Message::Text(
                        json!({"id": 1, "result": {}}).to_string().into(),
                    ))
                    .await
                    .unwrap();
                let initialized: Value =
                    serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap())
                        .unwrap();
                assert_eq!(initialized["method"], "initialized");
                let request: Value =
                    serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap())
                        .unwrap();
                assert_eq!(request["method"], expected);
                let response = if expected == "thread/start" {
                    json!({"id": 2, "result": {"thread": {"id": "remote-thread"}}})
                } else {
                    json!({"id": 2, "error": {"code": -32602, "message": "injected policy refusal"}})
                };
                socket
                    .send(Message::Text(response.to_string().into()))
                    .await
                    .unwrap();
            }
        });
        let home = temp.path().to_path_buf();
        let calls = tokio::task::spawn_blocking(move || {
            let client = NativeClient::new(&home);
            assert_eq!(
                client.call("thread/start", json!({})).unwrap()["thread"]["id"],
                "remote-thread"
            );
            assert!(client
                .call("turn/start", json!({}))
                .unwrap_err()
                .contains("injected policy refusal"));
        });
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            calls.await.unwrap();
            server.await.unwrap();
        })
        .await
        .unwrap();
    }

    #[test]
    fn result_snapshot_preserves_real_worktree_index_and_ignored_files() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("handoffs");
        let layout = Layout {
            repo: root.join("repos/repo.git"),
            worktree: root.join("work/handoff"),
            snapshot_ref: "refs/vellum-handoff/handoff".into(),
            object_format: "sha1",
            root,
        };
        let client = NativeClient::new(temp.path());
        client.prepare_repo(&layout).unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("file.txt"), "base\n").unwrap();
        fs::write(source.join(".gitignore"), "*.secret\n").unwrap();
        let index = temp.path().join("seed-index");
        let repo = posix(&layout.repo).unwrap();
        let source_path = posix(&source).unwrap();
        git(
            &layout,
            &source,
            &[
                "--git-dir",
                repo,
                "--work-tree",
                source_path,
                "read-tree",
                "--empty",
            ],
            Some(&index),
        )
        .unwrap();
        git(
            &layout,
            &source,
            &[
                "--git-dir",
                repo,
                "--work-tree",
                source_path,
                "add",
                "-A",
                "--",
                ".",
            ],
            Some(&index),
        )
        .unwrap();
        let tree = git(&layout, &layout.repo, &["write-tree"], Some(&index)).unwrap();
        let base = git(
            &layout,
            &layout.repo,
            &["commit-tree", tree.trim(), "-m", "handoff seed"],
            None,
        )
        .unwrap();
        let base = base.trim();
        git(
            &layout,
            &layout.repo,
            &["update-ref", &layout.snapshot_ref, base],
            None,
        )
        .unwrap();
        client.prepare_worktree(&layout, base).unwrap();

        // Preserve files deliberately tracked by a remote commit even when
        // today's .gitignore would hide them from an index based on the seed.
        fs::write(
            layout.worktree.join("committed.secret"),
            "tracked remotely\n",
        )
        .unwrap();
        git(
            &layout,
            &layout.worktree,
            &["add", "-f", "--", "committed.secret"],
            None,
        )
        .unwrap();
        git(
            &layout,
            &layout.worktree,
            &["commit", "-m", "remote commit"],
            None,
        )
        .unwrap();
        fs::write(layout.worktree.join("file.txt"), "staged\n").unwrap();
        git(&layout, &layout.worktree, &["add", "--", "file.txt"], None).unwrap();
        fs::write(layout.worktree.join("file.txt"), "unstaged\n").unwrap();
        fs::write(layout.worktree.join("new.txt"), "untracked\n").unwrap();
        for file in [
            "__pycache__/fizz.pyc",
            "cache.pyc",
            "node_modules/pkg/cache",
            ".pytest_cache/cache",
            "target/cache",
            ".DS_Store",
        ] {
            let path = layout.worktree.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "untracked cache").unwrap();
        }
        fs::write(layout.worktree.join("tracked.pyc"), "tracked cache").unwrap();
        git(
            &layout,
            &layout.worktree,
            &["add", "--", "tracked.pyc"],
            None,
        )
        .unwrap();
        fs::write(layout.worktree.join("ignored.secret"), "never transfer\n").unwrap();
        let index_path = git(
            &layout,
            &layout.worktree,
            &["rev-parse", "--git-path", "index"],
            None,
        )
        .unwrap();
        let index_path = layout.worktree.join(index_path.trim());
        let before_status = git(
            &layout,
            &layout.worktree,
            &["status", "--porcelain=v1", "-z"],
            None,
        )
        .unwrap();
        let result_ref = "refs/vellum-handoff-results/handoff/capture";
        let before_index = fs::read(&index_path).unwrap();
        let capture = client.capture(&layout, base, result_ref, false).unwrap();
        assert_eq!(capture.excluded_cache_files, 6);
        assert!(capture
            .diffstat
            .iter()
            .any(|file| file.path == "tracked.pyc"));
        assert!(!capture
            .diffstat
            .iter()
            .any(|file| file.path != "tracked.pyc" && cache_file(&file.path)));
        assert_eq!(fs::read(&index_path).unwrap(), before_index);
        assert_eq!(
            git(
                &layout,
                &layout.worktree,
                &["status", "--porcelain=v1", "-z"],
                None
            )
            .unwrap(),
            before_status
        );
        assert_eq!(
            fs::read_to_string(layout.worktree.join("file.txt")).unwrap(),
            "unstaged\n"
        );
        assert_eq!(
            fs::read_to_string(layout.worktree.join("ignored.secret")).unwrap(),
            "never transfer\n"
        );
        assert_eq!(
            git(
                &layout,
                &layout.repo,
                &["show", &format!("{}:file.txt", capture.commit)],
                None
            )
            .unwrap(),
            "unstaged\n"
        );
        assert!(capture
            .diffstat
            .iter()
            .any(|file| file.path == "committed.secret"));
        assert!(capture.diffstat.iter().any(|file| file.path == "new.txt"));
        assert!(!capture
            .diffstat
            .iter()
            .any(|file| file.path == "ignored.secret"));
        fs::write(layout.worktree.join("file.txt"), "later change\n").unwrap();
        assert_eq!(
            client.capture(&layout, base, result_ref, false).unwrap(),
            capture
        );
        let foreign = Layout {
            root: layout.root.clone(),
            repo: layout.root.join("repos/other.git"),
            worktree: layout.worktree.clone(),
            snapshot_ref: layout.snapshot_ref.clone(),
            object_format: "sha1",
        };
        client.prepare_repo(&foreign).unwrap();
        assert!(client
            .capture(&foreign, base, result_ref, false)
            .unwrap_err()
            .contains("HandoffWorktreeOwnershipMismatch"));
        #[cfg(unix)]
        {
            fs::rename(layout.root.join("repos"), layout.root.join("real-repos")).unwrap();
            std::os::unix::fs::symlink("real-repos", layout.root.join("repos")).unwrap();
            assert!(client
                .capture(&layout, base, result_ref, false)
                .unwrap_err()
                .contains("HandoffPathUnsafe"));
        }
    }
    #[test]
    fn nul_diffstat_keeps_unusual_names_and_binary_changes() {
        let stat =
            parse_diffstat(b"2\t1\tpath with\ttab\nand newline\0-\t-\tbinary.dat\0").unwrap();
        assert_eq!(stat[0].path, "path with\ttab\nand newline");
        assert_eq!(stat[0].added, Some(2));
        assert_eq!(stat[1].added, None);
        assert!(parse_diffstat(b"bad\t1\tfile\0").is_err());
    }
}
