//! Keeps each opted-in account's 5-hour window running back to back: once a
//! window resets, one tiny Haiku turn starts the next. The turn runs through
//! Desktop's own bundled Claude Code with the account's token, never a
//! hand-built API call, in an empty config home with no tools, MCP servers or
//! settings. Mirrors `crate::five_hour_cadence` for Codex.
use super::accounts::{remaining, AccountUsage, CodeAccounts};
use serde_json::Value;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use vellum_proxy_runtime::claude_code::account_hash;

pub const TRIGGER_MODEL: &str = "claude-haiku-5-5";
const TRIGGER_PROMPT: &str = "Reply with exactly OK.";
const RESET_SAFETY_SECS: i64 = 2;
const VERIFY_DELAY: Duration = Duration::from_secs(3);
const ZERO_WINDOW_SAMPLE_DELAY: Duration = Duration::from_secs(3);
const RECENT_TRIGGER_COOLDOWN_SECS: i64 = 15 * 60;
const ERROR_RETRY_DELAY: Duration = Duration::from_secs(15 * 60);
const VERIFY_RETRY_DELAY: Duration = Duration::from_secs(2 * 60);
const IDLE_DELAY: Duration = Duration::from_secs(24 * 60 * 60);
const TRIGGER_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Debug, PartialEq)]
enum Next {
    /// Weekly usage is at the account's weekly gate: wait for its reset.
    WeeklyGate { retry_at: Option<i64> },
    /// A window is running; the next one can start once it resets.
    Wait { due_at: i64 },
    /// Unused with a reset time: read again to see whether it slides.
    Probe { reset_at: i64 },
    /// No window is running.
    Trigger(&'static str),
    /// Used, but without a reset time to wait for.
    Unknown,
}

fn timestamp(at: Option<&str>) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(at?)
        .ok()
        .map(|at| at.timestamp())
}

fn next_step(usage: &AccountUsage, weekly_floor: u8, now: i64) -> Next {
    if let Some(left) = remaining(usage.seven_day.as_ref(), now) {
        if left <= f64::from(weekly_floor) {
            return Next::WeeklyGate {
                retry_at: usage
                    .seven_day
                    .as_ref()
                    .and_then(|w| timestamp(w.resets_at.as_deref()))
                    .map(|at| at + RESET_SAFETY_SECS),
            };
        }
    }
    let Some(window) = usage.five_hour.as_ref() else {
        return Next::Trigger("no_window");
    };
    let unused = window.utilization <= f64::EPSILON;
    match timestamp(window.resets_at.as_deref()) {
        None if unused => Next::Trigger("inactive_window"),
        None => Next::Unknown,
        Some(reset) if reset <= now => Next::Trigger("expired_window"),
        Some(reset) if unused => Next::Probe { reset_at: reset },
        Some(reset) => Next::Wait {
            due_at: reset + RESET_SAFETY_SECS,
        },
    }
}

/// An unused window may report a projection of `now + 5h` that moves with
/// each read; a running window keeps its reset even when usage rounds to 0%.
fn reset_moves_with_clock(
    first_reset: i64,
    second_reset: i64,
    first_observed_at: i64,
    second_observed_at: i64,
) -> bool {
    let elapsed = second_observed_at.saturating_sub(first_observed_at);
    let reset_shift = second_reset.saturating_sub(first_reset);
    reset_shift >= elapsed.saturating_sub(1).max(2)
}

/// Reads usage and settles an unused window with a second sample.
async fn observe(accounts: &CodeAccounts, id: &str, weekly_floor: u8) -> Result<Next, String> {
    let first = accounts.read_usage_now(id).await?;
    let first_at = chrono::Utc::now().timestamp();
    let first_reset = match next_step(&first, weekly_floor, first_at) {
        Next::Probe { reset_at } => reset_at,
        step => return Ok(step),
    };
    tokio::time::sleep(ZERO_WINDOW_SAMPLE_DELAY).await;
    let second = accounts.read_usage_now(id).await?;
    let second_at = chrono::Utc::now().timestamp();
    Ok(match next_step(&second, weekly_floor, second_at) {
        Next::Probe { reset_at } => {
            let sliding = reset_moves_with_clock(first_reset, reset_at, first_at, second_at);
            log::info!(
                "[ClaudeCode] five_hour event=window_probe account={} first_reset_at={first_reset} second_reset_at={reset_at} sliding={sliding}",
                account_hash(id)
            );
            if sliding {
                Next::Trigger("inactive_sliding_reset")
            } else {
                Next::Wait {
                    due_at: reset_at + RESET_SAFETY_SECS,
                }
            }
        }
        other => other,
    })
}

fn delay_until(at: i64, now: i64) -> Duration {
    Duration::from_secs(at.saturating_sub(now).max(1) as u64)
}

fn recent_trigger_delay(recent: &HashMap<String, i64>, id: &str) -> Option<Duration> {
    let elapsed = chrono::Utc::now()
        .timestamp()
        .saturating_sub(*recent.get(id)?);
    (elapsed < RECENT_TRIGGER_COOLDOWN_SECS)
        .then(|| Duration::from_secs((RECENT_TRIGGER_COOLDOWN_SECS - elapsed).max(1) as u64))
}

async fn inspect(
    accounts: &CodeAccounts,
    id: &str,
    weekly_floor: u8,
    recent: &mut HashMap<String, i64>,
) -> Duration {
    let tag = account_hash(id);
    let reason = match observe(accounts, id, weekly_floor).await {
        Err(error) => {
            log::warn!("[ClaudeCode] five_hour event=usage_error account={tag} error={error}");
            return ERROR_RETRY_DELAY;
        }
        Ok(Next::WeeklyGate { retry_at }) => {
            log::info!(
                "[ClaudeCode] five_hour event=skip account={tag} reason=weekly_gate floor={weekly_floor}"
            );
            return retry_at.map_or(ERROR_RETRY_DELAY, |at| {
                delay_until(at, chrono::Utc::now().timestamp())
            });
        }
        Ok(Next::Wait { due_at }) => {
            recent.remove(id);
            log::info!("[ClaudeCode] five_hour event=scheduled account={tag} due_at={due_at}");
            return delay_until(due_at, chrono::Utc::now().timestamp());
        }
        Ok(Next::Unknown | Next::Probe { .. }) => {
            log::warn!("[ClaudeCode] five_hour event=skip account={tag} reason=no_reset_time");
            return ERROR_RETRY_DELAY;
        }
        Ok(Next::Trigger(reason)) => reason,
    };
    if let Some(delay) = recent_trigger_delay(recent, id) {
        log::info!(
            "[ClaudeCode] five_hour event=awaiting_observation account={tag} cooldown_seconds={}",
            delay.as_secs()
        );
        return delay;
    }
    log::info!(
        "[ClaudeCode] five_hour event=trigger_start account={tag} reason={reason} model={TRIGGER_MODEL}"
    );
    if let Err(error) = trigger(accounts, id).await {
        log::warn!("[ClaudeCode] five_hour event=trigger_error account={tag} error={error}");
        accounts.set_five_hour_error(id, Some(error)).await;
        return ERROR_RETRY_DELAY;
    }
    accounts.set_five_hour_error(id, None).await;
    recent.insert(id.to_owned(), chrono::Utc::now().timestamp());
    tokio::time::sleep(VERIFY_DELAY).await;
    match observe(accounts, id, weekly_floor).await {
        Ok(Next::Wait { due_at }) => {
            recent.remove(id);
            log::info!(
                "[ClaudeCode] five_hour event=trigger_verified account={tag} due_at={due_at}"
            );
            delay_until(due_at, chrono::Utc::now().timestamp())
        }
        outcome => {
            log::warn!(
                "[ClaudeCode] five_hour event=trigger_unverified account={tag} observed={outcome:?}"
            );
            recent_trigger_delay(recent, id).unwrap_or(VERIFY_RETRY_DELAY)
        }
    }
}

/// `<config>/Claude/claude-code/<version>/[<build>/]claude(.exe)`, newest first.
fn bundled_claude_code(base: &Path) -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "claude.exe"
    } else {
        "claude"
    };
    let version = |path: &Path| -> Option<Vec<u64>> {
        path.file_name()?
            .to_str()?
            .split('.')
            .map(|part| part.parse().ok())
            .collect()
    };
    let mut versions: Vec<(Vec<u64>, PathBuf)> = std::fs::read_dir(base)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter_map(|path| Some((version(&path)?, path)))
        .collect();
    versions.sort_by(|a, b| b.0.cmp(&a.0));
    versions.into_iter().find_map(|(_, dir)| {
        let direct = dir.join(name);
        if direct.is_file() {
            return Some(direct);
        }
        std::fs::read_dir(&dir)
            .ok()?
            .flatten()
            .map(|entry| entry.path().join(name))
            .find(|path| path.is_file())
    })
}

/// Inherited variables that could point the turn somewhere else or at
/// another credential: every `ANTHROPIC_*` and `CLAUDE*` one.
fn is_scrubbed(key: &OsStr) -> bool {
    let key = key.to_string_lossy().to_ascii_uppercase();
    key.starts_with("ANTHROPIC_") || key.starts_with("CLAUDE")
}

fn trigger_command(
    exe: &Path,
    token: &str,
    config: &Path,
    cwd: &Path,
    inherited: impl IntoIterator<Item = OsString>,
) -> tokio::process::Command {
    let mut command = crate::process::background_tokio_command(exe);
    for key in inherited.into_iter().filter(|key| is_scrubbed(key)) {
        command.env_remove(key);
    }
    command
        .args(["-p", TRIGGER_PROMPT, "--model", TRIGGER_MODEL])
        .args(["--system-prompt", TRIGGER_PROMPT])
        // No tools, MCP servers, slash commands or settings files: the turn
        // is one short answer and nothing it reads can change that.
        .args([
            "--tools",
            "",
            "--strict-mcp-config",
            "--disable-slash-commands",
        ])
        .args(["--setting-sources", "", "--no-session-persistence"])
        .args(["--max-turns", "1", "--output-format", "json"])
        .env("CLAUDE_CODE_OAUTH_TOKEN", token)
        .env("CLAUDE_CONFIG_DIR", config)
        .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
        .env("DISABLE_AUTOUPDATER", "1")
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

/// `--output-format json` prints one result object; `is_error` covers an
/// API rejection that still exits cleanly.
fn outcome(success: bool, stdout: &[u8]) -> Result<(), String> {
    let value = serde_json::from_slice::<Value>(stdout.trim_ascii()).ok();
    let is_error = value
        .as_ref()
        .and_then(|v| v.get("is_error"))
        .and_then(Value::as_bool);
    if success && is_error != Some(true) {
        return Ok(());
    }
    let detail = value
        .as_ref()
        .and_then(|v| v.get("result"))
        .and_then(Value::as_str)
        .unwrap_or("Claude Code exited without a result");
    Err(format!(
        "Claude Code trigger failed: {}",
        detail.chars().take(300).collect::<String>()
    ))
}

async fn trigger(accounts: &CodeAccounts, id: &str) -> Result<(), String> {
    let exe = dirs::config_dir()
        .and_then(|dir| bundled_claude_code(&dir.join("Claude").join("claude-code")))
        .ok_or("Claude Desktop's bundled Claude Code was not found")?;
    let home = accounts.cadence_home();
    let (config, cwd) = (home.join("config"), home.join("cwd"));
    for dir in [&config, &cwd] {
        std::fs::create_dir_all(dir).map_err(|_| "Cannot create the 5-hour trigger folder")?;
    }
    let token = accounts.access_token(id).await?;
    let output = tokio::time::timeout(
        TRIGGER_TIMEOUT,
        trigger_command(
            &exe,
            &token,
            &config,
            &cwd,
            std::env::vars_os().map(|(k, _)| k),
        )
        .output(),
    )
    .await
    .map_err(|_| "Claude Code did not finish the 5-hour trigger in time")?
    .map_err(|_| "Cannot start Claude Code for the 5-hour trigger")?;
    outcome(output.status.success(), &output.stdout)
}

async fn run_cycle(accounts: &CodeAccounts, recent: &mut HashMap<String, i64>) -> Duration {
    let members = match accounts.cadence_members().await {
        Ok(members) => members,
        Err(error) => {
            log::warn!("[ClaudeCode] five_hour event=accounts_error error={error}");
            return ERROR_RETRY_DELAY;
        }
    };
    let mut next = IDLE_DELAY;
    for (id, weekly_floor) in members {
        next = next.min(inspect(accounts, &id, weekly_floor, recent).await);
    }
    next
}

pub fn spawn(accounts: Arc<CodeAccounts>) {
    let notify = accounts.cadence_notifier();
    tauri::async_runtime::spawn(async move {
        let mut recent = HashMap::new();
        log::info!("[ClaudeCode] five_hour event=scheduler_start model={TRIGGER_MODEL}");
        loop {
            let delay = run_cycle(&accounts, &mut recent).await;
            tokio::select! {
                _ = tokio::time::sleep(delay) => {},
                _ = notify.notified() => {
                    log::info!("[ClaudeCode] five_hour event=settings_changed");
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::super::accounts::UsageWindow;
    use super::*;

    fn usage(five_hour: Option<(f64, Option<i64>)>, weekly_used: f64) -> AccountUsage {
        let window = |utilization: f64, reset: Option<i64>| {
            UsageWindow::for_test(
                utilization,
                reset
                    .and_then(|at| chrono::DateTime::from_timestamp(at, 0))
                    .map(|at| at.to_rfc3339()),
            )
        };
        AccountUsage {
            observed_at: 0,
            five_hour: five_hour.map(|(used, reset)| window(used, reset)),
            seven_day: Some(window(weekly_used, Some(2_000_000_000))),
            fable: None,
            credits_enabled: None,
        }
    }

    #[test]
    fn running_windows_wait_for_reset_and_idle_ones_trigger() {
        let now = 1_900_000_000;
        assert_eq!(
            next_step(&usage(Some((12.0, Some(now + 600))), 30.0), 0, now),
            Next::Wait { due_at: now + 602 }
        );
        assert_eq!(
            next_step(&usage(Some((12.0, Some(now - 1))), 30.0), 0, now),
            Next::Trigger("expired_window")
        );
        assert_eq!(
            next_step(&usage(Some((0.0, None)), 30.0), 0, now),
            Next::Trigger("inactive_window")
        );
        assert_eq!(
            next_step(&usage(None, 30.0), 0, now),
            Next::Trigger("no_window")
        );
        assert_eq!(
            next_step(&usage(Some((0.0, Some(now + 18_000))), 30.0), 0, now),
            Next::Probe {
                reset_at: now + 18_000
            }
        );
        assert_eq!(
            next_step(&usage(Some((5.0, None)), 30.0), 0, now),
            Next::Unknown
        );
    }

    #[test]
    fn weekly_gate_holds_the_trigger_until_the_weekly_reset() {
        let now = 1_900_000_000;
        let idle = Some((0.0, None));
        assert_eq!(
            next_step(&usage(idle, 85.0), 15, now),
            Next::WeeklyGate {
                retry_at: Some(2_000_000_002)
            }
        );
        assert_eq!(
            next_step(&usage(idle, 84.0), 15, now),
            Next::Trigger("inactive_window")
        );
        // With no gate, only a spent week holds it.
        assert_eq!(
            next_step(&usage(idle, 100.0), 0, now),
            Next::WeeklyGate {
                retry_at: Some(2_000_000_002)
            }
        );
    }

    #[test]
    fn sliding_projection_is_told_apart_from_a_stable_reset() {
        assert!(reset_moves_with_clock(10_000, 10_004, 5_000, 5_004));
        assert!(reset_moves_with_clock(10_000, 10_003, 5_000, 5_004));
        assert!(!reset_moves_with_clock(10_000, 10_000, 5_000, 5_004));
        assert!(!reset_moves_with_clock(10_000, 10_001, 5_000, 5_004));
    }

    #[test]
    fn newest_bundled_claude_code_is_found() {
        let temp = tempfile::tempdir().unwrap();
        let name = if cfg!(windows) {
            "claude.exe"
        } else {
            "claude"
        };
        for (version, build) in [
            ("2.1.99", "a"),
            ("2.1.304", "b"),
            ("2.1.400", "c"),
            ("notes", "d"),
        ] {
            let dir = temp.path().join(version).join(build);
            std::fs::create_dir_all(&dir).unwrap();
            if version != "2.1.400" {
                std::fs::write(dir.join(name), b"").unwrap();
            }
        }
        // 2.1.400 has no executable yet (a download in progress).
        assert_eq!(
            bundled_claude_code(temp.path()),
            Some(temp.path().join("2.1.304").join("b").join(name))
        );
        std::fs::write(temp.path().join("2.1.400").join(name), b"").unwrap();
        assert_eq!(
            bundled_claude_code(temp.path()),
            Some(temp.path().join("2.1.400").join(name))
        );
        assert_eq!(bundled_claude_code(&temp.path().join("missing")), None);
    }

    #[test]
    fn trigger_is_one_haiku_turn_with_nothing_inherited() {
        let temp = tempfile::tempdir().unwrap();
        let command = trigger_command(
            Path::new("claude.exe"),
            "token-value",
            &temp.path().join("config"),
            &temp.path().join("cwd"),
            [
                "ANTHROPIC_BASE_URL",
                "CLAUDECODE",
                "CLAUDE_CODE_USE_BEDROCK",
                "PATH",
                "HTTPS_PROXY",
            ]
            .map(OsString::from),
        );
        let command = command.as_std();
        let args: Vec<_> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let flag = |name: &str| {
            let at = args.iter().position(|a| a == name).unwrap();
            args[at + 1].clone()
        };
        assert_eq!(flag("--model"), TRIGGER_MODEL);
        assert_eq!(flag("--tools"), "");
        assert_eq!(flag("--setting-sources"), "");
        assert_eq!(flag("--max-turns"), "1");
        assert!(args.iter().any(|a| a == "--strict-mcp-config"));
        assert!(args.iter().any(|a| a == "--no-session-persistence"));
        // The token travels only in the child's environment.
        assert!(!args.iter().any(|a| a.contains("token-value")));
        let envs: HashMap<String, Option<String>> = command
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert_eq!(
            envs["CLAUDE_CODE_OAUTH_TOKEN"].as_deref(),
            Some("token-value")
        );
        assert_eq!(
            envs["CLAUDE_CONFIG_DIR"].as_deref().map(PathBuf::from),
            Some(temp.path().join("config"))
        );
        for scrubbed in [
            "ANTHROPIC_BASE_URL",
            "CLAUDECODE",
            "CLAUDE_CODE_USE_BEDROCK",
        ] {
            assert_eq!(envs.get(scrubbed), Some(&None), "{scrubbed}");
        }
        assert!(!envs.contains_key("PATH") && !envs.contains_key("HTTPS_PROXY"));
        assert_eq!(
            command.get_current_dir(),
            Some(temp.path().join("cwd").as_path())
        );
    }

    #[test]
    fn result_errors_are_reported_even_on_a_clean_exit() {
        assert!(outcome(true, br#"{"type":"result","is_error":false,"result":"OK"}"#).is_ok());
        assert!(outcome(true, b"").is_ok());
        let error = outcome(
            true,
            br#"{"is_error":true,"result":"API Error: 429 rate limited"}"#,
        )
        .unwrap_err();
        assert!(error.contains("429"), "{error}");
        assert!(outcome(false, b"not json").is_err());
    }
}
