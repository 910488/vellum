//! Terminal capability handshake and the argv-safe execution contract.
//!
//! Issue #6 Phase 4. Codex's native shell spec tells the model exactly which
//! shell it is driving, keeps `command` / `workdir` / timeout separate, and
//! lets the runtime derive the executable arguments. A translated harness that
//! flattens argv into one string loses quoting and argument boundaries, so the
//! model learns to reconstruct commands by string surgery — which is where the
//! 2026-08-02 session lost most of its non-zero exits.
//!
//! Everything here is pure over a [`ShellProbe`] so the Windows/Git Bash/POSIX
//! matrix is testable without spawning a shell.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Windows,
    Linux,
    Macos,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ShellKind {
    Pwsh,
    Powershell,
    GitBash,
    Cmd,
    Bash,
    Zsh,
}

impl ShellKind {
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
pub enum AmpersandSemantics {
    PosixBackground,
    PowershellCore,
    WindowsPowershell,
    CmdSeparator,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PathStyle {
    Windows,
    Posix,
}

/// Mirrors the `TerminalCapabilities` shape in the issue. Detected once per
/// session and carried across compaction so the prompt and the tool
/// descriptions never disagree about which shell is actually running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalCapabilities {
    pub platform: Platform,
    pub shell: ShellKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell_version: Option<String>,
    pub supports_and_and: bool,
    pub has_unix_utilities: bool,
    pub ampersand_semantics: AmpersandSemantics,
    pub path_style: PathStyle,
    pub text_encoding: String,
}

/// Shared environment adapter: the host that owns execution resolves its
/// proven contract as [`crate::environment::ExecutionEnvironment`], and this
/// legacy shell record converts onto it. `verified_capabilities` stays empty
/// until a capability has been proven end to end — code merely existing never
/// proves it.
impl From<&TerminalCapabilities> for crate::environment::ExecutionEnvironment {
    fn from(value: &TerminalCapabilities) -> Self {
        use crate::environment::{
            ExecutionEnvironment, RuntimeAmpersandSemantics, RuntimePathStyle, RuntimePlatform,
            RuntimeShellKind,
        };
        let platform = match value.platform {
            Platform::Windows => RuntimePlatform::Windows,
            Platform::Linux => RuntimePlatform::Linux,
            Platform::Macos => RuntimePlatform::Macos,
        };
        let shell = match value.shell {
            ShellKind::Pwsh => RuntimeShellKind::Pwsh,
            ShellKind::Powershell => RuntimeShellKind::Powershell,
            ShellKind::GitBash => RuntimeShellKind::GitBash,
            ShellKind::Cmd => RuntimeShellKind::Cmd,
            ShellKind::Bash => RuntimeShellKind::Bash,
            ShellKind::Zsh => RuntimeShellKind::Zsh,
        };
        let ampersand_semantics = match value.ampersand_semantics {
            AmpersandSemantics::PosixBackground => RuntimeAmpersandSemantics::PosixBackground,
            AmpersandSemantics::PowershellCore => RuntimeAmpersandSemantics::PowershellCore,
            AmpersandSemantics::WindowsPowershell => RuntimeAmpersandSemantics::WindowsPowershell,
            AmpersandSemantics::CmdSeparator => RuntimeAmpersandSemantics::CmdSeparator,
        };
        let path_style = match value.path_style {
            PathStyle::Windows => RuntimePathStyle::Windows,
            PathStyle::Posix => RuntimePathStyle::Posix,
        };
        ExecutionEnvironment {
            platform,
            shell,
            shell_version: value.shell_version.clone(),
            supports_and_and: value.supports_and_and,
            has_unix_utilities: value.has_unix_utilities,
            path_style,
            ampersand_semantics,
            verified_capabilities: Vec::new(),
        }
    }
}

/// Bridge back from the explicit executor contract to the legacy shell record
/// prompt/snapshot generation consume. The round-trip is lossless: every field
/// on [`crate::environment::ExecutionEnvironment`] maps onto this type, and
/// `text_encoding` is the constant `utf-8` the probe always produced anyway.
impl From<&crate::environment::ExecutionEnvironment> for TerminalCapabilities {
    fn from(value: &crate::environment::ExecutionEnvironment) -> Self {
        use crate::environment::{RuntimeAmpersandSemantics, RuntimePathStyle, RuntimePlatform};
        let platform = match value.platform {
            RuntimePlatform::Windows => Platform::Windows,
            RuntimePlatform::Linux => Platform::Linux,
            RuntimePlatform::Macos => Platform::Macos,
        };
        let shell = match value.shell {
            crate::environment::RuntimeShellKind::Pwsh => ShellKind::Pwsh,
            crate::environment::RuntimeShellKind::Powershell => ShellKind::Powershell,
            crate::environment::RuntimeShellKind::GitBash => ShellKind::GitBash,
            crate::environment::RuntimeShellKind::Cmd => ShellKind::Cmd,
            crate::environment::RuntimeShellKind::Bash => ShellKind::Bash,
            crate::environment::RuntimeShellKind::Zsh => ShellKind::Zsh,
        };
        let ampersand_semantics = match value.ampersand_semantics {
            RuntimeAmpersandSemantics::PosixBackground => AmpersandSemantics::PosixBackground,
            RuntimeAmpersandSemantics::PowershellCore => AmpersandSemantics::PowershellCore,
            RuntimeAmpersandSemantics::WindowsPowershell => AmpersandSemantics::WindowsPowershell,
            RuntimeAmpersandSemantics::CmdSeparator => AmpersandSemantics::CmdSeparator,
        };
        let path_style = match value.path_style {
            RuntimePathStyle::Windows => PathStyle::Windows,
            RuntimePathStyle::Posix => PathStyle::Posix,
        };
        Self {
            platform,
            shell,
            shell_version: value.shell_version.clone(),
            supports_and_and: value.supports_and_and,
            has_unix_utilities: value.has_unix_utilities,
            ampersand_semantics,
            path_style,
            text_encoding: "utf-8".into(),
        }
    }
}

/// Raw environment facts. Separated from the derived capabilities so the
/// matrix below can be unit-tested without a real machine.
#[derive(Debug, Clone, Default)]
pub struct ShellProbe {
    pub platform: Option<Platform>,
    pub pwsh: Option<PathBuf>,
    pub pwsh_version: Option<String>,
    pub powershell: Option<PathBuf>,
    pub powershell_version: Option<String>,
    pub bash: Option<PathBuf>,
    pub zsh: Option<PathBuf>,
    /// `MSYSTEM` is set by Git Bash / MSYS2 shells on Windows.
    pub msystem: Option<String>,
    /// `SHELL` as exported by POSIX login shells.
    pub shell_env: Option<String>,
    /// Explicit user override, e.g. from settings. Wins over detection.
    pub forced_shell: Option<ShellKind>,
}

impl TerminalCapabilities {
    /// Derive the capability record. Order matters: an explicit override wins,
    /// then Git Bash (which is a POSIX shell living on a Windows filesystem),
    /// then PowerShell 7, then Windows PowerShell 5.1, then cmd.
    pub fn from_probe(probe: &ShellProbe) -> Self {
        let platform = probe.platform.unwrap_or(default_platform());
        let shell = probe
            .forced_shell
            .unwrap_or_else(|| detect_shell(probe, platform));
        let shell_version = match shell {
            ShellKind::Pwsh => probe.pwsh_version.clone(),
            ShellKind::Powershell => probe.powershell_version.clone(),
            _ => None,
        };

        // Windows PowerShell 5.1 has no pipeline chain operators. `pwsh` (7+)
        // does. Treat an unknown `powershell.exe` as 5.1: assuming the weaker
        // shell can only cost an extra statement, while assuming the stronger
        // one produces a parser error the model then has to debug.
        let supports_and_and = match shell {
            ShellKind::Powershell => false,
            ShellKind::Pwsh => shell_version
                .as_deref()
                .and_then(major_version)
                .is_none_or(|major| major >= 7),
            ShellKind::Cmd | ShellKind::Bash | ShellKind::Zsh | ShellKind::GitBash => true,
        };

        let has_unix_utilities = shell.is_posix_family() || platform != Platform::Windows;

        let ampersand_semantics = match shell {
            ShellKind::Pwsh => AmpersandSemantics::PowershellCore,
            ShellKind::Powershell => AmpersandSemantics::WindowsPowershell,
            ShellKind::Cmd => AmpersandSemantics::CmdSeparator,
            ShellKind::Bash | ShellKind::Zsh | ShellKind::GitBash => {
                AmpersandSemantics::PosixBackground
            }
        };

        // Git Bash still addresses a Windows filesystem; native paths remain
        // the safe interchange format and MSYS conversion is disabled below.
        let path_style = if platform == Platform::Windows {
            PathStyle::Windows
        } else {
            PathStyle::Posix
        };

        Self {
            platform,
            shell,
            shell_version,
            supports_and_and,
            has_unix_utilities,
            ampersand_semantics,
            path_style,
            text_encoding: "utf-8".into(),
        }
    }

    /// Environment overrides a command runner *should* apply so Chinese output
    /// from `gh`, `python`, and `cargo` decodes as UTF-8 and Git Bash does not
    /// rewrite `/nologo`-style switches into paths.
    ///
    /// Not applied by Vellum. Vellum translates requests; the shell is spawned
    /// by Codex, so this is the policy a future Vellum-side runner would use
    /// and, until then, what the shell guidance tells the model to set itself.
    pub fn env_policy(&self) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        if self.platform == Platform::Windows {
            // Python defaults to the ANSI codepage on Windows; without these
            // two a `print()` of Chinese text raises UnicodeEncodeError.
            env.insert("PYTHONIOENCODING".into(), "utf-8".into());
            env.insert("PYTHONUTF8".into(), "1".into());
        }
        if self.shell == ShellKind::GitBash {
            // MSYS rewrites anything that looks like a POSIX path, which
            // corrupts `MSBuild /t:Build` and `cl.exe /nologo`.
            env.insert("MSYS_NO_PATHCONV".into(), "1".into());
            env.insert("MSYS2_ARG_CONV_EXCL".into(), "*".into());
        }
        env
    }

    /// Quote a single argument for this shell. Only used for the *display*
    /// rendering of a command; the argv array stays authoritative.
    pub fn quote(&self, argument: &str) -> String {
        if self.shell.is_powershell_family() || self.shell == ShellKind::Cmd {
            quote_powershell(argument)
        } else {
            quote_posix(argument)
        }
    }

    /// Lossy, human-readable rendering of an argv vector. Never parse this
    /// back — [`shell_call_arguments`] keeps the array for that.
    pub fn render_command_line(&self, argv: &[String]) -> String {
        argv.iter()
            .map(|part| self.quote(part))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Guidance folded into the shell tool description and the prompt, so the
    /// model never has to guess the shell family from the OS name alone.
    pub fn guidance(&self) -> String {
        let mut lines = Vec::new();
        let shell_label = match (self.shell, self.shell_version.as_deref()) {
            (shell, Some(version)) => format!("{} {version}", shell.as_str()),
            (shell, None) => shell.as_str().to_string(),
        };
        lines.push(format!(
            "Shell: {shell_label} on {}. Paths use {} style; all I/O is UTF-8.",
            match self.platform {
                Platform::Windows => "Windows",
                Platform::Linux => "Linux",
                Platform::Macos => "macOS",
            },
            match self.path_style {
                PathStyle::Windows => "Windows",
                PathStyle::Posix => "POSIX",
            }
        ));
        if !self.supports_and_and {
            lines.push(
                "This shell has no `&&` / `||` chain operators. Run one statement per call, or separate statements with `;` and check `$?` explicitly."
                    .into(),
            );
        }
        if !self.has_unix_utilities {
            lines.push(
                "Unix utilities (grep, sed, awk, head, tail, wc, which) are not available. Use the dedicated search/read tools, or PowerShell cmdlets such as Select-String, Get-Content -TotalCount, and Select-Object."
                    .into(),
            );
        }
        match self.ampersand_semantics {
            AmpersandSemantics::PosixBackground => {
                lines.push("`&` backgrounds a job.".into());
            }
            AmpersandSemantics::PowershellCore | AmpersandSemantics::WindowsPowershell => {
                lines.push(
                    "`&` is the call operator, not a separator: invoke quoted executables as `& \"C:\\Program Files\\app.exe\" arg`."
                        .into(),
                );
            }
            AmpersandSemantics::CmdSeparator => {
                lines.push("`&` separates commands unconditionally.".into());
            }
        }
        if self.shell.is_powershell_family() {
            lines.push(
                "Use single-quoted here-strings (@'...'@, closing delimiter at column 0) for multi-line literals; do not use backtick line continuations."
                    .into(),
            );
        }
        if self.shell == ShellKind::GitBash {
            // Stated as something the model must do, not as something already
            // done for it: Vellum translates the request, it does not spawn
            // the shell, so it cannot pre-set this environment.
            lines.push(
                "MSYS rewrites arguments that look like POSIX paths. Set MSYS_NO_PATHCONV=1 (or MSYS2_ARG_CONV_EXCL=*) for commands passing switches such as /nologo or /t:Build."
                    .into(),
            );
        }
        lines.join(" ")
    }
}

fn major_version(version: &str) -> Option<u32> {
    version
        .trim()
        .trim_start_matches('v')
        .split(['.', '-'])
        .next()?
        .parse()
        .ok()
}

fn detect_shell(probe: &ShellProbe, platform: Platform) -> ShellKind {
    if platform == Platform::Windows {
        if probe.msystem.is_some() {
            return ShellKind::GitBash;
        }
        if probe.pwsh.is_some() {
            return ShellKind::Pwsh;
        }
        if probe.powershell.is_some() {
            return ShellKind::Powershell;
        }
        if probe.bash.is_some() {
            return ShellKind::GitBash;
        }
        return ShellKind::Cmd;
    }
    if let Some(shell) = probe.shell_env.as_deref() {
        if shell.ends_with("zsh") {
            return ShellKind::Zsh;
        }
        if shell.ends_with("bash") {
            return ShellKind::Bash;
        }
    }
    if probe.zsh.is_some() && platform == Platform::Macos {
        return ShellKind::Zsh;
    }
    if probe.pwsh.is_some() && probe.bash.is_none() {
        return ShellKind::Pwsh;
    }
    ShellKind::Bash
}

/// The compile-time default platform. Only the *host* fills a [`ShellProbe`]
/// with live facts (that reads `PATH`, `MSYSTEM`, `SHELL`, and `VELLUM_SHELL`,
/// so it belongs to each host's environment adapter — Desktop's
/// `crate::harness::shell` — not here); this is the pure fallback when a probe
/// does not name a platform.
pub fn default_platform() -> Platform {
    if cfg!(target_os = "windows") {
        Platform::Windows
    } else if cfg!(target_os = "macos") {
        Platform::Macos
    } else {
        Platform::Linux
    }
}

/// PowerShell single-quote literal: no expansion inside, `'` doubles.
fn quote_powershell(argument: &str) -> String {
    if !argument.is_empty()
        && argument
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || "-_./:\\=".contains(ch))
    {
        return argument.to_string();
    }
    format!("'{}'", argument.replace('\'', "''"))
}

/// POSIX single-quote literal: close, escape, reopen for each `'`.
fn quote_posix(argument: &str) -> String {
    if !argument.is_empty()
        && argument
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || "-_./:=".contains(ch))
    {
        return argument.to_string();
    }
    format!("'{}'", argument.replace('\'', "'\\''"))
}

/// Argv-safe single-program invocation. Preferred over a shell script for
/// `git`, `pnpm`, `cargo`, `gh`, `python` and friends: no quoting layer means
/// no quoting bugs, and Chinese or space-bearing paths survive unchanged.
///
/// **Not executed by Vellum.** This and [`plan_command`] are the contract a
/// Vellum-side command runner would implement (issue #6 Phase 4, second half).
/// Today the shell is spawned by Codex, so nothing here runs a process, and no
/// prompt or tool description may claim that it does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecInput {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// How a requested command should actually be run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandPlan {
    /// Direct `CreateProcess`/`execvp` with an explicit argv vector.
    Exec(ExecInput),
    /// Genuine shell work: pipelines, redirection, or shell builtins.
    Shell { script: String },
}

/// Characters that mean the request is a shell *script*, not a program call.
fn needs_shell(part: &str) -> bool {
    part.contains(['|', '>', '<', '&', ';', '`', '\n'])
        || part.contains("$(")
        || part.contains("&&")
        || part.contains("||")
}

/// Shell builtins have no executable on disk, so they can never be exec'd.
const SHELL_BUILTINS: &[&str] = &[
    "cd", "export", "set", "source", ".", "alias", "unset", "eval", "exec", "pushd", "popd", "if",
    "for", "while", "foreach",
];

/// Decide between argv-safe exec and a shell script. Given an argv vector we
/// keep exec unless a part carries shell metacharacters or the program is a
/// builtin; a bare string is only exec'd when it is a single bare word.
pub fn plan_command(argv: &[String], caps: &TerminalCapabilities) -> CommandPlan {
    let program = argv.first().map(String::as_str).unwrap_or_default();
    let is_builtin = SHELL_BUILTINS
        .iter()
        .any(|builtin| program.eq_ignore_ascii_case(builtin));
    if argv.is_empty() || is_builtin || argv.iter().any(|part| needs_shell(part)) {
        return CommandPlan::Shell {
            script: caps.render_command_line(argv),
        };
    }
    CommandPlan::Exec(ExecInput {
        program: program.to_string(),
        args: argv[1..].to_vec(),
        cwd: None,
        env: caps.env_policy(),
        timeout_ms: None,
    })
}

/// Structured result of one command. `truncated` and `timed_out` are explicit
/// so the model can tell "no output" from "output dropped".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandOutcome {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
    pub timed_out: bool,
    pub truncated: bool,
    pub output: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

/// Strip a UTF-8 BOM and replace invalid sequences instead of failing. Windows
/// tools emit both; a hard decode error would surface as an opaque tool crash.
pub fn decode_output(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    text.strip_prefix('\u{feff}').unwrap_or(&text).to_string()
}

/// Truncate from the middle: the head carries the command echo, the tail
/// carries the actual error. Returns the text and whether it was cut.
pub fn truncate_output(text: &str, limit: usize) -> (String, bool) {
    if text.chars().count() <= limit || limit == 0 {
        return (text.to_string(), false);
    }
    let head = limit * 2 / 3;
    let tail = limit - head;
    let chars = text.chars().collect::<Vec<_>>();
    let dropped = chars.len() - limit;
    let mut output = chars[..head].iter().collect::<String>();
    output.push_str(&format!("\n... [{dropped} characters truncated] ...\n"));
    output.extend(chars[chars.len() - tail..].iter());
    (output, true)
}

/// Lossless arguments for a translated `shell` history item.
///
/// The Codex `local_shell_call` action carries argv as an array. Flattening it
/// with `join(" ")` destroys quoting: `["git", "-C", "C:\\path with space",
/// "status"]` becomes a command that re-parses as five arguments, and every
/// such history item teaches the model that quoting is optional.
///
/// The output here must validate against the model-visible `shell` schema in
/// [`crate::harness::tools::builtin_tool`] — a history item carrying a field
/// the schema forbids is the same surface/runtime split in miniature. That
/// means: only `command`, `workdir`, and `timeout_ms`; snake_case throughout;
/// and no rendered command line, which is a display artifact and would be a
/// second, conflicting source of truth for what ran.
pub fn shell_call_arguments(action: Option<&Value>) -> Value {
    let mut arguments = serde_json::Map::new();
    match action.and_then(|action| action.get("command")) {
        Some(Value::Array(parts)) => {
            let argv = parts
                .iter()
                .map(|part| match part {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                })
                .collect::<Vec<_>>();
            arguments.insert("command".into(), json!(argv));
        }
        // Older histories and `shell_command`-style actions carry one script
        // string. It stays a string — wrapping it in an array would claim it
        // is a program name — and the schema accepts both forms.
        Some(Value::String(text)) => {
            arguments.insert("command".into(), json!(text));
        }
        _ => {
            arguments.insert("command".into(), json!([]));
        }
    }
    if let Some(value) = action
        .and_then(|action| {
            action
                .get("working_directory")
                .or_else(|| action.get("workdir"))
        })
        .filter(|value| !value.is_null())
    {
        arguments.insert("workdir".into(), value.clone());
    }
    if let Some(timeout) = action
        .and_then(|action| action.get("timeout_ms"))
        .filter(|value| !value.is_null())
    {
        arguments.insert("timeout_ms".into(), timeout.clone());
    }
    Value::Object(arguments)
}

/// Inverse of [`shell_call_arguments`] for the argv case. Used by the
/// round-trip test that guards the history contract.
pub fn parse_shell_command(arguments: &Value) -> Option<Vec<String>> {
    match arguments.get("command")? {
        Value::Array(parts) => Some(
            parts
                .iter()
                .map(|part| match part {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                })
                .collect(),
        ),
        Value::String(text) => Some(vec![text.clone()]),
        _ => None,
    }
}

/// Resolve the explicit shell contract carried by the evaluation gateway.
///
/// Production Desktop requests never set this value and use the host-side
/// session handshake (`crate::harness::shell::detected()` in the Desktop
/// crate) instead. The evaluator does set it because the proxy may run on a
/// Windows host while the Codex agent itself runs in a Linux container.  The
/// model-visible prompt must describe the shell that will execute the tool,
/// not the shell that happens to host Vellum.
pub fn from_eval_contract(value: &str) -> Option<TerminalCapabilities> {
    let (platform, shell) = match value.trim().to_ascii_lowercase().as_str() {
        "linux-bash" => (Platform::Linux, ShellKind::Bash),
        "linux-zsh" => (Platform::Linux, ShellKind::Zsh),
        "windows-pwsh" => (Platform::Windows, ShellKind::Pwsh),
        "windows-powershell" => (Platform::Windows, ShellKind::Powershell),
        "windows-cmd" => (Platform::Windows, ShellKind::Cmd),
        _ => return None,
    };
    Some(TerminalCapabilities::from_probe(&ShellProbe {
        platform: Some(platform),
        forced_shell: Some(shell),
        ..ShellProbe::default()
    }))
}

/// Parse a `VELLUM_SHELL`-style override into a shell kind. Pure — the caller
/// decides whether to read that variable; this only decodes it.
pub fn parse_shell_kind(value: &str) -> Option<ShellKind> {
    match value.trim().to_ascii_lowercase().as_str() {
        "pwsh" | "powershell-core" => Some(ShellKind::Pwsh),
        "powershell" | "windows-powershell" => Some(ShellKind::Powershell),
        "git-bash" | "gitbash" => Some(ShellKind::GitBash),
        "cmd" => Some(ShellKind::Cmd),
        "bash" => Some(ShellKind::Bash),
        "zsh" => Some(ShellKind::Zsh),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn windows_probe() -> ShellProbe {
        ShellProbe {
            platform: Some(Platform::Windows),
            powershell: Some("C:/Windows/System32/powershell.exe".into()),
            powershell_version: Some("5.1.26200.1".into()),
            ..Default::default()
        }
    }

    #[test]
    fn prefers_pwsh_and_falls_back_to_windows_powershell() {
        let mut probe = windows_probe();
        probe.pwsh = Some("C:/Program Files/PowerShell/7/pwsh.exe".into());
        probe.pwsh_version = Some("7.5.0".into());
        let capabilities = TerminalCapabilities::from_probe(&probe);
        assert_eq!(capabilities.shell, ShellKind::Pwsh);
        assert!(capabilities.supports_and_and);

        let fallback = TerminalCapabilities::from_probe(&windows_probe());
        assert_eq!(fallback.shell, ShellKind::Powershell);
        assert!(!fallback.supports_and_and);
        assert_eq!(
            fallback.ampersand_semantics,
            AmpersandSemantics::WindowsPowershell
        );
    }

    #[test]
    fn windows_powershell_guidance_forbids_and_and_and_unix_tools() {
        let guidance = TerminalCapabilities::from_probe(&windows_probe()).guidance();
        assert!(guidance.contains("no `&&`"));
        assert!(guidance.contains("Unix utilities"));
        assert!(guidance.contains("Select-String"));
    }

    #[test]
    fn git_bash_disables_msys_path_conversion() {
        let mut probe = windows_probe();
        probe.msystem = Some("MINGW64".into());
        let capabilities = TerminalCapabilities::from_probe(&probe);
        assert_eq!(capabilities.shell, ShellKind::GitBash);
        assert!(capabilities.has_unix_utilities);
        let env = capabilities.env_policy();
        assert_eq!(
            env.get("MSYS2_ARG_CONV_EXCL").map(String::as_str),
            Some("*")
        );
        assert_eq!(env.get("MSYS_NO_PATHCONV").map(String::as_str), Some("1"));
    }

    #[test]
    fn windows_injects_utf8_python_environment() {
        let env = TerminalCapabilities::from_probe(&windows_probe()).env_policy();
        assert_eq!(env.get("PYTHONUTF8").map(String::as_str), Some("1"));
        assert_eq!(
            env.get("PYTHONIOENCODING").map(String::as_str),
            Some("utf-8")
        );
    }

    #[test]
    fn explicit_override_beats_detection() {
        let mut probe = windows_probe();
        probe.forced_shell = Some(ShellKind::GitBash);
        assert_eq!(
            TerminalCapabilities::from_probe(&probe).shell,
            ShellKind::GitBash
        );
    }

    #[test]
    fn single_program_uses_argv_safe_exec() {
        let capabilities = TerminalCapabilities::from_probe(&windows_probe());
        let argv = ["git", "-C", "C:\\路徑 with space", "status"].map(String::from);
        match plan_command(&argv, &capabilities) {
            CommandPlan::Exec(exec) => {
                assert_eq!(exec.program, "git");
                assert_eq!(exec.args[1], "C:\\路徑 with space");
            }
            other => panic!("expected exec plan, got {other:?}"),
        }
    }

    #[test]
    fn pipelines_and_builtins_use_the_shell() {
        let capabilities = TerminalCapabilities::from_probe(&windows_probe());
        let piped = ["git".into(), "log".into(), "|".into(), "head".into()];
        assert!(matches!(
            plan_command(&piped, &capabilities),
            CommandPlan::Shell { .. }
        ));
        let builtin = ["cd".into(), "src".into()];
        assert!(matches!(
            plan_command(&builtin, &capabilities),
            CommandPlan::Shell { .. }
        ));
    }

    /// The schema this harness advertises for `shell`.
    fn shell_schema() -> Value {
        match crate::harness::tools::builtin_tool("shell") {
            crate::harness::tools::BuiltinDecision::Exact { parameters, .. } => parameters,
            other => panic!("shell must have an exact contract, got {other:?}"),
        }
    }

    #[test]
    fn argv_history_round_trips_without_join() {
        let argv = vec![
            "git".to_string(),
            "-C".to_string(),
            "C:\\path with space".to_string(),
            "status".to_string(),
        ];
        let action = json!({"command": argv, "working_directory": "C:\\repo", "timeout_ms": 1000});
        let arguments = shell_call_arguments(Some(&action));
        assert_eq!(parse_shell_command(&arguments), Some(argv));
        assert_eq!(arguments["workdir"], "C:\\repo");
        assert_eq!(arguments["timeout_ms"], 1000);
        // No rendered command line rides along: it would be a second, and
        // eventually conflicting, source of truth for what actually ran.
        assert!(arguments.get("commandLine").is_none());
        assert!(arguments.get("timeoutMs").is_none());
    }

    /// Issue #6 review, P0-3: the arguments this harness emits must satisfy the
    /// schema it advertises. Every shape that can appear in history is checked,
    /// including the legacy single-string command.
    #[test]
    fn every_emitted_history_shape_validates_against_the_declared_schema() {
        let schema = shell_schema();
        for action in [
            json!({"command": ["git", "-C", "C:\\路徑 with space", "status"]}),
            json!({"command": ["ls"], "working_directory": "/repo", "timeout_ms": 5000}),
            json!({"command": "git status | head -5"}),
            json!({"command": null}),
            json!({}),
        ] {
            let arguments = shell_call_arguments(Some(&action));
            crate::harness::tools::validate_against_schema(&arguments, &schema)
                .unwrap_or_else(|error| panic!("{action} produced invalid arguments: {error}"));
        }
        // And the validator is not vacuous.
        assert!(crate::harness::tools::validate_against_schema(
            &json!({"command": ["ls"], "commandLine": "ls"}),
            &schema
        )
        .is_err());
        assert!(crate::harness::tools::validate_against_schema(&json!({}), &schema).is_err());
    }

    #[test]
    fn powershell_quoting_doubles_single_quotes() {
        let capabilities = TerminalCapabilities::from_probe(&windows_probe());
        assert_eq!(capabilities.quote("it's"), "'it''s'");
        assert_eq!(capabilities.quote("plain"), "plain");
    }

    #[test]
    fn posix_quoting_escapes_single_quotes() {
        let probe = ShellProbe {
            platform: Some(Platform::Linux),
            shell_env: Some("/bin/bash".into()),
            ..Default::default()
        };
        let capabilities = TerminalCapabilities::from_probe(&probe);
        assert_eq!(capabilities.quote("it's"), "'it'\\''s'");
        assert!(capabilities.has_unix_utilities);
    }

    #[test]
    fn output_decoding_strips_bom_and_survives_invalid_bytes() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice("測試".as_bytes());
        bytes.push(0xFF);
        let decoded = decode_output(&bytes);
        assert!(decoded.starts_with("測試"));
        assert!(!decoded.starts_with('\u{feff}'));
    }

    #[test]
    fn truncation_keeps_head_and_tail() {
        let text = "a".repeat(100) + "TAIL";
        let (output, truncated) = truncate_output(&text, 40);
        assert!(truncated);
        assert!(output.ends_with("TAIL"));
        assert!(output.contains("truncated"));
        let (short, untouched) = truncate_output("small", 40);
        assert!(!untouched);
        assert_eq!(short, "small");
    }
}
