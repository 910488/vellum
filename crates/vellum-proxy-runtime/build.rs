fn main() {
    println!("cargo:rerun-if-env-changed=VELLUM_GIT_COMMIT");
    println!("cargo:rerun-if-env-changed=VELLUM_BUILD_VERSION");
    // The baked commit must track the checked-out HEAD: emit the ref file
    // (resolved through HEAD / symbolic refs, including worktrees) as a
    // rerun-if-changed input so a new commit re-runs this build script and
    // re-bakes the new commit into artifact/binary identity. Without this,
    // cargo kept a stale commit from the last crate recompile (provenance
    // drift on every new commit).
    if let Some(ref_file) = git_head_ref_file() {
        println!("cargo:rerun-if-changed={}", ref_file.display());
    }
    if std::env::var("VELLUM_GIT_COMMIT").is_ok() {
        return;
    }
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
    if let Ok(output) = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&manifest_dir)
        .output()
    {
        if output.status.success() {
            let commit = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !commit.is_empty() {
                println!("cargo:rustc-env=VELLUM_GIT_COMMIT={commit}");
            }
        }
    }
}

/// Resolve the git dir and then the concrete ref file backing HEAD, so cargo
/// re-runs this build script whenever the checked-out commit changes.
///
/// `HEAD` itself is usually `ref: refs/heads/<branch>`, so its file content
/// does not change on commit; the branch ref file (or the worktree's packed
/// refs) holds the actual object id and must be the tracked input. Handles:
/// a normal `.git` directory, a worktree `.git` file (`gitdir: <path>`), and
/// loose refs living in the main repo's `.git/refs` while worktree refs that
/// do not exist under the worktree gitdir.
fn git_head_ref_file() -> Option<std::path::PathBuf> {
    let gitdir = git_dir()?;
    let head = gitdir.join("HEAD");
    if !head.is_file() {
        return None;
    }
    let text = std::fs::read_to_string(&head).ok()?;
    let target = text.trim();
    if let Some(symbolic) = target.strip_prefix("ref:") {
        let ref_path = symbolic.trim();
        // Prefer the loose ref under the worktree gitdir, then fall back to
        // the common gitdir (worktree loose branch refs live in the main
        // repo's .git/refs; the worktree gitdir has a `commondir` file such
        // as `../..` pointing at it).
        let local = gitdir.join(ref_path);
        if local.is_file() {
            return Some(local);
        }
        if let Some(common) = common_gitdir(&gitdir) {
            let common_ref = common.join(ref_path);
            if common_ref.is_file() {
                return Some(common_ref);
            }
        }
        // Packed refs fallback: track the packed-refs file (coarse).
        for base in [Some(&gitdir), common_gitdir(&gitdir).as_ref()]
            .into_iter()
            .flatten()
        {
            let packed = base.join("packed-refs");
            if packed.is_file() {
                return Some(packed);
            }
        }
        return None;
    }
    Some(head)
}

/// Resolve the common gitdir a worktree shares with the main repo, from the
/// `commondir` file (e.g. `../..`) or by walking up from the gitdir.
fn common_gitdir(gitdir: &std::path::Path) -> Option<std::path::PathBuf> {
    let commondir = gitdir.join("commondir");
    if commondir.is_file() {
        if let Ok(text) = std::fs::read_to_string(&commondir) {
            let rel = text.trim();
            if !rel.is_empty() {
                let joined = gitdir.join(rel);
                if joined.is_dir() {
                    return Some(joined);
                }
            }
        }
    }
    // gitdir is `<main>/.git/worktrees/<name>`; the main gitdir is two up.
    if let Some(name) = gitdir.file_name().and_then(|name| name.to_str()) {
        if name != ".git" {
            if let Some(parent) = gitdir.parent().and_then(|p| p.parent()) {
                let main = parent.join(".git");
                if main.is_dir() {
                    return Some(main);
                }
            }
        }
    }
    None
}

/// Locate the git directory for this worktree (or plain checkout).
fn git_dir() -> Option<std::path::PathBuf> {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    let root = std::path::Path::new(&manifest_dir);
    let mut candidate = root.to_path_buf();
    loop {
        let dot_git = candidate.join(".git");
        if dot_git.is_dir() {
            return Some(dot_git);
        }
        if dot_git.is_file() {
            // Worktree: `.git` is `gitdir: /path/to/main/.git/worktrees/<name>`.
            let text = std::fs::read_to_string(&dot_git).ok()?;
            if let Some(line) = text
                .lines()
                .find_map(|line| line.strip_prefix("gitdir:"))
                .map(str::trim)
            {
                let gitdir = std::path::Path::new(line).to_path_buf();
                if gitdir.is_dir() {
                    return Some(gitdir);
                }
            }
        }
        if !candidate.pop() {
            break;
        }
    }
    None
}
