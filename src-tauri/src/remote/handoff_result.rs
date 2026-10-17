//! Explicit result capture and local disposition. Network calls never apply files.
use super::super::handoff_git::valid_commit;
use super::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Capture {
    #[serde(default)]
    pub excluded_cache_files: usize,
    pub result_ref: String,
    pub commit: String,
    pub snapshot_commit: String,
    pub partial: bool,
    pub diffstat: Vec<FileStat>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileStat {
    pub path: String,
    pub added: Option<u64>,
    pub deleted: Option<u64>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ResultState {
    pub capture_operation: Option<String>,
    pub capture: Option<Capture>,
    pub fetched: bool,
    pub local_commit: Option<String>,
    pub outcome: Option<String>,
    pub branch: Option<String>,
    pub apply_intent: bool,
    pub cleaned: bool,
    pub acknowledged: bool,
}
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Review {
    pub can_apply: bool,
    pub patch: String,
}

pub fn capture(state: &AppState, id: &str) -> AppResult<Record> {
    let root = state.data_root();
    let _guard = Lock::acquire(&root, id)?;
    let mut record = load(&root, id)?;
    if record.result.outcome.is_some() {
        return Ok(record);
    }
    let client = transport(state, &record.host_id)?;
    request_capture_with(&root, &mut record, &client)?;
    Ok(record)
}
fn request_capture_with(root: &Path, record: &mut Record, client: &dyn Transport) -> AppResult<()> {
    if record.result.fetched && record.result.capture.is_some() {
        record.result.capture_operation = None;
    }
    capture_with(root, record, client)
}
fn capture_with(root: &Path, record: &mut Record, client: &dyn Transport) -> AppResult<()> {
    // A failed fetch retries the SAME capture, including partial captures.
    if record.result.capture_operation.is_none() {
        record.result.capture_operation = Some(ulid::Ulid::new().to_string());
        record.result.capture = None;
        record.result.fetched = false;
        save(root, record)?;
    }
    if record.result.capture.is_none() {
        let operation = record
            .result
            .capture_operation
            .as_deref()
            .unwrap_or_default();
        let remote = client.rpc(
            json!({"method":"codex.handoffResult", "handoffId":record.id,
            "capture":true, "operationId":operation}),
        )?;
        let captured: Capture = serde_json::from_value(remote["capture"].clone())
            .map_err(|_| error("HandoffResultUnavailable"))?;
        if captured.result_ref != format!("refs/vellum-handoff-results/{}/{operation}", record.id)
            || captured.snapshot_commit != record.snapshot.commit
            || !valid_commit(&captured.commit)
            || captured.commit.len() != record.snapshot.commit.len()
            || (remote["state"] != "completed" && !captured.partial)
        {
            return Err(error("HandoffResultIdentityMismatch"));
        }
        update_remote(record, remote)?;
        record.result.capture = Some(captured);
        save(root, record)?;
    }
    let captured = record
        .result
        .capture
        .as_ref()
        .ok_or_else(|| error("HandoffResultUnavailable"))?;
    let git = Git::new(&record.source.repo, root);
    if !record.result.fetched {
        let repo = record
            .remote
            .as_ref()
            .and_then(|v| v["repoPath"].as_str())
            .ok_or_else(|| error("HandoffResponseIdentityInvalid"))?;
        client.fetch(
            &record.source.repo,
            repo,
            &captured.result_ref,
            &captured.commit,
        )?;
        git.verify_result(&record.snapshot, &captured.commit)?;
        git.result_ref(
            &record.id,
            &captured.commit,
            record.result.local_commit.as_deref(),
        )?;
        let stats = git
            .numstat(&record.snapshot, &captured.commit)?
            .split('\0')
            .filter(|line| !line.is_empty())
            .map(|line| {
                let mut fields = line.splitn(3, '\t');
                let parse = |text: &str| -> AppResult<Option<u64>> {
                    if text == "-" {
                        Ok(None)
                    } else {
                        text.parse()
                            .map(Some)
                            .map_err(|_| error("HandoffDiffstatInvalid"))
                    }
                };
                let added = parse(
                    fields
                        .next()
                        .ok_or_else(|| error("HandoffDiffstatInvalid"))?,
                )?;
                let deleted = parse(
                    fields
                        .next()
                        .ok_or_else(|| error("HandoffDiffstatInvalid"))?,
                )?;
                let path = fields
                    .next()
                    .ok_or_else(|| error("HandoffDiffstatInvalid"))?
                    .to_owned();
                Ok(FileStat {
                    path,
                    added,
                    deleted,
                })
            })
            .collect::<AppResult<Vec<_>>>()?;
        record
            .result
            .capture
            .as_mut()
            .ok_or_else(|| error("HandoffResultUnavailable"))?
            .diffstat = stats;
        record.result.local_commit = record
            .result
            .capture
            .as_ref()
            .map(|capture| capture.commit.clone());
        record.result.fetched = true;
        record.error = None;
        save(root, record)?;
    }
    Ok(())
}
pub fn review(state: &AppState, id: &str) -> AppResult<Review> {
    let root = state.data_root();
    let _guard = Lock::acquire(&root, id)?;
    let record = load(&root, id)?;
    let commit = ready_commit(&record)?;
    let git = Git::new(&record.source.repo, &root);
    Ok(Review {
        can_apply: !record.result.apply_intent && git.can_apply(&record.snapshot, commit)?,
        patch: String::from_utf8_lossy(&git.patch(&record.snapshot, commit)?).into_owned(),
    })
}
fn ready_commit(record: &Record) -> AppResult<&str> {
    if !record.result.fetched {
        return Err(error("HandoffResultUnavailable"));
    }
    record
        .result
        .capture
        .as_ref()
        .map(|v| v.commit.as_str())
        .ok_or_else(|| error("HandoffResultUnavailable"))
}
pub fn settle(state: &AppState, id: &str, branch_only: bool) -> AppResult<Record> {
    let root = state.data_root();
    let _guard = Lock::acquire(&root, id)?;
    let mut record = load(&root, id)?;
    settle_with(&root, &mut record, branch_only)?;
    Ok(record)
}
fn settle_with(root: &Path, record: &mut Record, branch_only: bool) -> AppResult<()> {
    let git = Git::new(&record.source.repo, root);
    if record.result.outcome.is_none() {
        if !terminal(record) {
            return Err(error("HandoffStillRunning"));
        }
        let commit = ready_commit(record)?.to_owned();
        git.verify_result(&record.snapshot, &commit)?;
        // A crash after writing application intent must never apply twice.
        // Recovery saves the result as a branch, preserving the worktree as found.
        if !branch_only
            && !record.result.apply_intent
            && git.can_apply(&record.snapshot, &commit)?
        {
            record.result.apply_intent = true;
            save(root, record)?;
            match git.apply_result(&record.snapshot, &commit) {
                Ok(()) => record.result.outcome = Some("applied".into()),
                Err(e) => {
                    record.error = Some(e.to_string());
                    save(root, record)?;
                    return Err(e);
                }
            }
        } else {
            record.result.branch = Some(git.branch_result(&record.id, &commit)?);
            record.result.outcome = Some("branched".into());
        }
        record.error = None;
        save(root, record)?; // disposition durable BEFORE removing reachability refs
    }
    cleanup(root, record, &git)
}
pub fn discard(state: &AppState, id: &str) -> AppResult<Record> {
    let root = state.data_root();
    let _guard = Lock::acquire(&root, id)?;
    let mut record = load(&root, id)?;
    if record.result.outcome.is_none() {
        // Discard is not a remote turn cancellation. Require an observed terminal
        // state before releasing the local thread to work again.
        if !terminal(&record)
            && !matches!(
                record.stage.as_str(),
                "localSnapshot" | "remotePrepared" | "snapshotPushed"
            )
        {
            let client = transport(state, &record.host_id)?;
            poll_with(&root, &mut record, &client)?;
            if !terminal(&record) {
                return Err(error(
                    "HandoffStillRunning: wait for a terminal result before discarding",
                ));
            }
        }
        record.result.outcome = Some("discarded".into());
        save(&root, &record)?;
    }
    let git = Git::new(&record.source.repo, &root);
    cleanup(&root, &mut record, &git)?;
    Ok(record)
}
fn cleanup(root: &Path, record: &mut Record, git: &Git) -> AppResult<()> {
    if !record.result.cleaned {
        git.release_snapshot(&record.snapshot)?;
        if let Some(capture) = &record.result.capture {
            // Publication may have succeeded just before a journal write failed.
            // Both candidates are recorded as ours; neither permits deleting an
            // unrelated ref update.
            git.release_result(&record.id, &capture.commit)
                .or_else(|e| {
                    if let Some(previous) = &record.result.local_commit {
                        git.release_result(&record.id, previous)
                    } else {
                        Err(e)
                    }
                })?;
        } else if let Some(commit) = &record.result.local_commit {
            git.release_result(&record.id, commit)?;
        }
        record.result.cleaned = true;
        save(root, record)?;
    }
    Ok(())
}
fn terminal(record: &Record) -> bool {
    matches!(
        record.remote.as_ref().and_then(|r| r["state"].as_str()),
        Some("completed" | "failed" | "interrupted")
    )
}
pub fn acknowledge(state: &AppState, id: &str) -> AppResult<()> {
    let root = state.data_root();
    let _guard = Lock::acquire(&root, id)?;
    let mut record = load(&root, id)?;
    record.result.acknowledged = true;
    save(&root, &record)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    struct FakeResult {
        remote: Value,
        commit: String,
        fail_fetch: Cell<bool>,
        captures: Cell<usize>,
        fetches: Cell<usize>,
    }
    impl Transport for FakeResult {
        fn rpc(&self, request: Value) -> AppResult<Value> {
            assert_eq!(request["method"], "codex.handoffResult");
            assert_eq!(request["capture"], true);
            self.captures.set(self.captures.get() + 1);
            let mut status = self.remote.clone();
            status["capture"] = json!({ "resultRef": format!("refs/vellum-handoff-results/{}/{}", request["handoffId"].as_str().unwrap(), request["operationId"].as_str().unwrap()),
                "commit": self.commit, "snapshotCommit": status["snapshotCommit"], "partial": false,
                "diffstat": [{"path":"file", "added":1, "deleted":1}] });
            Ok(status)
        }
        fn push(&self, _: &Path, _: &Snapshot, _: &str, _: &str) -> AppResult<()> {
            panic!("result must never push or start")
        }
        fn fetch(&self, _: &Path, _: &str, _: &str, commit: &str) -> AppResult<()> {
            self.fetches.set(self.fetches.get() + 1);
            assert_eq!(commit, self.commit);
            if self.fail_fetch.get() {
                Err(error("injected fetch failure"))
            } else {
                Ok(())
            }
        }
    }
    fn fixture() -> (tempfile::TempDir, Record, FakeResult) {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        fs::create_dir(&repo).unwrap();
        let git = Git::new(&repo, temp.path());
        git.run(&["init"], None).unwrap();
        git.run(&["config", "core.autocrlf", "false"], None)
            .unwrap();
        fs::write(repo.join("file"), "base\n").unwrap();
        git.run(&["add", "."], None).unwrap();
        git.run(&["commit", "-m", "base"], None).unwrap();
        let snapshot = git.snapshot("return-test").unwrap();
        fs::write(repo.join("file"), "remote\n").unwrap();
        let index = temp.path().join("capture-index");
        git.run(&["read-tree", &snapshot.commit], Some(&index))
            .unwrap();
        git.run(&["add", "-A"], Some(&index)).unwrap();
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
        fs::write(repo.join("file"), "base\n").unwrap();
        let remote = json!({"handoffId":"return-test", "sourceThreadId":"source", "state":"completed", "phase":"running", "threadId":"thread", "turnId":"turn",
            "repoPath":"/state/handoffs/repos/repo.git", "worktree":"/state/handoffs/work/return-test", "snapshotRef":"refs/vellum-handoff/return-test", "snapshotCommit":snapshot.commit});
        let record = Record {
            writeback: None,
            id: "return-test".into(),
            host_id: "host".into(),
            source: LocalThread {
                thread_id: "source".into(),
                name: "Task".into(),
                repo,
                branch: "main".into(),
                updated_at: 1,
                model: Some("model".into()),
                model_provider: Some("vellum".into()),
                effort: Some("high".into()),
                active: Some(false),
                handed_off: true,
                draft: None,
            },
            snapshot,
            request: json!({"repoId":"repo"}),
            stage: "running".into(),
            remote: Some(remote.clone()),
            error: None,
            result: ResultState::default(),
        };
        let client = FakeResult {
            remote,
            commit,
            fail_fetch: Cell::new(false),
            captures: Cell::new(0),
            fetches: Cell::new(0),
        };
        (temp, record, client)
    }
    #[test]
    fn explicit_recapture_uses_a_new_operation_after_a_successful_fetch() {
        let (temp, mut record, client) = fixture();
        let index = fs::read(record.source.repo.join(".git/index")).unwrap();
        request_capture_with(temp.path(), &mut record, &client).unwrap();
        let first_operation = record.result.capture_operation.clone();
        record.result.capture.as_mut().unwrap().partial = true;
        request_capture_with(temp.path(), &mut record, &client).unwrap();
        assert_ne!(record.result.capture_operation, first_operation);
        assert!(!record.result.capture.as_ref().unwrap().partial);
        let second_operation = record.result.capture_operation.clone();
        request_capture_with(temp.path(), &mut record, &client).unwrap();
        assert_ne!(record.result.capture_operation, second_operation);
        assert_eq!(client.captures.get(), 3);
        assert_eq!(client.fetches.get(), 3);
        assert_eq!(
            fs::read(record.source.repo.join(".git/index")).unwrap(),
            index
        );
    }

    #[test]
    fn fetch_failure_retries_same_capture_and_apply_cleans_refs_without_changing_index() {
        let (temp, mut record, client) = fixture();
        let index = fs::read(record.source.repo.join(".git/index")).unwrap();
        client.fail_fetch.set(true);
        assert!(request_capture_with(temp.path(), &mut record, &client).is_err());
        let operation = record.result.capture_operation.clone();
        record = load(temp.path(), &record.id).unwrap();
        client.fail_fetch.set(false);
        request_capture_with(temp.path(), &mut record, &client).unwrap();
        assert_eq!(record.result.capture_operation, operation);
        assert_eq!(client.captures.get(), 1);
        assert_eq!(client.fetches.get(), 2);
        settle_with(temp.path(), &mut record, false).unwrap();
        settle_with(temp.path(), &mut record, false).unwrap();
        assert_eq!(record.result.outcome.as_deref(), Some("applied"));
        assert!(record.result.cleaned);
        assert_eq!(
            fs::read_to_string(record.source.repo.join("file")).unwrap(),
            "remote\n"
        );
        assert_eq!(
            fs::read(record.source.repo.join(".git/index")).unwrap(),
            index
        );
        assert!(Git::new(&record.source.repo, temp.path())
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
    fn drift_and_uncertain_apply_intent_create_branch_preserving_local_files() {
        for uncertain in [false, true] {
            let (temp, mut record, client) = fixture();
            capture_with(temp.path(), &mut record, &client).unwrap();
            let content = if uncertain {
                "remote\n"
            } else {
                "new local work\n"
            };
            fs::write(record.source.repo.join("file"), content).unwrap();
            record.result.apply_intent = uncertain;
            settle_with(temp.path(), &mut record, false).unwrap();
            assert_eq!(record.result.outcome.as_deref(), Some("branched"));
            assert_eq!(
                record.result.branch.as_deref(),
                Some("vellum/handoff-return-test")
            );
            assert_eq!(
                fs::read_to_string(record.source.repo.join("file")).unwrap(),
                content
            );
            assert!(record.result.cleaned);
        }
    }
    #[test]
    fn discard_cleanup_is_local_repeatable_and_preserves_changed_refs() {
        let (temp, mut record, client) = fixture();
        capture_with(temp.path(), &mut record, &client).unwrap();
        let git = Git::new(&record.source.repo, temp.path());
        git.run(
            &[
                "update-ref",
                &record.snapshot.local_ref,
                &record.snapshot.base,
            ],
            None,
        )
        .unwrap();
        record.result.outcome = Some("discarded".into());
        save(temp.path(), &record).unwrap();
        assert!(cleanup(temp.path(), &mut record, &git).is_err());
        assert!(!record.result.cleaned);
        git.run(
            &[
                "update-ref",
                &record.snapshot.local_ref,
                &record.snapshot.commit,
            ],
            None,
        )
        .unwrap();
        cleanup(temp.path(), &mut record, &git).unwrap();
        cleanup(temp.path(), &mut record, &git).unwrap();
        assert_eq!(
            fs::read_to_string(record.source.repo.join("file")).unwrap(),
            "base\n"
        );
        assert_eq!(client.captures.get(), 1);
    }
    #[test]
    fn older_records_default_result_state_and_running_cannot_be_applied() {
        let (temp, mut record, client) = fixture();
        let mut legacy = serde_json::to_value(&record).unwrap();
        legacy.as_object_mut().unwrap().remove("result");
        assert!(serde_json::from_value::<Record>(legacy)
            .unwrap()
            .result
            .outcome
            .is_none());
        capture_with(temp.path(), &mut record, &client).unwrap();
        record.remote.as_mut().unwrap()["state"] = "running".into();
        assert!(settle_with(temp.path(), &mut record, false)
            .unwrap_err()
            .to_string()
            .starts_with("HandoffStillRunning"));
    }
    #[test]
    fn terminal_discard_and_acknowledgement_work_without_resolving_an_ssh_host() {
        let (temp, mut record, client) = fixture();
        capture_with(temp.path(), &mut record, &client).unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        // No cached host named "host" exists. Any transport resolution fails.
        assert!(transport(&state, "host").is_err());
        let discarded = discard(&state, &record.id).unwrap();
        assert_eq!(discarded.result.outcome.as_deref(), Some("discarded"));
        assert!(discarded.result.cleaned);
        acknowledge(&state, &record.id).unwrap();
        assert!(load(temp.path(), &record.id).unwrap().result.acknowledged);
        assert!(discard(&state, &record.id).unwrap().result.cleaned);
    }
    #[test]
    fn wrong_result_ancestry_is_rejected_without_publishing_a_local_result_ref() {
        let (temp, mut record, mut client) = fixture();
        client.commit = record.snapshot.base.clone();
        assert!(capture_with(temp.path(), &mut record, &client).is_err());
        assert!(!record.result.fetched);
        assert!(record.result.outcome.is_none());
        let git = Git::new(&record.source.repo, temp.path());
        assert_eq!(
            git.run(&["rev-parse", &record.snapshot.local_ref], None)
                .unwrap()
                .trim(),
            record.snapshot.commit
        );
        assert!(git
            .run(
                &[
                    "for-each-ref",
                    "--format=%(refname)",
                    "refs/vellum-handoff-results"
                ],
                None
            )
            .unwrap()
            .is_empty());
    }
}
