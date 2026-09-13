//! OS + architecture identity for Remote Manager hosts.
//!
//! Artifact pick is always OS+arch. Linux x64/ARM64 stay first-class Docker
//! targets. Apple Silicon is a first-class native-proxy target. Intel Mac is
//! recognised so the UI can say 尚未支援, but it is never deployable.

use std::path::{Path, PathBuf};

/// Criterion 5 (Codex App native SSH isolation on a real Apple Silicon host)
/// must pass before macOS is marked deployable. Source that the launcher
/// exports `CODEX_HOME` is not that evidence.
pub const MACOS_ISOLATION_GATE_PASSED: bool = false;

pub const LINUX_PROXY_PORT: u16 = 15721;
pub const DARWIN_PROXY_PORT: u16 = 15722;
pub const LINUX_PROXY_LISTEN: &str = "127.0.0.1:15721";
pub const DARWIN_PROXY_LISTEN: &str = "127.0.0.1:15722";

pub const PROXY_BACKEND_DOCKER: &str = "docker";
pub const PROXY_BACKEND_NATIVE: &str = "native";
pub const SERVICE_MANAGER_SYSTEMD_USER: &str = "systemd-user";
pub const SERVICE_MANAGER_LAUNCHD: &str = "launchd";
pub const PERSISTENCE_LINGER: &str = "linger";
pub const PERSISTENCE_LOGIN: &str = "login";

pub const PLATFORM_LINUX_X64: &str = "linux-x64";
pub const PLATFORM_LINUX_ARM64: &str = "linux-arm64";
pub const PLATFORM_DARWIN_ARM64: &str = "darwin-arm64";
pub const RUST_TARGET_DARWIN_ARM64: &str = "aarch64-apple-darwin";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemotePlatform {
    LinuxAmd64,
    LinuxArm64,
    DarwinArm64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyBackend {
    Docker,
    Native,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedPlatform {
    pub os: String,
    pub arch: String,
    pub code: &'static str,
    pub message: String,
    /// Shown in the UI when the host is an Intel Mac.
    pub ui_unsupported: bool,
}

impl RemotePlatform {
    pub fn from_os_arch(os: &str, arch: &str) -> Result<Self, UnsupportedPlatform> {
        match (normalize_os(os), normalize_arch(arch)) {
            (Some("linux"), Some("amd64")) => Ok(Self::LinuxAmd64),
            (Some("linux"), Some("arm64")) => Ok(Self::LinuxArm64),
            (Some("darwin"), Some("arm64")) => Ok(Self::DarwinArm64),
            (Some("darwin"), Some("amd64")) => Err(UnsupportedPlatform {
                os: os.trim().to_string(),
                arch: arch.trim().to_string(),
                code: "intelMacUnsupported",
                message: "尚未支援".into(),
                ui_unsupported: true,
            }),
            (os_norm, arch_norm) => Err(UnsupportedPlatform {
                os: os.trim().to_string(),
                arch: arch.trim().to_string(),
                code: "unsupportedRemotePlatform",
                message: format!(
                    "unsupported remote platform: {}/{}",
                    os_norm.unwrap_or(os),
                    arch_norm.unwrap_or(arch)
                ),
                ui_unsupported: false,
            }),
        }
    }

    pub fn artifact_key(self) -> &'static str {
        match self {
            Self::LinuxAmd64 => PLATFORM_LINUX_X64,
            Self::LinuxArm64 => PLATFORM_LINUX_ARM64,
            Self::DarwinArm64 => PLATFORM_DARWIN_ARM64,
        }
    }

    pub fn rust_target(self) -> &'static str {
        match self {
            Self::LinuxAmd64 => "x86_64-unknown-linux-gnu",
            Self::LinuxArm64 => "aarch64-unknown-linux-gnu",
            Self::DarwinArm64 => RUST_TARGET_DARWIN_ARM64,
        }
    }

    pub fn bundle_dir(self) -> &'static str {
        match self {
            Self::LinuxAmd64 => "linux-amd64",
            Self::LinuxArm64 => "linux-arm64",
            Self::DarwinArm64 => "darwin-arm64",
        }
    }

    pub fn proxy_backend(self) -> ProxyBackend {
        match self {
            Self::LinuxAmd64 | Self::LinuxArm64 => ProxyBackend::Docker,
            Self::DarwinArm64 => ProxyBackend::Native,
        }
    }

    pub fn proxy_backend_name(self) -> &'static str {
        match self.proxy_backend() {
            ProxyBackend::Docker => PROXY_BACKEND_DOCKER,
            ProxyBackend::Native => PROXY_BACKEND_NATIVE,
        }
    }

    pub fn service_manager(self) -> &'static str {
        match self {
            Self::LinuxAmd64 | Self::LinuxArm64 => SERVICE_MANAGER_SYSTEMD_USER,
            Self::DarwinArm64 => SERVICE_MANAGER_LAUNCHD,
        }
    }

    pub fn persistence_scope(self) -> &'static str {
        match self {
            Self::LinuxAmd64 | Self::LinuxArm64 => PERSISTENCE_LINGER,
            Self::DarwinArm64 => PERSISTENCE_LOGIN,
        }
    }

    pub fn default_proxy_port(self) -> u16 {
        match self {
            Self::LinuxAmd64 | Self::LinuxArm64 => LINUX_PROXY_PORT,
            Self::DarwinArm64 => DARWIN_PROXY_PORT,
        }
    }

    pub fn default_proxy_listen(self) -> &'static str {
        match self {
            Self::LinuxAmd64 | Self::LinuxArm64 => LINUX_PROXY_LISTEN,
            Self::DarwinArm64 => DARWIN_PROXY_LISTEN,
        }
    }

    /// Linux remains deployable on agent protocols 1–3. macOS features require
    /// agent protocol 4, and only after the isolation gate.
    pub fn deployable(self) -> bool {
        match self {
            Self::LinuxAmd64 | Self::LinuxArm64 => true,
            Self::DarwinArm64 => MACOS_ISOLATION_GATE_PASSED,
        }
    }

    pub fn requires_agent_protocol(self) -> u32 {
        match self {
            Self::LinuxAmd64 | Self::LinuxArm64 => 1,
            Self::DarwinArm64 => 4,
        }
    }

    pub fn docker_is_blocker(self) -> bool {
        matches!(self.proxy_backend(), ProxyBackend::Docker)
    }

    pub fn broker_required(self) -> bool {
        matches!(self, Self::LinuxAmd64 | Self::LinuxArm64)
    }
}

impl ProxyBackend {
    pub fn parse(value: Option<&str>) -> Self {
        match value {
            Some(PROXY_BACKEND_NATIVE) => Self::Native,
            _ => Self::Docker,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Docker => PROXY_BACKEND_DOCKER,
            Self::Native => PROXY_BACKEND_NATIVE,
        }
    }
}

pub fn normalize_os(value: &str) -> Option<&'static str> {
    match value.trim().to_ascii_lowercase().as_str() {
        "linux" => Some("linux"),
        "darwin" | "macos" | "osx" => Some("darwin"),
        _ => None,
    }
}

pub fn normalize_arch(value: &str) -> Option<&'static str> {
    match value.trim() {
        "x86_64" | "amd64" => Some("amd64"),
        "aarch64" | "arm64" => Some("arm64"),
        _ => None,
    }
}

pub fn current_platform() -> Result<RemotePlatform, UnsupportedPlatform> {
    RemotePlatform::from_os_arch(std::env::consts::OS, std::env::consts::ARCH)
}

/// Remote Codex home. macOS uses the short path so the Unix control socket
/// stays under typical `sockaddr_un` limits. Linux keeps `~/.codex`.
pub fn managed_codex_home(user_home: &Path, os: &str) -> PathBuf {
    match normalize_os(os) {
        Some("darwin") => user_home.join(".vellum-remote").join("codex"),
        _ => user_home.join(".codex"),
    }
}

/// Remote agent/proxy state root. On macOS this is Application Support.
pub fn managed_state_root(data_local_dir: &Path) -> PathBuf {
    data_local_dir.join("vellum-remote")
}

pub fn desktop_accepts_agent_protocol(protocol: u64, platform: Option<RemotePlatform>) -> bool {
    match platform {
        Some(RemotePlatform::DarwinArm64) => protocol >= 4,
        _ => matches!(protocol, 1..=4),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_plus_arch_keeps_linux_and_adds_apple_silicon() {
        assert_eq!(
            RemotePlatform::from_os_arch("Linux", "x86_64").unwrap(),
            RemotePlatform::LinuxAmd64
        );
        assert_eq!(
            RemotePlatform::from_os_arch("linux", "amd64").unwrap(),
            RemotePlatform::LinuxAmd64
        );
        assert_eq!(
            RemotePlatform::from_os_arch("Linux", "aarch64").unwrap(),
            RemotePlatform::LinuxArm64
        );
        assert_eq!(
            RemotePlatform::from_os_arch("linux", "arm64").unwrap(),
            RemotePlatform::LinuxArm64
        );
        assert_eq!(
            RemotePlatform::from_os_arch("Darwin", "arm64").unwrap(),
            RemotePlatform::DarwinArm64
        );
        assert_eq!(
            RemotePlatform::from_os_arch("macos", "aarch64").unwrap(),
            RemotePlatform::DarwinArm64
        );
        assert_eq!(
            RemotePlatform::DarwinArm64.artifact_key(),
            "darwin-arm64"
        );
        assert_eq!(
            RemotePlatform::DarwinArm64.rust_target(),
            "aarch64-apple-darwin"
        );
    }

    #[test]
    fn intel_mac_is_shown_unsupported_and_is_not_deployable() {
        let error = RemotePlatform::from_os_arch("Darwin", "x86_64").unwrap_err();
        assert_eq!(error.code, "intelMacUnsupported");
        assert_eq!(error.message, "尚未支援");
        assert!(error.ui_unsupported);
        assert!(RemotePlatform::from_os_arch("darwin", "amd64").is_err());
    }

    #[test]
    fn linux_stays_docker_macos_is_native_and_not_blocked_by_docker() {
        assert_eq!(
            RemotePlatform::LinuxAmd64.proxy_backend(),
            ProxyBackend::Docker
        );
        assert_eq!(
            RemotePlatform::LinuxArm64.proxy_backend(),
            ProxyBackend::Docker
        );
        assert_eq!(
            RemotePlatform::DarwinArm64.proxy_backend(),
            ProxyBackend::Native
        );
        assert!(RemotePlatform::LinuxAmd64.docker_is_blocker());
        assert!(!RemotePlatform::DarwinArm64.docker_is_blocker());
        assert!(!RemotePlatform::DarwinArm64.broker_required());
        assert_eq!(
            RemotePlatform::DarwinArm64.service_manager(),
            SERVICE_MANAGER_LAUNCHD
        );
        assert_eq!(
            RemotePlatform::DarwinArm64.persistence_scope(),
            PERSISTENCE_LOGIN
        );
        assert_eq!(RemotePlatform::DarwinArm64.default_proxy_port(), 15722);
        assert_eq!(
            RemotePlatform::DarwinArm64.default_proxy_listen(),
            "127.0.0.1:15722"
        );
        assert_eq!(RemotePlatform::LinuxAmd64.default_proxy_port(), 15721);
    }

    #[test]
    fn macos_is_not_deployable_until_the_isolation_gate_passes() {
        assert!(RemotePlatform::LinuxAmd64.deployable());
        assert!(RemotePlatform::LinuxArm64.deployable());
        assert!(!RemotePlatform::DarwinArm64.deployable());
        assert_eq!(RemotePlatform::DarwinArm64.requires_agent_protocol(), 4);
        assert_eq!(RemotePlatform::LinuxAmd64.requires_agent_protocol(), 1);
    }

    #[test]
    fn protocol_4_is_required_for_macos_while_linux_1_to_3_still_parse() {
        assert!(desktop_accepts_agent_protocol(1, Some(RemotePlatform::LinuxAmd64)));
        assert!(desktop_accepts_agent_protocol(2, Some(RemotePlatform::LinuxArm64)));
        assert!(desktop_accepts_agent_protocol(3, None));
        assert!(desktop_accepts_agent_protocol(4, Some(RemotePlatform::LinuxAmd64)));
        assert!(desktop_accepts_agent_protocol(
            4,
            Some(RemotePlatform::DarwinArm64)
        ));
        assert!(!desktop_accepts_agent_protocol(
            3,
            Some(RemotePlatform::DarwinArm64)
        ));
        assert!(!desktop_accepts_agent_protocol(
            1,
            Some(RemotePlatform::DarwinArm64)
        ));
        assert!(!desktop_accepts_agent_protocol(0, None));
        assert!(!desktop_accepts_agent_protocol(5, None));
    }

    #[test]
    fn isolation_paths_are_the_short_codex_home_and_application_support_state() {
        let home = Path::new("/Users/joshhuang");
        assert_eq!(
            managed_codex_home(home, "darwin"),
            Path::new("/Users/joshhuang/.vellum-remote/codex")
        );
        assert_eq!(
            managed_codex_home(home, "macos"),
            Path::new("/Users/joshhuang/.vellum-remote/codex")
        );
        assert_eq!(
            managed_codex_home(Path::new("/home/operator"), "linux"),
            Path::new("/home/operator/.codex")
        );
        assert_eq!(
            managed_state_root(Path::new(
                "/Users/joshhuang/Library/Application Support"
            )),
            Path::new("/Users/joshhuang/Library/Application Support/vellum-remote")
        );
    }

    #[test]
    fn unmarked_install_backend_defaults_to_docker() {
        assert_eq!(ProxyBackend::parse(None), ProxyBackend::Docker);
        assert_eq!(ProxyBackend::parse(Some("")), ProxyBackend::Docker);
        assert_eq!(ProxyBackend::parse(Some("docker")), ProxyBackend::Docker);
        assert_eq!(ProxyBackend::parse(Some("native")), ProxyBackend::Native);
    }
}
