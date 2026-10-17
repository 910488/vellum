use super::*;

#[test]
fn git_paths_normalize_verbatim_drive_and_unc_without_changing_ordinary_paths() {
    for (input, expected) in [
        (
            r"\\?\E:\repo with spaces\.git\objects",
            r"E:\repo with spaces\.git\objects",
        ),
        (
            r"\\?\UNC\server\share\repo\.git\objects",
            r"\\server\share\repo\.git\objects",
        ),
        (r"E:\repo", r"E:\repo"),
        (r"\\server\share\repo", r"\\server\share\repo"),
        (r"relative\repo", r"relative\repo"),
    ] {
        assert_eq!(git_path(Path::new(input)), Path::new(expected));
    }
}

fn assert_no_verbatim(cmd: &Command) {
    for arg in cmd.get_args() {
        assert!(!arg.to_string_lossy().contains(r"\\?\"), "{arg:?}");
    }
    for (name, value) in cmd.get_envs() {
        if let Some(value) = value {
            assert!(
                !value.to_string_lossy().contains(r"\\?\"),
                "{name:?}={value:?}"
            );
        }
    }
}

#[test]
fn push_fetch_and_verify_commands_normalize_all_git_path_arguments() {
    let root = Path::new(r"\\?\E:\data with spaces");
    let git = Git::new(Path::new(r"\\?\E:\repo"), root);
    let directory = Path::new(r"\\?\UNC\server\share\transport.git");
    let objects = Path::new(r"\\?\E:\repo\.git\objects");
    let snapshot = Snapshot {
        base: "a".repeat(40),
        commit: "b".repeat(40),
        local_ref: String::new(),
        changes: vec![],
    };
    for fetch in [false, true] {
        let reference = if fetch {
            "refs/vellum-handoff-results/handoff/capture"
        } else {
            "refs/vellum-handoff/handoff"
        };
        let cmd = git
            .transport_command(
                root,
                "vellum-remote-added-host",
                "/state/repo.git",
                &snapshot,
                reference,
                Some((directory, objects)),
                fetch,
            )
            .unwrap();
        assert_no_verbatim(&cmd);
        assert!(cmd
            .get_envs()
            .any(|(key, value)| key == "GIT_OBJECT_DIRECTORY"
                && value == Some(std::ffi::OsStr::new(r"E:\repo\.git\objects"))));
    }
    let mut verify = git.command_with_config(false);
    git.isolate_objects(&mut verify, directory, objects);
    verify.args(["rev-parse", "--verify", "refs/result^{commit}"]);
    assert_no_verbatim(&verify);
}

#[test]
fn isolated_bare_repository_can_read_and_pack_canonicalized_source_objects() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("source repo");
    let root = temp.path().join("data");
    std::fs::create_dir(&repo).unwrap();
    // Real std canonicalization supplies exactly the problematic prefix.
    let repo = std::fs::canonicalize(repo).unwrap();
    assert!(repo.to_string_lossy().starts_with(r"\\?\"));
    let git = Git::new(&repo, &root);
    git.run(&["init"], None).unwrap();
    std::fs::write(repo.join("file.txt"), "snapshot content\n").unwrap();
    git.run(&["add", "."], None).unwrap();
    git.run(&["commit", "-m", "base"], None).unwrap();
    let snapshot = git.snapshot("windows-isolation").unwrap();
    let bare = temp.path().join("transport.git");
    let mut init = git.command_with_config(false);
    init.args(["init", "--bare", "--template="])
        .arg(git_path(&bare));
    run(init).unwrap();
    let bare = std::fs::canonicalize(bare).unwrap();
    let objects = std::fs::canonicalize(repo.join(".git/objects")).unwrap();
    let index = temp.path().join("temporary-index");
    git.run(&["read-tree", "HEAD"], Some(&index)).unwrap();
    let index = std::fs::canonicalize(index).unwrap();
    git.run(&["write-tree"], Some(&index)).unwrap();
    let mut inspect = git.command_with_config(false);
    git.isolate_objects(&mut inspect, &bare, &objects);
    inspect.args(["cat-file", "-t", &snapshot.commit]);
    assert_no_verbatim(&inspect);
    assert_eq!(
        String::from_utf8(run(inspect).unwrap().stdout)
            .unwrap()
            .trim(),
        "commit"
    );
    let mut pack = git.command_with_config(false);
    git.isolate_objects(&mut pack, &bare, &objects);
    pack.args(["pack-objects", "--stdout", "--revs"]);
    assert_no_verbatim(&pack);
    let output = run_command_with_input(
        &mut pack,
        Some(format!("{}\n", snapshot.commit).into_bytes()),
    )
    .unwrap();
    assert!(output.stdout.starts_with(b"PACK"));
}
