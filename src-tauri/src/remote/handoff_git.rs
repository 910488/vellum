//! Local Git plumbing for handoff. The real index and worktree are never written.
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use super::{process::background_command, ssh_trust};
use crate::error::{AppError, AppResult};
use serde::{Deserialize, Serialize};

fn error(message: impl Into<String>) -> AppError {
    AppError::Message(message.into())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Change {
    pub path: String,
    pub staged: bool,
    pub untracked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub base: String,
    pub commit: String,
    pub local_ref: String,
    pub changes: Vec<Change>,
}

pub(crate) struct Git {
    pub repo: PathBuf,
    hooks: PathBuf,
}
impl Git {
    pub fn new(repo: &Path, data_root: &Path) -> Self {
        Self {
            repo: repo.into(),
            hooks: data_root.join("remote-handoffs/empty-hooks"),
        }
    }
    fn command(&self) -> Command {
        let mut cmd = background_command("git");
        for (name, _) in
            std::env::vars_os().filter(|(name, _)| name.to_string_lossy().starts_with("GIT_"))
        {
            cmd.env_remove(name);
        }
        cmd.env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .env("GIT_CONFIG_COUNT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_AUTHOR_NAME", "Vellum Handoff")
            .env("GIT_AUTHOR_EMAIL", "handoff@vellum.invalid")
            .env("GIT_COMMITTER_NAME", "Vellum Handoff")
            .env("GIT_COMMITTER_EMAIL", "handoff@vellum.invalid")
            .args([
                "--no-optional-locks",
                "-c",
                "commit.gpgSign=false",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "protocol.allow=never",
                "-c",
            ])
            .arg(format!("core.hooksPath={}", self.hooks.display()))
            .arg("-C")
            .arg(&self.repo);
        cmd
    }
    pub fn run(&self, args: &[&str], index: Option<&Path>) -> AppResult<String> {
        let mut cmd = self.command();
        cmd.args(args);
        if let Some(index) = index {
            cmd.env("GIT_INDEX_FILE", index);
        }
        let output = run(cmd)?;
        String::from_utf8(output.stdout).map_err(|e| error(format!("HandoffGitInvalidUtf8: {e}")))
    }
    pub fn inspect(&self) -> AppResult<(String, Vec<Change>)> {
        let version = self.run(&["version"], None)?;
        let mut numbers = version
            .strip_prefix("git version ")
            .unwrap_or_default()
            .split('.');
        let supported = matches!((numbers.next().and_then(|v| v.parse::<u32>().ok()), numbers.next().and_then(|v| v.parse::<u32>().ok())),
            (Some(major), Some(minor)) if major > 2 || major == 2 && minor >= 29);
        if !supported {
            return Err(error(
                "HandoffGitVersionUnsupported: upgrade local Git to 2.29 or newer",
            ));
        }
        let top = self.run(&["rev-parse", "--show-toplevel"], None)?;
        if std::fs::canonicalize(top.trim()).map_err(|e| error(e.to_string()))?
            != std::fs::canonicalize(&self.repo).map_err(|e| error(e.to_string()))?
        {
            return Err(error(
                "HandoffRepositoryChanged: repo root no longer matches thread",
            ));
        }
        let base = self
            .run(&["rev-parse", "--verify", "HEAD^{commit}"], None)?
            .trim()
            .to_owned();
        if !valid_commit(&base) {
            return Err(error("HandoffSnapshotInvalid"));
        }
        if self
            .run(&["rev-parse", "--is-shallow-repository"], None)?
            .trim()
            == "true"
        {
            return Err(error("HandoffShallowCloneUnsupported: fetch the complete repository history before handoff"));
        }
        // An external clean filter can run arbitrary commands while staging.
        // Refuse configured filters rather than executing them in preflight or
        // silently changing what the temporary index captures.
        let config = self.run(&["config", "--includes", "--list"], None)?;
        if config.lines().any(|line| line.starts_with("filter.")) {
            return Err(error(
                "HandoffGitFiltersUnsupported: repository clean filters need explicit support",
            ));
        }
        let status = self.run(&["status", "--porcelain=v1", "--no-renames", "-z"], None)?;
        let changes = status
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(|entry| {
                let path = entry
                    .get(3..)
                    .ok_or_else(|| error("HandoffGitStatusInvalid"))?;
                let bytes = entry.as_bytes();
                Ok(Change {
                    path: path.into(),
                    staged: bytes[0] != b' ' && bytes[0] != b'?',
                    untracked: entry.starts_with("??"),
                })
            })
            .collect::<AppResult<Vec<_>>>()?;
        Ok((base, changes))
    }
    pub fn snapshot(&self, id: &str) -> AppResult<Snapshot> {
        if !super::handoff::valid_id(id) {
            return Err(error("HandoffIdInvalid"));
        }
        let (base, changes) = self.inspect()?;
        let temporary = tempfile::tempdir().map_err(|e| error(e.to_string()))?;
        let index = temporary.path().join("index");
        self.run(&["read-tree", &base], Some(&index))?;
        self.run(&["add", "-A", "--", "."], Some(&index))?;
        let tree = self.run(&["write-tree"], Some(&index))?;
        let commit = self
            .run(
                &[
                    "commit-tree",
                    tree.trim(),
                    "-p",
                    &base,
                    "-m",
                    "Vellum handoff snapshot",
                ],
                None,
            )?
            .trim()
            .to_owned();
        if !valid_commit(&commit) {
            return Err(error("HandoffSnapshotInvalid"));
        }
        let local_ref = format!("refs/vellum-handoff/{id}");
        self.run(
            &["update-ref", &local_ref, &commit, &"0".repeat(commit.len())],
            None,
        )?;
        Ok(Snapshot {
            base,
            commit,
            local_ref,
            changes,
        })
    }
    pub fn push(
        &self,
        data_root: &Path,
        destination: &str,
        remote_repo: &str,
        snapshot: &Snapshot,
        remote_ref: &str,
    ) -> AppResult<()> {
        let target = ssh_trust::resolve_ssh_target(destination)?;
        ssh_trust::require_trust_or_error(data_root, &target)?;
        // Borrow source objects, not its Git configuration. Repository-local
        // url.*.insteadOf and remote helpers must never redirect this push.
        let temporary = tempfile::tempdir().map_err(|e| error(e.to_string()))?;
        let bare = temporary.path().join("transport.git");
        let format = if snapshot.commit.len() == 64 {
            "sha256"
        } else {
            "sha1"
        };
        self.run(
            &[
                "init",
                "--bare",
                "--template=",
                &format!("--object-format={format}"),
                bare.to_str()
                    .ok_or_else(|| error("HandoffGitInvalidUtf8"))?,
            ],
            None,
        )?;
        let objects = self.run(&["rev-parse", "--git-path", "objects"], None)?;
        let objects = std::fs::canonicalize(self.repo.join(objects.trim()))
            .map_err(|e| error(e.to_string()))?;
        let mut cmd = self.push_command(
            data_root,
            destination,
            remote_repo,
            snapshot,
            remote_ref,
            Some((&bare, &objects)),
        )?;
        run_command(&mut cmd)?;
        Ok(())
    }
    fn push_command(
        &self,
        data_root: &Path,
        destination: &str,
        remote_repo: &str,
        snapshot: &Snapshot,
        remote_ref: &str,
        isolation: Option<(&Path, &Path)>,
    ) -> AppResult<Command> {
        // Only cached managed aliases, never a renderer-provided URL. Git itself
        // quotes the SCP-style repository path for the remote receive-pack.
        if destination.is_empty()
            || destination.starts_with('-')
            || !destination
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            || !remote_repo.starts_with('/')
            || remote_repo.contains(['\0', '\n', '\r', '\\'])
            || !valid_commit(&snapshot.commit)
            || !remote_ref.starts_with("refs/vellum-handoff/")
            || !super::handoff::valid_id(remote_ref.trim_start_matches("refs/vellum-handoff/"))
        {
            return Err(error("HandoffTransportTargetInvalid"));
        }
        let mut ssh_args = vec![
            "ssh".to_string(),
            "-T".into(),
            "-o".into(),
            "BatchMode=yes".into(),
            "-o".into(),
            "ConnectTimeout=10".into(),
            "-o".into(),
            "ServerAliveInterval=10".into(),
            "-o".into(),
            "ServerAliveCountMax=2".into(),
        ];
        ssh_args.extend(
            ssh_trust::strict_host_key_args(data_root)
                .into_iter()
                .map(|option| {
                    if let Some(path) = option.strip_prefix("UserKnownHostsFile=") {
                        let path = if cfg!(windows) {
                            path.replace('\\', "/")
                        } else {
                            path.to_owned()
                        };
                        format!(
                            "UserKnownHostsFile=\"{}\"",
                            path.replace('\\', "\\\\").replace('"', "\\\"")
                        )
                    } else {
                        option
                    }
                }),
        );
        let ssh = ssh_args
            .iter()
            .map(|arg| quote(arg))
            .collect::<Vec<_>>()
            .join(" ");
        let mut cmd = self.command();
        if let Some((directory, objects)) = isolation {
            cmd.arg("--git-dir")
                .arg(directory)
                .env("GIT_OBJECT_DIRECTORY", objects);
        }
        cmd.env("GIT_SSH_COMMAND", ssh)
            .env("GIT_SSH_VARIANT", "ssh")
            .env("GIT_ALLOW_PROTOCOL", "ssh")
            .args([
                "-c",
                "protocol.ssh.allow=always",
                "push",
                "--porcelain",
                "--no-verify",
                "--",
            ])
            .arg(format!("{destination}:{remote_repo}"))
            .arg(format!("{}:{remote_ref}", snapshot.commit));
        Ok(cmd)
    }
}

pub(crate) fn valid_commit(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|b| b.is_ascii_hexdigit())
}
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn run(mut command: Command) -> AppResult<Output> {
    run_command(&mut command)
}
fn run_command(command: &mut Command) -> AppResult<Output> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| error(format!("HandoffGitUnavailable: {e}")))?;
    // Drain both pipes while waiting, including pushes large enough to fill an
    // OS pipe. Bound retained bytes and total process time.
    let reader = |stream: Box<dyn Read + Send>| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut stream = stream;
            let mut bytes = Vec::new();
            let mut oversized = false;
            let mut chunk = [0u8; 8192];
            let result = loop {
                match stream.read(&mut chunk) {
                    Ok(0) => {
                        break if oversized {
                            Err(std::io::Error::other("handoff Git output exceeds 32 MiB"))
                        } else {
                            Ok(bytes)
                        }
                    }
                    Ok(n) => {
                        if bytes.len() + n <= 32 * 1024 * 1024 {
                            bytes.extend_from_slice(&chunk[..n]);
                        } else {
                            oversized = true;
                        }
                    }
                    Err(e) => break Err(e),
                }
            };
            let _ = tx.send(result);
        });
        rx
    };
    let stdout = reader(Box::new(
        child
            .stdout
            .take()
            .ok_or_else(|| error("HandoffGitPipeMissing"))?,
    ));
    let stderr = reader(Box::new(
        child
            .stderr
            .take()
            .ok_or_else(|| error("HandoffGitPipeMissing"))?,
    ));
    let deadline = Instant::now() + Duration::from_secs(120);
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| error(e.to_string()))? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error("HandoffGitTimeout"));
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let collect = |rx: std::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>| -> AppResult<Vec<u8>> {
        rx.recv_timeout(Duration::from_secs(2))
            .map_err(|e| error(e.to_string()))?
            .map_err(|e| error(e.to_string()))
    };
    let output = Output {
        status,
        stdout: collect(stdout)?,
        stderr: collect(stderr)?,
    };
    if !status.success() {
        return Err(error(format!(
            "HandoffGitFailed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn snapshot_keeps_real_index_worktree_and_ignored_files() {
        let temp = tempfile::tempdir().unwrap();
        let git = Git::new(temp.path(), temp.path());
        git.run(&["init"], None).unwrap();
        std::fs::write(temp.path().join("file"), "base\n").unwrap();
        std::fs::write(temp.path().join(".gitignore"), "ignored\n").unwrap();
        git.run(&["add", "."], None).unwrap();
        git.run(&["commit", "-m", "base"], None).unwrap();
        std::fs::write(temp.path().join("file"), "staged\n").unwrap();
        git.run(&["add", "."], None).unwrap();
        std::fs::write(temp.path().join("file"), "unstaged\n").unwrap();
        std::fs::write(temp.path().join("untracked"), "new\n").unwrap();
        std::fs::write(temp.path().join("ignored"), "private\n").unwrap();
        let before = git.run(&["status", "--porcelain=v1", "-z"], None).unwrap();
        let index = std::fs::read(temp.path().join(".git/index")).unwrap();
        let snapshot = git.snapshot("operation-1").unwrap();
        assert_eq!(
            std::fs::read(temp.path().join(".git/index")).unwrap(),
            index
        );
        assert_eq!(
            git.run(&["status", "--porcelain=v1", "-z"], None).unwrap(),
            before
        );
        assert_eq!(
            git.run(&["show", &format!("{}:file", snapshot.commit)], None)
                .unwrap(),
            "unstaged\n"
        );
        let files = git
            .run(&["ls-tree", "--name-only", &snapshot.commit], None)
            .unwrap();
        assert!(files.lines().any(|p| p == "untracked"));
        assert!(!files.lines().any(|p| p == "ignored"));
    }
    #[test]
    fn push_is_pinned_to_trusted_ssh_and_an_explicit_ref_without_force() {
        let git = Git::new(Path::new("repo"), Path::new("data"));
        let snapshot = Snapshot {
            base: "a".repeat(40),
            commit: "b".repeat(40),
            local_ref: String::new(),
            changes: vec![],
        };
        let cmd = git
            .push_command(
                Path::new("data"),
                "vellum-remote-test",
                "/home/user/a repo.git",
                &snapshot,
                "refs/vellum-handoff/operation-1",
                Some((Path::new("isolated.git"), Path::new("source-objects"))),
            )
            .unwrap();
        let args: Vec<_> = cmd
            .get_args()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect();
        assert!(args.contains(&"vellum-remote-test:/home/user/a repo.git".into()));
        assert!(args
            .windows(2)
            .any(|pair| pair[0] == "--git-dir" && pair[1] == "isolated.git"));
        assert!(!args.iter().any(|arg| arg.contains("--force")));
        let ssh = cmd
            .get_envs()
            .find(|(k, _)| *k == "GIT_SSH_COMMAND")
            .unwrap()
            .1
            .unwrap()
            .to_string_lossy();
        assert!(ssh.contains("StrictHostKeyChecking=yes"));
        assert!(ssh.contains("known_hosts"));
        assert!(git
            .push_command(
                Path::new("data"),
                "-oProxyCommand=bad",
                "/repo",
                &snapshot,
                "refs/vellum-handoff/x",
                None
            )
            .is_err());
    }
}
