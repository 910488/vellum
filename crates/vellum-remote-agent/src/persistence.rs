//! Observed persistence, independent of the platform's preferred manager.

use crate::native_proxy::{ProcessServiceCommandRunner, ServiceCommandRunner};

pub const ON_DEMAND: &str = "onDemand";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinuxPersistence {
    pub systemd_user: bool,
    pub linger: bool,
}

impl LinuxPersistence {
    pub fn scope(self) -> &'static str {
        match (self.systemd_user, self.linger) {
            (true, true) => "linger",
            (true, false) => "login",
            (false, _) => ON_DEMAND,
        }
    }

    pub fn service_manager(self) -> &'static str {
        if self.systemd_user {
            "systemd-user"
        } else {
            "agent-supervisor"
        }
    }
}

pub fn probe_with(client: &dyn ServiceCommandRunner) -> LinuxPersistence {
    let systemd_user = client
        .run("systemctl", &["--user", "show-environment"])
        .is_ok_and(|output| output.success);
    let linger = systemd_user
        && client
            .run("id", &["-un"])
            .ok()
            .filter(|output| output.success)
            .is_some_and(|user| {
                client
                    .run(
                        "loginctl",
                        &["show-user", user.stdout.trim(), "-p", "Linger", "--value"],
                    )
                    .is_ok_and(|output| output.success && output.stdout.trim() == "yes")
            });
    LinuxPersistence {
        systemd_user,
        linger,
    }
}

pub fn probe() -> LinuxPersistence {
    probe_with(&ProcessServiceCommandRunner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_proxy::ServiceCommandOutput;

    struct FakeServices {
        systemd: bool,
        linger: bool,
    }
    impl ServiceCommandRunner for FakeServices {
        fn run(&self, program: &str, _args: &[&str]) -> Result<ServiceCommandOutput, String> {
            Ok(ServiceCommandOutput {
                success: program != "systemctl" || self.systemd,
                stdout: match program {
                    "id" => "remote".into(),
                    "loginctl" if self.linger => "yes\n".into(),
                    _ => String::new(),
                },
                stderr: String::new(),
            })
        }
    }

    #[test]
    fn observed_services_select_linger_login_or_on_demand() {
        for (systemd, linger, scope) in [
            (true, true, "linger"),
            (true, false, "login"),
            (false, true, ON_DEMAND),
            (false, false, ON_DEMAND),
        ] {
            let observed = probe_with(&FakeServices { systemd, linger });
            assert_eq!(observed.scope(), scope);
            assert_eq!(
                observed.service_manager(),
                if systemd {
                    "systemd-user"
                } else {
                    "agent-supervisor"
                }
            );
        }
    }
}
