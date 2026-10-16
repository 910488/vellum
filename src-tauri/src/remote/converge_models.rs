//! Credential metadata for automatic deployment. Secret bytes never enter
//! this selection or its renderer-visible result.

use crate::error::{AppError, AppResult};
use serde_json::Value;
use std::collections::BTreeSet;

#[derive(Debug)]
pub(crate) struct Candidate {
    pub catalog_id: String,
    pub official: bool,
    pub credential_available: bool,
}

#[derive(Debug)]
pub(crate) struct Selection {
    pub catalog_ids: Vec<String>,
    pub omitted_catalog_ids: Vec<String>,
    pub chatgpt_pairing_suggested: bool,
}

pub(crate) fn observe_official_account<T>(
    needed: bool,
    expected: Option<&str>,
    resolve: impl FnOnce() -> AppResult<T>,
    query: impl FnOnce(T, &str) -> AppResult<Value>,
) -> AppResult<Option<Value>> {
    let Some(expected) = expected.filter(|_| needed) else {
        return Ok(None);
    };
    let status = query(resolve()?, expected)?;
    if !matches!(
        status.get("state").and_then(Value::as_str),
        Some(
            "ready"
                | "synchronized"
                | "pairingRequired"
                | "pairingPending"
                | "accountUnavailable"
                | "activationRequired"
                | "reauthenticationRequired"
        )
    ) {
        return Err(AppError::Message(
            "officialAccountStatusUnavailable: invalid account status".into(),
        ));
    }
    Ok(Some(status))
}

pub(crate) fn official_account_ready(expected: Option<&str>, status: Option<&Value>) -> bool {
    let Some(expected) = expected else {
        return false;
    };
    status.is_some_and(|status| {
        status.get("accountId").and_then(Value::as_str) == Some(expected)
            && status.get("active").and_then(Value::as_bool) == Some(true)
            && matches!(
                status.get("state").and_then(Value::as_str),
                Some("ready" | "synchronized")
            )
            && !matches!(
                status.get("credentialState").and_then(Value::as_str),
                Some("expired" | "missing")
            )
    })
}

pub(crate) fn select(
    requested: &[String],
    candidates: &[Candidate],
    official_blocker: &str,
) -> AppResult<Selection> {
    let requested = requested.iter().cloned().collect::<BTreeSet<_>>();
    let wanted = |id: &str| requested.is_empty() || requested.contains(id);
    let selected = candidates
        .iter()
        .filter(|candidate| wanted(&candidate.catalog_id) && candidate.credential_available)
        .map(|candidate| candidate.catalog_id.clone())
        .collect::<BTreeSet<_>>();
    let omitted_official = candidates.iter().any(|candidate| {
        wanted(&candidate.catalog_id) && candidate.official && !candidate.credential_available
    });
    if selected.is_empty() {
        return Err(AppError::Message(
            if omitted_official {
                official_blocker
            } else {
                "noDeployableModels"
            }
            .into(),
        ));
    }
    let mut omitted = candidates
        .iter()
        .filter(|candidate| wanted(&candidate.catalog_id) && !candidate.credential_available)
        .map(|candidate| candidate.catalog_id.clone())
        .collect::<BTreeSet<_>>();
    // Persisted choices can refer to removed, disabled or newly unqualified
    // models. Omit them; do not silently choose unrelated models instead.
    omitted.extend(requested.difference(&selected).cloned());
    Ok(Selection {
        catalog_ids: selected.into_iter().collect(),
        omitted_catalog_ids: omitted.into_iter().collect(),
        chatgpt_pairing_suggested: omitted_official,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn transport_failures_abort_before_model_selection_with_or_without_alternatives() {
        for alternatives in [true, false] {
            let selection_reached = std::cell::Cell::new(false);
            let result = (|| {
                let _status = observe_official_account(
                    true,
                    Some("current"),
                    || Ok(()),
                    |_, account| {
                        assert_eq!(account, "current");
                        Err(AppError::Message(
                            "agent.transportTimeout: SSH reply lost".into(),
                        ))
                    },
                )?;
                selection_reached.set(true);
                select(
                    &[],
                    &[
                        candidate("other", false, alternatives),
                        candidate("official", true, false),
                    ],
                    "officialAccountPairingRequired",
                )
            })();
            let error = result.unwrap_err().to_string();
            assert_eq!(error, "agent.transportTimeout: SSH reply lost");
            assert!(!error.contains("PairingRequired"));
            assert!(!selection_reached.get());
        }
    }
    #[test]
    fn target_resolution_errors_are_preserved_and_irrelevant_accounts_are_not_queried() {
        let result = observe_official_account::<()>(
            true,
            Some("current"),
            || Err(AppError::Message("unknown remote host: missing".into())),
            |_, _| panic!("no target"),
        );
        assert_eq!(
            result.unwrap_err().to_string(),
            "unknown remote host: missing"
        );
        for (needed, account) in [(false, Some("current")), (true, None)] {
            assert!(observe_official_account::<()>(
                needed,
                account,
                || panic!("not needed"),
                |_, _| panic!("not needed")
            )
            .unwrap()
            .is_none());
        }
        assert!(observe_official_account(
            true,
            Some("current"),
            || Ok(()),
            |_, _| Ok(serde_json::json!({}))
        )
        .unwrap_err()
        .to_string()
        .contains("officialAccountStatusUnavailable"));
    }
    fn candidate(id: &str, official: bool, credential: bool) -> Candidate {
        Candidate {
            catalog_id: id.into(),
            official,
            credential_available: credential,
        }
    }
    #[test]
    fn defaults_choose_only_credential_ready_models_and_skip_optional_chatgpt() {
        let selection = select(
            &[],
            &[
                candidate("bearer", false, true),
                candidate("missing", false, false),
                candidate("official", true, false),
                candidate("free", false, true),
            ],
            "officialAccountPairingRequired",
        )
        .unwrap();
        assert_eq!(selection.catalog_ids, vec!["bearer", "free"]);
        assert_eq!(selection.omitted_catalog_ids, vec!["missing", "official"]);
        assert!(selection.chatgpt_pairing_suggested);
    }
    #[test]
    fn saved_selection_does_not_gain_mandatory_official_or_unrelated_models() {
        let selection = select(
            &["chosen".into(), "deleted".into()],
            &[
                candidate("chosen", false, true),
                candidate("official", true, true),
                candidate("other", false, true),
            ],
            "officialAccountPairingRequired",
        )
        .unwrap();
        assert_eq!(selection.catalog_ids, vec!["chosen"]);
        assert_eq!(selection.omitted_catalog_ids, vec!["deleted"]);
        assert!(!selection.chatgpt_pairing_suggested);
        assert!(select(
            &["deleted".into()],
            &[candidate("other", false, true)],
            "unused"
        )
        .unwrap_err()
        .to_string()
        .contains("noDeployableModels"));
    }
    #[test]
    fn only_chatgpt_requires_its_single_account_remedy() {
        for code in [
            "officialAccountPairingRequired",
            "officialAccountActivationRequired",
            "officialAccountReauthenticationRequired",
            "desktopOfficialAccountMissing",
        ] {
            assert_eq!(
                select(&[], &[candidate("official", true, false)], code)
                    .unwrap_err()
                    .to_string(),
                code
            );
        }
        assert!(select(&[], &[candidate("missing", false, false)], "unused")
            .unwrap_err()
            .to_string()
            .contains("noDeployableModels"));
    }
    #[test]
    fn official_readiness_requires_the_current_desktop_identity_and_active_healthy_grant() {
        let ready = serde_json::json!({"accountId": "current", "active": true, "state": "synchronized", "credentialState": "current"});
        assert!(official_account_ready(Some("current"), Some(&ready)));
        assert!(!official_account_ready(Some("another"), Some(&ready)));
        assert!(!official_account_ready(None, Some(&ready)));
        assert!(!official_account_ready(Some("current"), None));
        for patch in [
            serde_json::json!({"active": false}),
            serde_json::json!({"state": "pairingPending"}),
            serde_json::json!({"credentialState": "expired"}),
            serde_json::json!({"credentialState": "missing"}),
        ] {
            let mut status = ready.clone();
            status
                .as_object_mut()
                .unwrap()
                .extend(patch.as_object().unwrap().clone());
            assert!(!official_account_ready(Some("current"), Some(&status)));
        }
    }
}
