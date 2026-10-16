//! Read-only prerequisites checked before remote deployment mutations.

use crate::error::{AppError, AppResult};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentPreflight {
    pub auto_fix: Vec<String>,
    pub blockers: Vec<String>,
    pub repair_commands: Vec<String>,
}

/// Do not enter the deployment write phase until inspection succeeds.
/// An existing SSH wrapper's idempotent recovery is outside this deployment.
pub(crate) fn before_deployment<C, T>(
    inspect: impl FnOnce() -> AppResult<(DeploymentPreflight, C)>,
    write: impl FnOnce(DeploymentPreflight, C) -> AppResult<T>,
) -> AppResult<T> {
    let (report, context) = inspect()?;
    if !report.blockers.is_empty() {
        return Err(AppError::Message(format!(
            "OneClickBootstrapBlocked: {}; repair: {}",
            report.blockers.join(", "),
            report.repair_commands.join(" | ")
        )));
    }
    write(report, context)
}

pub(crate) const PS_START_TIME_BLOCKER: &str = "psStartTimeUnavailable";

// Probe our remote shell, rather than PID 1 (which may be hidden by procfs
// permissions). No files, daemons, or package installations are involved.
pub(crate) const PS_PROBE_SCRIPT: &str = r#"set -eu
if LC_ALL=C ps -p "$$" -o lstart=; then
  printf '\nVELLUM_PS_PROBE_STATUS=ok\n'
else
  printf '\nVELLUM_PS_PROBE_STATUS=failed\n'
fi
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PsProbeOutput {
    pub success: bool,
    pub stdout: String,
}

/// The only process boundary this preflight needs. Production uses trusted
/// SSH; tests supply BusyBox/procps output without executing any host command.
pub(crate) trait PsProcessClient {
    fn probe_start_time(&self) -> AppResult<PsProbeOutput>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PrerequisiteBlocker {
    pub code: &'static str,
    pub repair: &'static str,
}

pub(crate) fn parse_ps_probe_output(stdout: &str) -> AppResult<PsProbeOutput> {
    let (value, status) = stdout
        .rsplit_once("\nVELLUM_PS_PROBE_STATUS=")
        .ok_or_else(|| AppError::Message("PsStartTimeProbeInvalid: missing probe status".into()))?;
    let success = match status.trim() {
        "ok" => true,
        "failed" => false,
        _ => {
            return Err(AppError::Message(
                "PsStartTimeProbeInvalid: invalid probe status".into(),
            ))
        }
    };
    Ok(PsProbeOutput {
        success,
        stdout: value.trim().to_string(),
    })
}

pub(crate) fn ps_install_remedy(distribution: &str, distribution_like: &str) -> &'static str {
    // Select fixed human-facing hints only. Never construct or execute a
    // command from untrusted /etc/os-release values.
    for id in std::iter::once(distribution).chain(distribution_like.split_whitespace()) {
        match id {
            "alpine" => return "apk add procps-ng",
            "debian" | "ubuntu" | "linuxmint" | "raspbian" => return "apt-get install procps",
            "fedora" | "rhel" | "centos" | "rocky" | "almalinux" => return "dnf install procps-ng",
            "arch" | "manjaro" => return "pacman -S procps-ng",
            "opensuse" | "opensuse-leap" | "opensuse-tumbleweed" | "sles" | "suse" => {
                return "zypper install procps"
            }
            _ => {}
        }
    }
    "Install a ps implementation supporting ps -p <pid> -o lstart=, then retry deployment."
}

pub(crate) fn ps_start_time_preflight(
    os: &str,
    distribution: &str,
    distribution_like: &str,
    client: &dyn PsProcessClient,
) -> AppResult<Option<PrerequisiteBlocker>> {
    if super::platform::normalize_os(os) != Some("linux") {
        return Ok(None);
    }
    let output = client.probe_start_time()?;
    // This is the same contract the pinned Codex PID backend requires: the
    // command must succeed and supply a nonempty process start time. A
    // package name/version check would reject compatible implementations.
    if output.success && !output.stdout.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(PrerequisiteBlocker {
        code: PS_START_TIME_BLOCKER,
        repair: ps_install_remedy(distribution, distribution_like),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn fresh_host_preflight_refusal_makes_no_transport_write_calls() {
        struct FakeTransport {
            writes: std::cell::RefCell<Vec<&'static str>>,
        }
        let transport = FakeTransport {
            writes: Default::default(),
        };
        for code in [
            "unsupportedRemotePlatform",
            "intelMacUnsupported",
            "psStartTimeUnavailable",
            "insufficientDiskSpace",
            "noDeployableModels",
            "bundledReleaseUnavailable",
        ] {
            let result = before_deployment(
                || {
                    Ok((
                        DeploymentPreflight {
                            blockers: vec![code.into()],
                            ..Default::default()
                        },
                        false,
                    ))
                },
                |_, managed_install| {
                    assert!(!managed_install);
                    transport.writes.borrow_mut().extend([
                        "ssh.isolation",
                        "ssh.upload",
                        "credential.put",
                        "codex.bootstrapNative",
                    ]);
                    Ok(())
                },
            );
            assert!(result.unwrap_err().to_string().contains(code));
            assert!(
                transport.writes.borrow().is_empty(),
                "{code} must precede every deployment write"
            );
        }
        let result: AppResult<()> = before_deployment::<(), _>(
            || Err(AppError::Message("SSH unavailable".into())),
            |_, _| {
                transport.writes.borrow_mut().push("ssh.upload");
                Ok(())
            },
        );
        assert_eq!(result.unwrap_err().to_string(), "SSH unavailable");
        assert!(transport.writes.borrow().is_empty());
    }

    #[test]
    fn allowed_preflight_passes_its_fix_list_to_the_write_phase_once() {
        let writes = Cell::new(0);
        before_deployment(
            || {
                Ok((
                    DeploymentPreflight {
                        auto_fix: vec!["agent.update".into()],
                        ..Default::default()
                    },
                    (),
                ))
            },
            |report, _| {
                assert_eq!(report.auto_fix, vec!["agent.update"]);
                writes.set(writes.get() + 1);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(writes.get(), 1);
    }

    struct FakePs {
        output: PsProbeOutput,
        calls: Cell<usize>,
    }

    impl PsProcessClient for FakePs {
        fn probe_start_time(&self) -> AppResult<PsProbeOutput> {
            self.calls.set(self.calls.get() + 1);
            Ok(self.output.clone())
        }
    }

    fn fake(success: bool, stdout: &str) -> FakePs {
        FakePs {
            output: PsProbeOutput {
                success,
                stdout: stdout.into(),
            },
            calls: Cell::new(0),
        }
    }

    #[test]
    fn busybox_without_systemd_blocks_with_alpine_remedy() {
        let client = fake(
            false,
            "ps: unrecognized option: p\nBusyBox v1.36.1\nUsage: ps [-o COL1,COL2=HEADER] [-T]\n",
        );
        let blocker = ps_start_time_preflight("Linux", "alpine", "", &client)
            .unwrap()
            .unwrap();
        assert_eq!(blocker.code, "psStartTimeUnavailable");
        assert_eq!(blocker.repair, "apk add procps-ng");
        assert_eq!(client.calls.get(), 1);
    }

    #[test]
    fn procps_is_allowed_and_empty_output_fails_closed() {
        let procps = fake(true, "Thu Oct 15 23:46:01 2026\n");
        assert!(ps_start_time_preflight("linux", "alpine", "", &procps)
            .unwrap()
            .is_none());
        let empty = fake(true, " \n");
        assert!(ps_start_time_preflight("linux", "alpine", "", &empty)
            .unwrap()
            .is_some());
    }

    #[test]
    fn macos_skips_the_linux_probe() {
        let client = fake(false, "ps: unrecognized option: p");
        assert!(ps_start_time_preflight("darwin", "", "", &client)
            .unwrap()
            .is_none());
        assert_eq!(client.calls.get(), 0);
    }

    #[test]
    fn distro_like_is_a_hint_and_never_interpreted_as_shell_code() {
        assert_eq!(
            ps_install_remedy("derivative", "debian ubuntu"),
            "apt-get install procps"
        );
        assert_eq!(
            ps_install_remedy("rocky", "rhel fedora"),
            "dnf install procps-ng"
        );
        assert_eq!(ps_install_remedy("arch", ""), "pacman -S procps-ng");
        assert_eq!(
            ps_install_remedy("opensuse-leap", "suse"),
            "zypper install procps"
        );
        assert!(ps_install_remedy("alpine; touch /tmp/bad", "")
            .starts_with("Install a ps implementation"));
    }

    #[test]
    fn ssh_probe_protocol_preserves_failure_and_rejects_missing_status() {
        let procps =
            parse_ps_probe_output("Thu Oct 15 23:46:01 2026\n\nVELLUM_PS_PROBE_STATUS=ok\n")
                .unwrap();
        assert!(procps.success && !procps.stdout.is_empty());
        let busybox = parse_ps_probe_output("\nVELLUM_PS_PROBE_STATUS=failed\n").unwrap();
        assert!(!busybox.success);
        assert!(parse_ps_probe_output("SSH closed").is_err());
        assert!(parse_ps_probe_output("\nVELLUM_PS_PROBE_STATUS=unknown\n").is_err());
    }

    #[test]
    fn transport_errors_are_not_mislabeled_as_missing_procps() {
        struct Unreachable;
        impl PsProcessClient for Unreachable {
            fn probe_start_time(&self) -> AppResult<PsProbeOutput> {
                Err(AppError::Message("SSH unavailable".into()))
            }
        }
        assert!(ps_start_time_preflight("linux", "alpine", "", &Unreachable)
            .unwrap_err()
            .to_string()
            .contains("SSH unavailable"));
    }
}
