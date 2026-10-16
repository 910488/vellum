//! Local intent only. Deployment always runs through the existing controlled converge.
use super::{deployment::RemoteModelOption, desired_state::RemoteHostDesiredState};
use crate::error::{AppError, AppResult};
use crate::state::AppState;
use std::collections::BTreeSet;

fn selection(
    mut desired: RemoteHostDesiredState,
    options: &[RemoteModelOption],
    ids: &[String],
) -> AppResult<RemoteHostDesiredState> {
    let ids = ids.iter().cloned().collect::<BTreeSet<_>>();
    if ids.is_empty() {
        return Err(AppError::Message("noModelsSelected".into()));
    }
    if options
        .iter()
        .any(|model| model.mandatory && !ids.contains(&model.catalog_id))
    {
        return Err(AppError::Message("RemoteMandatoryModelRequired".into()));
    }
    if ids
        .iter()
        .any(|id| !options.iter().any(|model| &model.catalog_id == id))
    {
        return Err(AppError::Message("RemoteModelUnavailable".into()));
    }
    desired.selected_catalog_ids = ids.into_iter().collect();
    desired.deferred_catalog_ids.clear();
    desired.last_plan_id = None;
    desired.desired_revision = desired
        .desired_revision
        .checked_add(1)
        .ok_or_else(|| AppError::Message("RemoteDesiredRevisionOverflow".into()))?;
    Ok(desired)
}

/// Called only after acquiring the same host registration as bootstrap.
pub(crate) fn prepare_selection(
    state: &AppState,
    host_id: &str,
    ids: &[String],
) -> AppResult<RemoteHostDesiredState> {
    let options = super::deployment::model_options(state, host_id)?;
    let desired = super::desired_state::load(&state.data_root(), host_id)?;
    selection(desired, &options, ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn options() -> Vec<RemoteModelOption> {
        ["official", "other"]
            .into_iter()
            .map(|id| RemoteModelOption {
                catalog_id: id.into(),
                display_name: id.into(),
                route_id: id.into(),
                upstream_model: id.into(),
                selected: true,
                mandatory: id == "official",
            })
            .collect()
    }
    #[test]
    fn changing_models_preserves_host_policy_overrides_and_observed_revision() {
        let desired = RemoteHostDesiredState {
            desired_revision: 3,
            observed_revision: 2,
            policy_overrides: super::super::deployment::RemotePolicyOverrides {
                auto_review_enabled: Some(false),
                compaction_threshold_percent: Some(71),
                standalone_web_search: Some(false),
            },
            ..Default::default()
        };
        let changed = selection(desired.clone(), &options(), &["official".into()]).unwrap();
        assert_eq!(changed.policy_overrides, desired.policy_overrides);
        assert_eq!(changed.observed_revision, 2);
        assert_eq!(changed.desired_revision, 4);
        assert_eq!(changed.selected_catalog_ids, vec!["official"]);
    }
    #[test]
    fn mandatory_unknown_and_empty_selections_are_rejected_by_backend() {
        for (ids, error) in [
            (vec!["other".into()], "RemoteMandatoryModelRequired"),
            (
                vec!["official".into(), "unknown".into()],
                "RemoteModelUnavailable",
            ),
            (vec![], "noModelsSelected"),
        ] {
            assert_eq!(
                selection(Default::default(), &options(), &ids)
                    .unwrap_err()
                    .to_string(),
                error
            );
        }
    }
    #[test]
    fn model_listing_does_not_write_desired_state_or_create_a_plan() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        state
            .remote()
            .import_discovered_host(&super::super::discovery::RemoteHostCandidate {
                codex_host_id: None,
                vellum_host_id: "models".into(),
                display_name: "models".into(),
                ssh_alias: "models".into(),
                hostname: Some("example.invalid".into()),
                user: Some("test".into()),
                port: Some(22),
                source: "openSsh".into(),
                validated: false,
                validation_error: None,
            })
            .unwrap();
        let desired = RemoteHostDesiredState {
            host_id: "models".into(),
            ..Default::default()
        };
        super::super::desired_state::save(temp.path(), &desired).unwrap();
        let path = temp.path().join("remote-desired-state/models.json");
        let before = std::fs::read(&path).unwrap();
        super::super::deployment::model_options(&state, "models").unwrap();
        assert_eq!(std::fs::read(path).unwrap(), before);
        assert!(!temp.path().join("remote-deployment-plans").exists());
        std::fs::remove_file(temp.path().join("remote-desired-state/models.json")).unwrap();
        super::super::deployment::model_options(&state, "models").unwrap();
        assert!(!temp
            .path()
            .join("remote-desired-state/models.json")
            .exists());
        super::super::deployment::model_options(&state, "missing").unwrap_err();
        assert!(!temp
            .path()
            .join("remote-desired-state/missing.json")
            .exists());
    }
}
