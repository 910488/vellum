//! Local Git plumbing. Snapshot/transport leave the index and worktree alone;
//! only explicit result application writes the worktree (never the index).
use std::io::{Read, Write};
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
    #[cfg(test)]
    system_config: Option<PathBuf>,
}
impl Git {
    pub fn new(repo: &Path, data_root: &Path) -> Self {
        Self {
            repo: repo.into(),
            hooks: data_root.join("remote-handoffs/empty-hooks"),
            #[cfg(test)]
            system_config: None,
        }
    }
    fn command(&self) -> Command {
        self.command_with_config(true)
    }
    fn command_with_config(&self, inherit: bool) -> Command {
        let mut cmd = background_command("git");
        for (name, _) in std::env::vars_os().filter(|(name, _)| {
            name.to_string_lossy()
                .to_ascii_uppercase()
                .starts_with("GIT_")
        }) {
            // Staging must use the same normalization and clean filters as a
            // user's git add. Clear routing overrides, preserving Git config.
            if !inherit
                || !name
                    .to_string_lossy()
                    .to_ascii_uppercase()
                    .starts_with("GIT_CONFIG")
            {
                cmd.env_remove(name);
            }
        }
        if !inherit {
            cmd.env("GIT_CONFIG_NOSYSTEM", "1")
                .env(
                    "GIT_CONFIG_GLOBAL",
                    if cfg!(windows) { "NUL" } else { "/dev/null" },
                )
                .env("GIT_CONFIG_COUNT", "0")
                .args(["-c", "protocol.allow=never"]);
        }
        #[cfg(test)]
        if inherit {
            if let Some(path) = &self.system_config {
                cmd.env_remove("GIT_CONFIG_NOSYSTEM")
                    .env("GIT_CONFIG_SYSTEM", path)
                    .env(
                        "GIT_CONFIG_GLOBAL",
                        if cfg!(windows) { "NUL" } else { "/dev/null" },
                    );
            }
        }
        cmd.env("GIT_OPTIONAL_LOCKS", "0")
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
                // builtin/diff.c::refresh_index_quietly takes a mandatory
                // index lock (Git 2.39), despite --no-optional-locks:
                // https://github.com/git/git/blob/v2.39.0/builtin/diff.c#L224-L236
                // This
                // disables its stat-only refresh, including in tree diffs.
                "diff.autoRefreshIndex=false",
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
    fn copied_index(&self) -> AppResult<(tempfile::TempDir, PathBuf)> {
        let temporary = tempfile::tempdir().map_err(|e| error(e.to_string()))?;
        let index = temporary.path().join("index");
        let source = self.run(&["rev-parse", "--git-path", "index"], None)?;
        match std::fs::copy(self.repo.join(source.trim()), &index) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.run(&["read-tree", "HEAD"], Some(&index))?;
            }
            Err(e) => return Err(error(format!("HandoffIndexUnavailable: {e}"))),
        }
        Ok((temporary, index))
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
        let (_temporary, index) = self.copied_index()?;
        self.require_no_lfs(Some(&index))?;
        let status = self.run(
            &["status", "--porcelain=v1", "--no-renames", "-z"],
            Some(&index),
        )?;
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
    fn require_no_lfs(&self, index: Option<&Path>) -> AppResult<()> {
        let files = self.run(
            &[
                "ls-files",
                "--cached",
                "--others",
                "--exclude-standard",
                "-z",
            ],
            index,
        )?;
        // Also catch an LFS rule that currently matches no files. Other clean
        // filters are the user's own Git configuration and remain enabled.
        for path in files.split('\0').filter(|path| {
            Path::new(path)
                .file_name()
                .is_some_and(|name| name == ".gitattributes")
        }) {
            let path_on_disk = self.repo.join(path);
            let attributes = match std::fs::read_to_string(&path_on_disk) {
                Ok(text) => text,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    self.run(&["show", &format!(":{path}")], index)?
                }
                Err(e) => return Err(error(format!("HandoffAttributesUnavailable: {e}"))),
            };
            if attributes
                .lines()
                .filter(|line| !line.trim_start().starts_with('#'))
                .any(|line| line.split_whitespace().any(|field| field == "filter=lfs"))
            {
                return Err(error("HandoffLfsUnsupported: LFS objects and filters are not transferred; use a repository without LFS for handoff"));
            }
        }
        // Git resolves nested rules, macros, info/attributes and global
        // attributes. stdin avoids argv limits and preserves unusual names.
        let mut command = self.command();
        if let Some(index) = index {
            command.env("GIT_INDEX_FILE", index);
        }
        command.args(["check-attr", "-z", "--stdin", "filter"]);
        let output = run_command_with_input(&mut command, Some(files.into_bytes()))?;
        let fields = output.stdout.split(|byte| *byte == 0).collect::<Vec<_>>();
        if fields
            .as_chunks::<3>()
            .0
            .iter()
            .any(|entry| entry[1] == b"filter" && entry[2] == b"lfs")
        {
            return Err(error("HandoffLfsUnsupported: LFS objects and filters are not transferred; use a repository without LFS for handoff"));
        }
        Ok(())
    }
    pub fn snapshot(&self, id: &str) -> AppResult<Snapshot> {
        if !super::handoff::valid_id(id) {
            return Err(error("HandoffIdInvalid"));
        }
        let (base, changes) = self.inspect()?;
        let temporary = tempfile::tempdir().map_err(|e| error(e.to_string()))?;
        let index = temporary.path().join("index");
        self.run(&["read-tree", &base], Some(&index))?;
        self.require_no_lfs(Some(&index))?;
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
        let mut initialize = self.command_with_config(false);
        initialize.args([
            "init",
            "--bare",
            "--template=",
            &format!("--object-format={format}"),
            bare.to_str()
                .ok_or_else(|| error("HandoffGitInvalidUtf8"))?,
        ]);
        run(initialize)?;
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
        pin_transport_target(&mut cmd, &target)?;
        run_command(&mut cmd)?;
        Ok(())
    }
    /// Remove only the ref we created, and only while it still names the
    /// recorded snapshot. Used on pre-transport journal failure and by the
    /// later apply/discard lifecycle; never remove somebody else's update.
    pub fn release_snapshot(&self, snapshot: &Snapshot) -> AppResult<()> {
        if !snapshot
            .local_ref
            .strip_prefix("refs/vellum-handoff/")
            .is_some_and(super::handoff::valid_id)
            || !valid_commit(&snapshot.commit)
        {
            return Err(error("HandoffSnapshotRefInvalid"));
        }
        self.release_ref(&snapshot.local_ref, &snapshot.commit)
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
        self.transport_command(
            data_root,
            destination,
            remote_repo,
            snapshot,
            remote_ref,
            isolation,
            false,
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn transport_command(
        &self,
        data_root: &Path,
        destination: &str,
        remote_repo: &str,
        snapshot: &Snapshot,
        remote_ref: &str,
        isolation: Option<(&Path, &Path)>,
        fetch: bool,
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
            || if fetch {
                !remote_ref
                    .strip_prefix("refs/vellum-handoff-results/")
                    .is_some_and(|tail| {
                        let parts = tail.split('/').collect::<Vec<_>>();
                        parts.len() == 2 && parts.iter().all(|part| super::handoff::valid_id(part))
                    })
            } else {
                !remote_ref
                    .strip_prefix("refs/vellum-handoff/")
                    .is_some_and(super::handoff::valid_id)
            }
        {
            return Err(error("HandoffTransportTargetInvalid"));
        }
        let mut ssh_args = vec![
            crate::process::resolve_ssh_tool("ssh")
                .to_string_lossy()
                .replace('\\', "/"),
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
        ssh_args.extend(super::ssh_added::config_args(data_root, destination));
        ssh_args.extend(ssh_trust::strict_host_key_args(data_root));
        let ssh = ssh_args
            .iter()
            .map(|arg| quote(arg))
            .collect::<Vec<_>>()
            .join(" ");
        let mut cmd = self.command_with_config(false);
        if let Some((directory, objects)) = isolation {
            cmd.arg("--git-dir")
                .arg(directory)
                .env("GIT_OBJECT_DIRECTORY", objects);
        }
        cmd.env("GIT_SSH_COMMAND", ssh)
            .env("GIT_SSH_VARIANT", "ssh")
            .env("GIT_ALLOW_PROTOCOL", "ssh")
            .args(["-c", "protocol.ssh.allow=always"]);
        if fetch {
            cmd.args([
                "fetch",
                "--no-tags",
                "--no-recurse-submodules",
                "--no-write-fetch-head",
                "--",
            ])
            .arg(format!("{destination}:{remote_repo}"))
            .arg(format!("{remote_ref}:refs/result"));
        } else {
            cmd.args(["push", "--porcelain", "--no-verify", "--"])
                .arg(format!("{destination}:{remote_repo}"))
                .arg(format!("{}:{remote_ref}", snapshot.commit));
        }
        Ok(cmd)
    }

    pub fn fetch(
        &self,
        root: &Path,
        destination: &str,
        repo: &str,
        reference: &str,
        commit: &str,
    ) -> AppResult<()> {
        let target = ssh_trust::resolve_ssh_target(destination)?;
        ssh_trust::require_trust_or_error(root, &target)?;
        if !valid_commit(commit) {
            return Err(error("HandoffResultInvalid"));
        }
        let format = self.run(&["rev-parse", "--show-object-format"], None)?;
        if format.trim() != if commit.len() == 64 { "sha256" } else { "sha1" } {
            return Err(error(
                "HandoffResultIdentityMismatch: Git object format changed",
            ));
        }
        let temp = tempfile::tempdir().map_err(|e| error(e.to_string()))?;
        let bare = temp.path().join("transport.git");
        let mut init = self.command_with_config(false);
        init.args([
            "init",
            "--bare",
            "--template=",
            if commit.len() == 64 {
                "--object-format=sha256"
            } else {
                "--object-format=sha1"
            },
        ])
        .arg(&bare);
        run(init)?;
        let objects = self.run(&["rev-parse", "--git-path", "objects"], None)?;
        let objects = std::fs::canonicalize(self.repo.join(objects.trim()))
            .map_err(|e| error(e.to_string()))?;
        let snapshot = Snapshot {
            base: commit.into(),
            commit: commit.into(),
            local_ref: String::new(),
            changes: vec![],
        };
        let mut cmd = self.transport_command(
            root,
            destination,
            repo,
            &snapshot,
            reference,
            Some((&bare, &objects)),
            true,
        )?;
        pin_transport_target(&mut cmd, &target)?;
        run_command(&mut cmd)?;
        let mut verify = self.command_with_config(false);
        verify
            .arg("--git-dir")
            .arg(&bare)
            .env("GIT_OBJECT_DIRECTORY", &objects)
            .args(["rev-parse", "--verify", "refs/result^{commit}"]);
        if String::from_utf8_lossy(&run(verify)?.stdout).trim() != commit {
            return Err(error("HandoffResultIdentityMismatch"));
        }
        Ok(())
    }

    pub fn result_ref(&self, id: &str, commit: &str, expected: Option<&str>) -> AppResult<()> {
        if !super::handoff::valid_id(id) || !valid_commit(commit) {
            return Err(error("HandoffResultInvalid"));
        }
        let reference = format!("refs/vellum-handoff-results/{id}");
        if self
            .run(&["rev-parse", "--verify", &reference], None)
            .is_ok_and(|v| v.trim() == commit)
        {
            return Ok(());
        }
        let zero = "0".repeat(commit.len());
        self.run(
            &["update-ref", &reference, commit, expected.unwrap_or(&zero)],
            None,
        )?;
        Ok(())
    }
    pub fn release_result(&self, id: &str, commit: &str) -> AppResult<()> {
        if !super::handoff::valid_id(id) || !valid_commit(commit) {
            return Err(error("HandoffResultInvalid"));
        }
        self.release_ref(&format!("refs/vellum-handoff-results/{id}"), commit)
    }
    fn release_ref(&self, reference: &str, commit: &str) -> AppResult<()> {
        // Missing is already clean. An existing changed ref must fail the CAS.
        let refs = self.run(&["for-each-ref", "--format=%(refname)", reference], None)?;
        if refs.lines().any(|name| name == reference) {
            self.run(&["update-ref", "-d", reference, commit], None)?;
        }
        Ok(())
    }
    pub fn verify_result(&self, snapshot: &Snapshot, commit: &str) -> AppResult<()> {
        if !valid_commit(commit) || commit.len() != snapshot.commit.len() {
            return Err(error("HandoffResultInvalid"));
        }
        let ancestry = self.run(&["rev-list", "--parents", "-n", "1", commit], None)?;
        if ancestry.split_whitespace().collect::<Vec<_>>() != [commit, snapshot.commit.as_str()] {
            return Err(error("HandoffResultIdentityMismatch: result must have exactly the original snapshot as parent"));
        }
        Ok(())
    }
    pub fn worktree_tree(&self) -> AppResult<String> {
        let temp = tempfile::tempdir().map_err(|e| error(e.to_string()))?;
        let index = temp.path().join("index");
        self.inspect()?;
        self.run(&["read-tree", "HEAD"], Some(&index))?;
        self.require_no_lfs(Some(&index))?;
        self.run(&["add", "-A", "--", "."], Some(&index))?;
        Ok(self.run(&["write-tree"], Some(&index))?.trim().into())
    }
    pub fn patch(&self, snapshot: &Snapshot, commit: &str) -> AppResult<Vec<u8>> {
        self.verify_result(snapshot, commit)?;
        let (_temporary, index) = self.copied_index()?;
        let mut cmd = self.command();
        cmd.env("GIT_INDEX_FILE", index);
        cmd.args([
            "diff",
            "--binary",
            "--full-index",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            &snapshot.commit,
            commit,
            "--",
        ]);
        Ok(run(cmd)?.stdout)
    }
    pub fn numstat(&self, snapshot: &Snapshot, commit: &str) -> AppResult<String> {
        self.verify_result(snapshot, commit)?;
        self.run(
            &[
                "diff",
                "--numstat",
                "--no-renames",
                "--no-ext-diff",
                "--no-textconv",
                "-z",
                &snapshot.commit,
                commit,
                "--",
            ],
            None,
        )
    }
    pub fn can_apply(&self, snapshot: &Snapshot, commit: &str) -> AppResult<bool> {
        self.verify_result(snapshot, commit)?;
        let (head, _) = self.inspect()?;
        if head != snapshot.base {
            return Ok(false);
        }
        let (_temporary, index) = self.copied_index()?;
        if !self
            .run(
                &["diff", "--name-only", "--diff-filter=U", "-z", "--"],
                Some(&index),
            )?
            .is_empty()
        {
            return Ok(false);
        }
        let modes = self.run(
            &[
                "diff",
                "--raw",
                "--no-renames",
                "-z",
                &snapshot.commit,
                commit,
                "--",
            ],
            None,
        )?;
        if modes.split('\0').any(|part| {
            part.starts_with(':')
                && part
                    .split_whitespace()
                    .take(2)
                    .any(|mode| mode.trim_start_matches(':') == "160000")
        }) {
            return Ok(false); // worktree-only git apply does not update submodules
        }
        let current = self.worktree_tree()?;
        let changed = self.run(
            &[
                "diff",
                "--name-only",
                "--no-renames",
                "-z",
                &snapshot.commit,
                commit,
                "--",
            ],
            None,
        )?;
        let drift = self.run(
            &[
                "diff",
                "--name-only",
                "--no-renames",
                "-z",
                &snapshot.commit,
                &current,
                "--",
            ],
            None,
        )?;
        let paths = changed
            .split('\0')
            .filter(|s| !s.is_empty())
            .collect::<std::collections::HashSet<_>>();
        if drift.split('\0').any(|path| paths.contains(path)) {
            return Ok(false);
        }
        let patch = self.patch(snapshot, commit)?;
        if patch.is_empty() {
            return Ok(true);
        }
        let mut cmd = self.command();
        cmd.env("GIT_INDEX_FILE", &index);
        cmd.args(["apply", "--check", "--binary", "--whitespace=nowarn", "-"]);
        Ok(run_command_with_input(&mut cmd, Some(patch)).is_ok())
    }
    pub fn apply_result(&self, snapshot: &Snapshot, commit: &str) -> AppResult<()> {
        if !self.can_apply(snapshot, commit)? {
            return Err(error("HandoffLocalDrift"));
        }
        let patch = self.patch(snapshot, commit)?;
        if !patch.is_empty() {
            let (_temporary, index) = self.copied_index()?;
            let mut cmd = self.command();
            cmd.env("GIT_INDEX_FILE", index);
            cmd.args(["apply", "--binary", "--whitespace=nowarn", "-"]);
            run_command_with_input(&mut cmd, Some(patch))?;
        }
        Ok(())
    }
    pub fn branch_result(&self, id: &str, commit: &str) -> AppResult<String> {
        if !super::handoff::valid_id(id) || !valid_commit(commit) {
            return Err(error("HandoffResultInvalid"));
        }
        let branch = format!("vellum/handoff-{id}");
        let reference = format!("refs/heads/{branch}");
        if let Ok(existing) = self.run(&["rev-parse", "--verify", &reference], None) {
            if existing.trim() == commit {
                return Ok(branch);
            }
            return Err(error("HandoffBranchExists: existing branch was preserved"));
        }
        self.run(
            &["update-ref", &reference, commit, &"0".repeat(commit.len())],
            None,
        )?;
        Ok(branch)
    }
}

fn pin_transport_target(command: &mut Command, target: &ssh_trust::SshTarget) -> AppResult<()> {
    let ssh = command
        .get_envs()
        .find(|(name, _)| *name == "GIT_SSH_COMMAND")
        .and_then(|(_, value)| value)
        .map(|value| value.to_string_lossy().into_owned())
        .ok_or_else(|| error("HandoffTransportTargetInvalid"))?;
    let pattern = if target.port == 22 {
        target.host.clone()
    } else {
        format!("[{}]:{}", target.host, target.port)
    };
    command.env(
        "GIT_SSH_COMMAND",
        format!("{ssh} -o {}", quote(&format!("HostKeyAlias={pattern}"))),
    );
    Ok(())
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
    run_command_with_input(command, None)
}
fn run_command_with_input(command: &mut Command, input: Option<Vec<u8>>) -> AppResult<Output> {
    let mut child = command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
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
    let writer = if let Some(input) = input {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| error("HandoffGitPipeMissing"))?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(stdin.write_all(&input));
        });
        Some(rx)
    } else {
        None
    };
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
    if let Some(writer) = writer {
        writer
            .recv_timeout(Duration::from_secs(2))
            .map_err(|e| error(e.to_string()))?
            .map_err(|e| error(e.to_string()))?;
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crlf_apply_inherits_system_config_and_never_touches_real_index_even_when_locked() {
        let (temp, git) = configured_fixture();
        std::fs::write(git.repo.join("t.txt"), "a\nb\n").unwrap();
        git.run(&["add", "."], None).unwrap();
        git.run(&["commit", "-m", "base"], None).unwrap();
        std::fs::remove_file(git.repo.join("t.txt")).unwrap();
        git.run(&["checkout", "--", "t.txt"], None).unwrap();
        assert_eq!(
            std::fs::read(git.repo.join("t.txt")).unwrap(),
            b"a\r\nb\r\n"
        );
        let snapshot = git.snapshot("crlf-apply").unwrap();
        let content = temp.path().join("remote-content");
        std::fs::write(&content, "a\nb\nc\n").unwrap();
        let blob = git
            .run(&["hash-object", "-w", content.to_str().unwrap()], None)
            .unwrap();
        let index = temp.path().join("remote-index");
        git.run(&["read-tree", &snapshot.commit], Some(&index))
            .unwrap();
        git.run(
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("100644,{},t.txt", blob.trim()),
            ],
            Some(&index),
        )
        .unwrap();
        let tree = git.run(&["write-tree"], Some(&index)).unwrap();
        let commit = git
            .run(
                &[
                    "commit-tree",
                    tree.trim(),
                    "-p",
                    &snapshot.commit,
                    "-m",
                    "remote result",
                ],
                None,
            )
            .unwrap();
        let before = std::fs::read(git.repo.join(".git/index")).unwrap();
        // Deliberately invalidate only the entry's cached stat information.
        std::fs::File::options()
            .write(true)
            .open(git.repo.join("t.txt"))
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1_000_000))
            .unwrap();
        // Exercise the formerly mandatory diff auto-refresh independently.
        git.run(
            &["diff", "--name-only", "--diff-filter=U", "-z", "--"],
            None,
        )
        .unwrap();
        assert_eq!(std::fs::read(git.repo.join(".git/index")).unwrap(), before);
        let lock = git.repo.join(".git/index.lock");
        let _guard = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&lock)
            .unwrap();
        assert!(git.inspect().unwrap().1.is_empty());
        assert_eq!(std::fs::read(git.repo.join(".git/index")).unwrap(), before);
        assert!(git.can_apply(&snapshot, commit.trim()).unwrap());
        assert_eq!(std::fs::read(git.repo.join(".git/index")).unwrap(), before);
        git.apply_result(&snapshot, commit.trim()).unwrap();
        assert_eq!(std::fs::read(git.repo.join(".git/index")).unwrap(), before);
        assert_eq!(
            std::fs::read(git.repo.join("t.txt")).unwrap(),
            b"a\r\nb\r\nc\r\n"
        );
        assert!(lock.exists());
    }

    fn result_fixture() -> (tempfile::TempDir, Git, Snapshot, String) {
        let (temp, git) = configured_fixture();
        git.run(&["config", "core.autocrlf", "false"], None)
            .unwrap();
        std::fs::write(git.repo.join("t.txt"), "base\n").unwrap();
        git.run(&["add", "."], None).unwrap();
        git.run(&["commit", "-m", "base"], None).unwrap();
        std::fs::write(git.repo.join("t.txt"), "local unfinished\n").unwrap();
        git.run(&["add", "t.txt"], None).unwrap();
        let snapshot = git.snapshot("return-fixture").unwrap();
        let content = temp.path().join("result-content");
        std::fs::write(&content, "remote finished\n").unwrap();
        let blob = git
            .run(&["hash-object", "-w", content.to_str().unwrap()], None)
            .unwrap();
        let index = temp.path().join("result-index");
        git.run(&["read-tree", &snapshot.commit], Some(&index))
            .unwrap();
        git.run(
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("100644,{},t.txt", blob.trim()),
            ],
            Some(&index),
        )
        .unwrap();
        let tree = git.run(&["write-tree"], Some(&index)).unwrap();
        let commit = git
            .run(
                &[
                    "commit-tree",
                    tree.trim(),
                    "-p",
                    &snapshot.commit,
                    "-m",
                    "result",
                ],
                None,
            )
            .unwrap()
            .trim()
            .to_owned();
        (temp, git, snapshot, commit)
    }
    #[test]
    fn result_applies_against_uncommitted_snapshot_without_changing_real_index() {
        let (_temp, git, snapshot, result) = result_fixture();
        let index = std::fs::read(git.repo.join(".git/index")).unwrap();
        std::fs::write(git.repo.join("unrelated.txt"), "keep me\n").unwrap();
        assert!(git.can_apply(&snapshot, &result).unwrap());
        git.apply_result(&snapshot, &result).unwrap();
        assert_eq!(
            std::fs::read_to_string(git.repo.join("t.txt")).unwrap(),
            "remote finished\n"
        );
        assert_eq!(std::fs::read(git.repo.join(".git/index")).unwrap(), index);
        assert_eq!(
            std::fs::read_to_string(git.repo.join("unrelated.txt")).unwrap(),
            "keep me\n"
        );
        assert_eq!(
            git.run(&["rev-parse", "HEAD"], None).unwrap().trim(),
            snapshot.base
        );
    }
    #[test]
    fn changed_head_or_same_file_drift_saves_branch_without_touching_worktree_or_index() {
        let (_temp, git, snapshot, result) = result_fixture();
        std::fs::write(git.repo.join("t.txt"), "keep newer local work\n").unwrap();
        let index = std::fs::read(git.repo.join(".git/index")).unwrap();
        assert!(!git.can_apply(&snapshot, &result).unwrap());
        assert!(git.apply_result(&snapshot, &result).is_err());
        let branch = git.branch_result("drift-test", &result).unwrap();
        assert_eq!(branch, "vellum/handoff-drift-test");
        assert_eq!(git.branch_result("drift-test", &result).unwrap(), branch);
        assert!(git.branch_result("drift-test", &snapshot.base).is_err());
        assert_eq!(std::fs::read(git.repo.join(".git/index")).unwrap(), index);
        assert_eq!(
            std::fs::read_to_string(git.repo.join("t.txt")).unwrap(),
            "keep newer local work\n"
        );
        std::fs::write(git.repo.join("t.txt"), "local unfinished\n").unwrap();
        git.run(&["update-ref", "HEAD", &snapshot.commit], None)
            .unwrap();
        assert!(!git.can_apply(&snapshot, &result).unwrap());
    }
    #[test]
    fn result_requires_exact_snapshot_parent_and_ref_cleanup_is_repeatable() {
        let (_temp, git, snapshot, result) = result_fixture();
        assert!(git.verify_result(&snapshot, &snapshot.base).is_err());
        git.result_ref("return-fixture", &result, None).unwrap();
        assert!(git
            .result_ref("return-fixture", &snapshot.base, None)
            .is_err());
        assert!(git
            .release_result("return-fixture", &snapshot.base)
            .is_err());
        git.release_result("return-fixture", &result).unwrap();
        git.release_result("return-fixture", &result).unwrap();
        git.release_snapshot(&snapshot).unwrap();
        git.release_snapshot(&snapshot).unwrap();
        assert!(git
            .run(
                &[
                    "for-each-ref",
                    "--format=%(refname)",
                    "refs/vellum-handoff",
                    "refs/vellum-handoff-results"
                ],
                None
            )
            .unwrap()
            .is_empty());
    }
    #[test]
    fn binary_result_is_applied_without_staging_or_replacing_unrelated_local_changes() {
        let (temp, git, snapshot, _) = result_fixture();
        let bytes = b"\0remote\xff\x01\n";
        let content = temp.path().join("binary-content");
        std::fs::write(&content, bytes).unwrap();
        let blob = git
            .run(&["hash-object", "-w", content.to_str().unwrap()], None)
            .unwrap();
        let index = temp.path().join("binary-index");
        git.run(&["read-tree", &snapshot.commit], Some(&index))
            .unwrap();
        git.run(
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("100644,{},binary.dat", blob.trim()),
            ],
            Some(&index),
        )
        .unwrap();
        let tree = git.run(&["write-tree"], Some(&index)).unwrap();
        let commit = git
            .run(
                &[
                    "commit-tree",
                    tree.trim(),
                    "-p",
                    &snapshot.commit,
                    "-m",
                    "binary result",
                ],
                None,
            )
            .unwrap();
        let before = std::fs::read(git.repo.join(".git/index")).unwrap();
        assert!(git
            .numstat(&snapshot, commit.trim())
            .unwrap()
            .starts_with("-\t-\tbinary.dat\0"));
        git.apply_result(&snapshot, commit.trim()).unwrap();
        assert_eq!(std::fs::read(git.repo.join("binary.dat")).unwrap(), bytes);
        assert_eq!(std::fs::read(git.repo.join(".git/index")).unwrap(), before);
        assert_eq!(
            std::fs::read_to_string(git.repo.join("t.txt")).unwrap(),
            "local unfinished\n"
        );
    }
    #[test]
    fn result_fetch_uses_isolated_git_and_only_the_explicit_result_ref() {
        let git = Git::new(Path::new("repo"), Path::new("data"));
        let snapshot = Snapshot {
            base: "a".repeat(40),
            commit: "b".repeat(40),
            local_ref: String::new(),
            changes: vec![],
        };
        let cmd = git
            .transport_command(
                Path::new("data"),
                "vellum-remote-test",
                "/state/repo.git",
                &snapshot,
                "refs/vellum-handoff-results/handoff/capture",
                Some((Path::new("isolated.git"), Path::new("objects"))),
                true,
            )
            .unwrap();
        let args = cmd
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(args.contains(&"fetch".into()));
        assert!(args.contains(&"--no-recurse-submodules".into()));
        assert!(args.contains(&"refs/vellum-handoff-results/handoff/capture:refs/result".into()));
        assert!(cmd
            .get_envs()
            .any(|(key, value)| key == "GIT_CONFIG_NOSYSTEM" && value.is_some_and(|v| v == "1")));
        assert!(git
            .transport_command(
                Path::new("data"),
                "vellum-remote-test",
                "/state/repo.git",
                &snapshot,
                "refs/heads/main",
                None,
                true
            )
            .is_err());
    }
    #[test]
    fn added_host_git_transport_uses_private_alias_and_pins_resolved_endpoint() {
        let git = Git::new(Path::new("repo"), Path::new("data"));
        let snapshot = Snapshot {
            base: "a".repeat(40),
            commit: "b".repeat(40),
            local_ref: String::new(),
            changes: vec![],
        };
        let mut cmd = git
            .transport_command(
                Path::new("data with spaces"),
                "vellum-remote-added-123",
                "/state/repo.git",
                &snapshot,
                "refs/vellum-handoff-results/handoff/capture",
                None,
                true,
            )
            .unwrap();
        pin_transport_target(
            &mut cmd,
            &ssh_trust::SshTarget {
                host: "host.test".into(),
                port: 2222,
            },
        )
        .unwrap();
        let ssh = cmd
            .get_envs()
            .find(|(name, _)| *name == "GIT_SSH_COMMAND")
            .unwrap()
            .1
            .unwrap()
            .to_string_lossy();
        assert!(ssh.contains("'-F' 'data with spaces/remote-ssh/config'"));
        assert!(ssh.contains("HostKeyAlias=[host.test]:2222"));
        assert!(ssh.contains("StrictHostKeyChecking=yes"));
    }
    fn configured_fixture() -> (tempfile::TempDir, Git) {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let system = temp.path().join("system.gitconfig");
        std::fs::write(&system, "[core]\n autocrlf = true\n[filter \"lfs\"]\n clean = git-lfs clean -- %f\n required = true\n[filter \"trim\"]\n clean = git stripspace\n").unwrap();
        let mut git = Git::new(&repo, temp.path());
        git.system_config = Some(system);
        git.run(&["init"], None).unwrap();
        (temp, git)
    }
    #[test]
    fn crlf_checkout_keeps_system_normalization_and_snapshot_tree_matches_head() {
        let (_temp, git) = configured_fixture();
        std::fs::write(git.repo.join("t.txt"), "a\nb\n").unwrap();
        git.run(&["add", "."], None).unwrap();
        git.run(&["commit", "-m", "base"], None).unwrap();
        std::fs::remove_file(git.repo.join("t.txt")).unwrap();
        git.run(&["checkout", "--", "t.txt"], None).unwrap();
        assert_eq!(
            std::fs::read(git.repo.join("t.txt")).unwrap(),
            b"a\r\nb\r\n"
        );
        let before_index = std::fs::read(git.repo.join(".git/index")).unwrap();
        assert!(git.inspect().unwrap().1.is_empty());
        let snapshot = git.snapshot("crlf-fixture").unwrap();
        assert_eq!(
            git.run(
                &["rev-parse", &format!("{}^{{tree}}", snapshot.commit)],
                None
            )
            .unwrap(),
            git.run(&["rev-parse", "HEAD^{tree}"], None).unwrap()
        );
        assert_eq!(
            std::fs::read(git.repo.join(".git/index")).unwrap(),
            before_index
        );
        assert_eq!(
            std::fs::read(git.repo.join("t.txt")).unwrap(),
            b"a\r\nb\r\n"
        );
        let isolated = git.command_with_config(false);
        assert!(isolated
            .get_envs()
            .any(|(key, value)| key == "GIT_CONFIG_NOSYSTEM"
                && value.is_some_and(|value| value == "1")));
    }
    #[test]
    fn non_lfs_clean_filter_uses_the_users_config() {
        let (_temp, git) = configured_fixture();
        std::fs::write(git.repo.join(".gitattributes"), "t.txt filter=trim\n").unwrap();
        std::fs::write(git.repo.join("t.txt"), "base\n").unwrap();
        git.run(&["add", "."], None).unwrap();
        git.run(&["commit", "-m", "base"], None).unwrap();
        std::fs::write(git.repo.join("t.txt"), "changed   \n\n\n").unwrap();
        let snapshot = git.snapshot("filter-fixture").unwrap();
        assert_eq!(
            git.run(&["show", &format!("{}:t.txt", snapshot.commit)], None)
                .unwrap(),
            "changed\n"
        );
        assert_eq!(
            std::fs::read_to_string(git.repo.join("t.txt")).unwrap(),
            "changed   \n\n\n"
        );
    }
    #[test]
    fn release_snapshot_ref_checks_the_expected_commit() {
        let (_temp, git) = configured_fixture();
        std::fs::write(git.repo.join("t.txt"), "base\n").unwrap();
        git.run(&["add", "."], None).unwrap();
        git.run(&["commit", "-m", "base"], None).unwrap();
        let snapshot = git.snapshot("cleanup-fixture").unwrap();
        git.run(&["update-ref", &snapshot.local_ref, &snapshot.base], None)
            .unwrap();
        assert!(git.release_snapshot(&snapshot).is_err());
        assert_eq!(
            git.run(&["rev-parse", &snapshot.local_ref], None)
                .unwrap()
                .trim(),
            snapshot.base
        );
        git.run(
            &[
                "update-ref",
                &snapshot.local_ref,
                &snapshot.commit,
                &snapshot.base,
            ],
            None,
        )
        .unwrap();
        git.release_snapshot(&snapshot).unwrap();
        assert!(git
            .run(&["for-each-ref", &snapshot.local_ref], None)
            .unwrap()
            .is_empty());
    }
    #[test]
    fn lfs_attribute_refuses_even_without_matching_files() {
        let (_temp, git) = configured_fixture();
        std::fs::write(git.repo.join("t.txt"), "base\n").unwrap();
        git.run(&["add", "."], None).unwrap();
        git.run(&["commit", "-m", "base"], None).unwrap();
        std::fs::write(git.repo.join(".gitattributes"), "# example filter=lfs\n").unwrap();
        assert!(git.inspect().is_ok());
        std::fs::write(
            git.repo.join(".gitattributes"),
            "*.bin filter=lfs diff=lfs\n",
        )
        .unwrap();
        assert!(git
            .inspect()
            .unwrap_err()
            .to_string()
            .contains("HandoffLfsUnsupported"));
        assert!(git
            .snapshot("lfs-fixture")
            .unwrap_err()
            .to_string()
            .contains("HandoffLfsUnsupported"));
        assert!(git
            .run(&["for-each-ref", "refs/vellum-handoff/"], None)
            .unwrap()
            .is_empty());
    }
    #[test]
    fn effective_lfs_attributes_from_user_config_refuse_before_clean_filters_run() {
        let (temp, git) = configured_fixture();
        std::fs::write(git.repo.join("t.bin"), "base\n").unwrap();
        git.run(&["add", "."], None).unwrap();
        git.run(&["commit", "-m", "base"], None).unwrap();
        let attributes = temp.path().join("global.attributes");
        std::fs::write(&attributes, "*.bin filter=lfs\n").unwrap();
        git.run(
            &[
                "config",
                "core.attributesFile",
                attributes.to_str().unwrap(),
            ],
            None,
        )
        .unwrap();
        assert!(git
            .inspect()
            .unwrap_err()
            .to_string()
            .contains("HandoffLfsUnsupported"));
    }
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
