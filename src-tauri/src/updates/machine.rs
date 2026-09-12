//! Persistent update phase machine.
//!
//! `checking → available → downloading → verifying → staged →
//! waitingForIdle | waitingForRestart → applying → validating → applied`
//! with `blocked` / `failed` / `rolledBack` carrying reason context in the
//! journal, not in the phase enum.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UpdatePhase {
    Idle,
    Checking,
    Available,
    Downloading,
    Verifying,
    Staged,
    WaitingForIdle,
    WaitingForRestart,
    Applying,
    Validating,
    Applied,
    Blocked,
    Failed,
    RolledBack,
}

impl UpdatePhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Checking => "checking",
            Self::Available => "available",
            Self::Downloading => "downloading",
            Self::Verifying => "verifying",
            Self::Staged => "staged",
            Self::WaitingForIdle => "waitingForIdle",
            Self::WaitingForRestart => "waitingForRestart",
            Self::Applying => "applying",
            Self::Validating => "validating",
            Self::Applied => "applied",
            Self::Blocked => "blocked",
            Self::Failed => "failed",
            Self::RolledBack => "rolledBack",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateEvent {
    StartCheck,
    FoundAvailable,
    NoneAvailable,
    StartDownload,
    DownloadProgress,
    StartVerify,
    Staged { wait: WaitKind },
    StartApply,
    StartValidate,
    Succeeded,
    Block { recoverable: bool },
    Fail,
    Rollback,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitKind {
    Idle,
    Restart,
}

impl WaitKind {
    pub fn as_restart(self) -> bool {
        matches!(self, Self::Restart)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionError {
    pub from: UpdatePhase,
    pub event: &'static str,
}

pub fn transition(from: UpdatePhase, event: &UpdateEvent) -> Result<UpdatePhase, TransitionError> {
    let next = match (from, event) {
        (_, UpdateEvent::StartCheck) => UpdatePhase::Checking,
        (UpdatePhase::Checking, UpdateEvent::FoundAvailable) => UpdatePhase::Available,
        (UpdatePhase::Checking, UpdateEvent::NoneAvailable) => UpdatePhase::Idle,
        (
            UpdatePhase::Available | UpdatePhase::Idle | UpdatePhase::Failed,
            UpdateEvent::StartDownload,
        ) => UpdatePhase::Downloading,
        (UpdatePhase::Downloading, UpdateEvent::DownloadProgress) => UpdatePhase::Downloading,
        (UpdatePhase::Downloading, UpdateEvent::StartVerify) => UpdatePhase::Verifying,
        (
            UpdatePhase::Verifying,
            UpdateEvent::Staged {
                wait: WaitKind::Idle,
            },
        ) => UpdatePhase::WaitingForIdle,
        (
            UpdatePhase::Verifying,
            UpdateEvent::Staged {
                wait: WaitKind::Restart,
            },
        ) => UpdatePhase::WaitingForRestart,
        (
            UpdatePhase::Staged | UpdatePhase::WaitingForIdle | UpdatePhase::WaitingForRestart,
            UpdateEvent::StartApply,
        ) => UpdatePhase::Applying,
        (UpdatePhase::Applying, UpdateEvent::StartValidate) => UpdatePhase::Validating,
        (UpdatePhase::Validating, UpdateEvent::Succeeded) => UpdatePhase::Applied,
        (_, UpdateEvent::Block { .. }) => UpdatePhase::Blocked,
        (_, UpdateEvent::Fail) => UpdatePhase::Failed,
        (_, UpdateEvent::Rollback) => UpdatePhase::RolledBack,
        (UpdatePhase::Downloading | UpdatePhase::Verifying, UpdateEvent::Cancel) => {
            UpdatePhase::Available
        }
        _ => {
            return Err(TransitionError {
                from,
                event: event_name(event),
            })
        }
    };
    Ok(next)
}

fn event_name(event: &UpdateEvent) -> &'static str {
    match event {
        UpdateEvent::StartCheck => "startCheck",
        UpdateEvent::FoundAvailable => "foundAvailable",
        UpdateEvent::NoneAvailable => "noneAvailable",
        UpdateEvent::StartDownload => "startDownload",
        UpdateEvent::DownloadProgress => "downloadProgress",
        UpdateEvent::StartVerify => "startVerify",
        UpdateEvent::Staged { .. } => "staged",
        UpdateEvent::StartApply => "startApply",
        UpdateEvent::StartValidate => "startValidate",
        UpdateEvent::Succeeded => "succeeded",
        UpdateEvent::Block { .. } => "block",
        UpdateEvent::Fail => "fail",
        UpdateEvent::Rollback => "rollback",
        UpdateEvent::Cancel => "cancel",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path_reaches_applied() {
        let mut phase = UpdatePhase::Idle;
        for event in [
            UpdateEvent::StartCheck,
            UpdateEvent::FoundAvailable,
            UpdateEvent::StartDownload,
            UpdateEvent::StartVerify,
            UpdateEvent::Staged {
                wait: WaitKind::Restart,
            },
            UpdateEvent::StartApply,
            UpdateEvent::StartValidate,
            UpdateEvent::Succeeded,
        ] {
            phase = transition(phase, &event).expect("legal");
        }
        assert_eq!(phase, UpdatePhase::Applied);
    }

    #[test]
    fn cancel_returns_to_available_not_idle() {
        let phase = transition(UpdatePhase::Downloading, &UpdateEvent::Cancel).unwrap();
        assert_eq!(phase, UpdatePhase::Available);
    }

    #[test]
    fn illegal_jump_is_rejected() {
        assert!(transition(UpdatePhase::Idle, &UpdateEvent::Succeeded).is_err());
    }
}
