//! Process identity for the managed Codex runtime.
//!
//! Authority is UID + binary path + start time + daemon/socket ownership.
//! PID-only matches, "first app-server" scans, and process-name takeover of
//! local ChatGPT / Vellum Enhanced are rejected.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessFacts {
    pub pid: u32,
    pub uid: u32,
    pub binary: PathBuf,
    pub start_time: String,
    pub codex_home: Option<PathBuf>,
    pub socket_path: Option<PathBuf>,
    pub socket_owner_uid: Option<u32>,
    pub comm: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedIdentity {
    pub uid: u32,
    pub binary: PathBuf,
    pub start_time: String,
    pub codex_home: PathBuf,
    pub socket_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityVerdict {
    Match,
    Reject { code: &'static str, detail: String },
}

pub fn match_managed_process(facts: &ProcessFacts, expected: &ExpectedIdentity) -> IdentityVerdict {
    if facts.uid != expected.uid {
        return IdentityVerdict::Reject {
            code: "processUidMismatch",
            detail: format!("uid {} != {}", facts.uid, expected.uid),
        };
    }
    if canonicalize_or(&facts.binary) != canonicalize_or(&expected.binary) {
        return IdentityVerdict::Reject {
            code: "processBinaryMismatch",
            detail: format!(
                "binary {} != {}",
                facts.binary.display(),
                expected.binary.display()
            ),
        };
    }
    if facts.start_time.trim() != expected.start_time.trim() {
        return IdentityVerdict::Reject {
            code: "processStartTimeMismatch",
            detail: "start time does not match the daemon pid record".into(),
        };
    }
    match facts.codex_home.as_ref() {
        Some(home) if canonicalize_or(home) == canonicalize_or(&expected.codex_home) => {}
        Some(home) => {
            return IdentityVerdict::Reject {
                code: "processCodexHomeMismatch",
                detail: format!(
                    "CODEX_HOME {} != {}",
                    home.display(),
                    expected.codex_home.display()
                ),
            };
        }
        None => {
            return IdentityVerdict::Reject {
                code: "processCodexHomeMissing",
                detail: "process has no CODEX_HOME; refusing to guess".into(),
            };
        }
    }
    if facts.socket_owner_uid != Some(expected.uid) {
        return IdentityVerdict::Reject {
            code: "processSocketOwnershipMismatch",
            detail: "control socket is not owned by the managed uid".into(),
        };
    }
    match facts.socket_path.as_ref() {
        Some(path) if canonicalize_or(path) == canonicalize_or(&expected.socket_path) => {}
        Some(path) => {
            return IdentityVerdict::Reject {
                code: "processSocketMismatch",
                detail: format!(
                    "socket {} != {}",
                    path.display(),
                    expected.socket_path.display()
                ),
            };
        }
        None => {
            return IdentityVerdict::Reject {
                code: "processSocketMissing",
                detail: "no control socket observed".into(),
            };
        }
    }
    IdentityVerdict::Match
}

/// PID-only identity is never sufficient, including the non-Linux fallback
/// that used to emit `pid-{n}`.
pub fn pid_only_identity_is_sufficient(_pid: u32) -> bool {
    false
}

/// A host-wide scan that returns the first `app-server` must never become
/// managed authority — that is how a local ChatGPT / Enhanced process would
/// be taken over on a shared Mac.
pub fn first_app_server_scan_is_authority() -> bool {
    false
}

/// Process-name matching (`codex`, `app-server`, ChatGPT, Vellum Enhanced)
/// is not an ownership signal.
pub fn process_name_scan_may_takeover(comm: &str) -> bool {
    let _ = comm;
    false
}

pub fn local_codex_home_is_out_of_scope(local_home: &Path, managed_home: &Path) -> bool {
    canonicalize_or(local_home) != canonicalize_or(managed_home)
}

fn canonicalize_or(path: &Path) -> PathBuf {
    path.components().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expected() -> ExpectedIdentity {
        ExpectedIdentity {
            uid: 501,
            binary: PathBuf::from("/Users/joshhuang/.vellum-remote/codex/packages/standalone/current/codex"),
            start_time: "Fri Aug 14 16:21:43 2026".into(),
            codex_home: PathBuf::from("/Users/joshhuang/.vellum-remote/codex"),
            socket_path: PathBuf::from(
                "/Users/joshhuang/.vellum-remote/codex/app-server-control/app-server-control.sock",
            ),
        }
    }

    fn matching_facts() -> ProcessFacts {
        let expected = expected();
        ProcessFacts {
            pid: 4242,
            uid: expected.uid,
            binary: expected.binary.clone(),
            start_time: expected.start_time.clone(),
            codex_home: Some(expected.codex_home.clone()),
            socket_path: Some(expected.socket_path.clone()),
            socket_owner_uid: Some(expected.uid),
            comm: Some("codex".into()),
        }
    }

    #[test]
    fn matching_uid_binary_start_time_and_socket_is_accepted() {
        assert_eq!(
            match_managed_process(&matching_facts(), &expected()),
            IdentityVerdict::Match
        );
    }

    #[test]
    fn pid_only_and_first_app_server_and_name_scan_are_rejected() {
        assert!(!pid_only_identity_is_sufficient(4242));
        assert!(!first_app_server_scan_is_authority());
        assert!(!process_name_scan_may_takeover("app-server"));
        assert!(!process_name_scan_may_takeover("Codex"));
        assert!(!process_name_scan_may_takeover("ChatGPT"));
        assert!(!process_name_scan_may_takeover("Vellum Enhanced"));
    }

    #[test]
    fn local_chatgpt_home_is_not_the_managed_home() {
        assert!(local_codex_home_is_out_of_scope(
            Path::new("/Users/joshhuang/.codex"),
            Path::new("/Users/joshhuang/.vellum-remote/codex")
        ));
        let mut facts = matching_facts();
        facts.codex_home = Some(PathBuf::from("/Users/joshhuang/.codex"));
        let verdict = match_managed_process(&facts, &expected());
        assert!(
            matches!(verdict, IdentityVerdict::Reject { code, .. } if code == "processCodexHomeMismatch")
        );
    }

    #[test]
    fn missing_home_or_socket_or_start_time_fails_closed() {
        let expected = expected();
        let mut facts = matching_facts();
        facts.codex_home = None;
        assert!(matches!(
            match_managed_process(&facts, &expected),
            IdentityVerdict::Reject {
                code: "processCodexHomeMissing",
                ..
            }
        ));
        facts = matching_facts();
        facts.socket_owner_uid = Some(0);
        assert!(matches!(
            match_managed_process(&facts, &expected),
            IdentityVerdict::Reject {
                code: "processSocketOwnershipMismatch",
                ..
            }
        ));
        facts = matching_facts();
        facts.start_time = "other".into();
        assert!(matches!(
            match_managed_process(&facts, &expected),
            IdentityVerdict::Reject {
                code: "processStartTimeMismatch",
                ..
            }
        ));
        facts = matching_facts();
        facts.uid = 502;
        assert!(matches!(
            match_managed_process(&facts, &expected),
            IdentityVerdict::Reject {
                code: "processUidMismatch",
                ..
            }
        ));
    }
}
