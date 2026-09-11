//! Explicit, serializable executor capabilities (plan §25.2).
//!
//! The shell contract a model sees must describe the environment that
//! *actually* executes Codex tools, never the environment the proxy process
//! happens to live in. [`ExecutionEnvironment`] is that explicit value: it is
//! resolved by the host that owns execution (Desktop's local Codex executor,
//! a remote agent's Jetson/native executor) and passed into the runtime per
//! request. The Docker daemon is never allowed to derive it from the
//! container's own environment and call that the Codex executor contract.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimePlatform {
    Windows,
    Linux,
    Macos,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeShellKind {
    Pwsh,
    Powershell,
    GitBash,
    Cmd,
    Bash,
    Zsh,
}

impl RuntimeShellKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pwsh => "pwsh",
            Self::Powershell => "powershell",
            Self::GitBash => "git-bash",
            Self::Cmd => "cmd",
            Self::Bash => "bash",
            Self::Zsh => "zsh",
        }
    }

    pub fn is_powershell_family(self) -> bool {
        matches!(self, Self::Pwsh | Self::Powershell)
    }

    pub fn is_posix_family(self) -> bool {
        matches!(self, Self::GitBash | Self::Bash | Self::Zsh)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeAmpersandSemantics {
    PosixBackground,
    PowershellCore,
    WindowsPowershell,
    CmdSeparator,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimePathStyle {
    Windows,
    Posix,
}

/// Optional capabilities the executor host has verified end to end. A
/// capability is advertised only after it has been proven on this host —
/// code merely existing never proves it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExecutorCapability {
    /// Direct argv-safe execution of single programs is available.
    ArgvSafeExec,
    /// Genuine shell scripts (pipelines, redirection, builtins) are supported.
    ShellScripts,
    /// Dedicated search tools resolve on this executor.
    SearchTools,
}

/// One host's proven model-visible execution contract.
///
/// `platform` and `shell` identify *where* the tool will run, not where the
/// proxy runs. `supports_and_and` / `has_unix_utilities` / `path_style` /
/// `ampersand_semantics` are the facts prompt generation needs to describe
/// that shell without guessing. `verified_capabilities` carries the optional
/// features this host has actually proven.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionEnvironment {
    pub platform: RuntimePlatform,
    pub shell: RuntimeShellKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell_version: Option<String>,
    pub supports_and_and: bool,
    pub has_unix_utilities: bool,
    pub path_style: RuntimePathStyle,
    pub ampersand_semantics: RuntimeAmpersandSemantics,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verified_capabilities: Vec<ExecutorCapability>,
}

impl ExecutionEnvironment {
    /// A conservative POSIX reference, only meaningful as a test/fallback
    /// starting point. Production hosts must resolve and pass the real value;
    /// this is explicit so a caller that forgets to resolve one can never be
    /// mistaken for a real executor contract.
    pub fn posix_reference() -> Self {
        Self {
            platform: RuntimePlatform::Linux,
            shell: RuntimeShellKind::Bash,
            shell_version: None,
            supports_and_and: true,
            has_unix_utilities: true,
            path_style: RuntimePathStyle::Posix,
            ampersand_semantics: RuntimeAmpersandSemantics::PosixBackground,
            verified_capabilities: Vec::new(),
        }
    }

    /// Whether the advertised shell is a PowerShell-family one (drives
    /// quoting/guidance the same way across Desktop and the daemon).
    pub fn is_powershell_family(&self) -> bool {
        self.shell.is_powershell_family()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_serializes_and_round_trips() {
        let environment = ExecutionEnvironment {
            platform: RuntimePlatform::Windows,
            shell: RuntimeShellKind::Pwsh,
            shell_version: Some("7.5.0".into()),
            supports_and_and: true,
            has_unix_utilities: false,
            path_style: RuntimePathStyle::Windows,
            ampersand_semantics: RuntimeAmpersandSemantics::PowershellCore,
            verified_capabilities: vec![ExecutorCapability::ArgvSafeExec],
        };
        let encoded = serde_json::to_value(&environment).unwrap();
        assert_eq!(encoded["platform"], "windows");
        assert_eq!(encoded["shell"], "pwsh");
        assert_eq!(encoded["ampersandSemantics"], "powershell-core");
        let decoded: ExecutionEnvironment = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, environment);
        assert!(decoded.is_powershell_family());
    }

    #[test]
    fn posix_reference_is_an_explicit_marker_not_a_detected_value() {
        let reference = ExecutionEnvironment::posix_reference();
        assert_eq!(reference.platform, RuntimePlatform::Linux);
        assert_eq!(reference.shell, RuntimeShellKind::Bash);
        assert!(reference.verified_capabilities.is_empty());
    }

    #[test]
    fn shell_family_classification_matches_the_desktop_rules() {
        assert!(!ExecutionEnvironment::posix_reference().is_powershell_family());
        let cmd = ExecutionEnvironment {
            platform: RuntimePlatform::Windows,
            shell: RuntimeShellKind::Cmd,
            shell_version: None,
            supports_and_and: true,
            has_unix_utilities: false,
            path_style: RuntimePathStyle::Windows,
            ampersand_semantics: RuntimeAmpersandSemantics::CmdSeparator,
            verified_capabilities: Vec::new(),
        };
        assert!(!cmd.is_powershell_family());
        assert_eq!(RuntimeShellKind::Cmd.as_str(), "cmd");
    }
}
