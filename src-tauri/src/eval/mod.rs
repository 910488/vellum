mod artifacts;
pub mod enhanced_gate;
mod enhanced_mvp;
mod events;
mod gateway;
pub mod live;
mod manifest;
mod parity;
mod replay;
pub mod report;
pub mod runner;
mod zcode_desktop;

use std::path::PathBuf;

use crate::error::{AppError, AppResult};
use crate::model::ProviderKind;
use crate::state::AppState;
use runner::{default_codex_version, EvalPaths, RunOptions};

pub async fn run_cli() -> AppResult<()> {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    let Some(command) = arguments.first().map(String::as_str) else {
        print_help();
        return Ok(());
    };
    let args = &arguments[1..];
    match command {
        "-h" | "--help" | "help" => print_help(),
        "doctor" => {
            let parsed = CommonArgs::parse(args)?;
            let paths = EvalPaths::discover(parsed.dataset_root)?;
            let state = AppState::try_new().map_err(|error| {
                AppError::Message(format!(
                    "cannot initialize Vellum state for doctor; Keychain or profile data is unavailable: {error}"
                ))
            })?;
            let checks = runner::doctor(
                &paths,
                &state,
                &parsed.codex_version,
                parsed.profile.as_deref(),
                &parsed.executor,
                parsed.oauth_account.as_deref(),
                parsed.grok_account.as_deref(),
            )
            .await;
            let mut failed = false;
            for check in checks {
                println!(
                    "{} {:<32} {}",
                    if check.ok { "PASS" } else { "FAIL" },
                    check.name,
                    check.detail
                );
                failed |= !check.ok;
            }
            if failed {
                return Err(AppError::Message(
                    "one or more eval environment checks failed".into(),
                ));
            }
        }
        "models" => {
            let state = AppState::new();
            let routes = state.routes();
            let include_official = args.iter().any(|argument| argument == "--include-official");
            let codex_paths = crate::codex::CodexPaths::discover(&state.data_root());
            let official_catalog = crate::catalog::read_official_catalog(&codex_paths.models_cache);
            let mut count = 0;
            for model in crate::catalog::model_routes_with_official_catalog(
                &routes,
                official_catalog.as_ref(),
            ) {
                let Some(route) = routes.iter().find(|route| route.id == model.route_id) else {
                    continue;
                };
                if !route.enabled
                    || (route.provider_kind == ProviderKind::Official && !include_official)
                {
                    continue;
                }
                println!(
                    "{}\t{}\t{}\t{}",
                    model.catalog_id,
                    route.name,
                    model.upstream_model,
                    model
                        .context_window
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "unknown".into())
                );
                count += 1;
            }
            if count == 0 {
                return Err(AppError::Message(
                    "no enabled models are available for the requested filter".into(),
                ));
            }
        }
        "investigation-replay" => {
            let paths = EvalPaths::discover(None)?;
            let path = value_after(args, "--file")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    paths
                        .evals_root
                        .join("replays")
                        .join("investigation-replay.json")
                });
            let bytes = std::fs::read(&path).map_err(|error| {
                AppError::Message(format!(
                    "read investigation replay {}: {error}",
                    path.display()
                ))
            })?;
            let fixture = serde_json::from_slice::<serde_json::Value>(&bytes).ok();
            let captured_rollout = if fixture.is_none() {
                let text = std::str::from_utf8(&bytes).map_err(|error| {
                    AppError::Message(format!(
                        "decode investigation replay {} as UTF-8: {error}",
                        path.display()
                    ))
                })?;
                Some(
                    vellum_proxy_runtime::investigation_replay::parse_codex_rollout_jsonl(text)
                        .map_err(|error| {
                            AppError::Message(format!(
                                "parse investigation replay {}: {error}",
                                path.display()
                            ))
                        })?,
                )
            } else {
                None
            };
            let user_prompt = fixture
                .as_ref()
                .and_then(|value| value.get("userPrompt"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("Investigate the unresolved state using independent evidence.");
            let mode = value_after(args, "--mode")
                .or_else(|| {
                    fixture
                        .as_ref()
                        .and_then(|value| value.get("mode"))
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_else(|| "shadow".into());
            let policy = match mode.as_str() {
                "shadow" => {
                    vellum_proxy_runtime::investigation_reducer::InvestigationPolicy::shadow()
                }
                "recover" => {
                    vellum_proxy_runtime::investigation_reducer::InvestigationPolicy::recover()
                }
                mode => {
                    return Err(AppError::Message(format!(
                        "invalid investigation replay mode `{mode}`; expected shadow or recover"
                    )))
                }
            };
            let portable_items = captured_rollout
                .as_ref()
                .map(|capture| capture.items.as_slice())
                .or_else(|| {
                    fixture.as_ref().and_then(|value| {
                        value
                            .as_array()
                            .or_else(|| value.get("items").and_then(serde_json::Value::as_array))
                            .map(Vec::as_slice)
                    })
                });
            let (_, report) = if let Some(items) = portable_items {
                if let Some(capture) = captured_rollout.as_ref() {
                    vellum_proxy_runtime::investigation_replay::replay_investigation_items_with_usage(
                        items,
                        Default::default(),
                        Default::default(),
                        &policy,
                        Some(&capture.request_usage),
                    )
                } else {
                    vellum_proxy_runtime::investigation_replay::replay_investigation_items(
                        items,
                        Default::default(),
                        Default::default(),
                        &policy,
                    )
                }
            } else {
                let event_value = fixture
                    .as_ref()
                    .and_then(|value| value.get("events"))
                    .cloned()
                    .ok_or_else(|| {
                        AppError::Message(format!(
                            "investigation replay {} must contain `items` or `events`",
                            path.display()
                        ))
                    })?;
                let events = serde_json::from_value::<
                    Vec<vellum_proxy_runtime::investigation_replay::ReplayEvent>,
                >(event_value)
                .map_err(|error| {
                    AppError::Message(format!(
                        "parse investigation replay events {}: {error}",
                        path.display()
                    ))
                })?;
                vellum_proxy_runtime::investigation_replay::replay_investigation_events(
                    user_prompt,
                    &events,
                    Default::default(),
                    Default::default(),
                    &policy,
                )
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&report).map_err(|error| {
                    AppError::Message(format!("encode investigation replay report: {error}"))
                })?
            );
        }
        "protocol-replay" => {
            let paths = EvalPaths::discover(None)?;
            let path = value_after(args, "--file")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    paths
                        .evals_root
                        .join("replays")
                        .join("protocol-replay.json")
                });
            let count = replay::run(&path)?;
            println!("PASS {count} protocol replay cases");
        }
        "app-replay" => {
            let paths = EvalPaths::discover(None)?;
            let suite =
                value_after(args, "--suite").unwrap_or_else(|| "desktop-protocol-replay".into());
            let path = paths
                .evals_root
                .join("replays")
                .join(format!("{suite}.json"));
            let count = replay::run(&path)?;
            println!("PASS {count} desktop protocol replay cases");
        }
        "parity" => {
            let parsed = CommonArgs::parse(args)?;
            let models = value_after(args, "--models")
                .or_else(|| value_after(args, "--model"))
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>();
            let executor = value_after(args, "--executor").unwrap_or_else(|| {
                if cfg!(target_os = "windows") {
                    "windows-sandbox".into()
                } else {
                    "docker".into()
                }
            });
            let path = parity::write_report(
                models,
                parsed.profile.as_deref(),
                &parsed.codex_version,
                &executor,
            )
            .await?;
            println!(
                "PASS static model-visible parity (not a Desktop capture): {}",
                path.display()
            );
        }
        "app-server-parity" => {
            let parsed = CommonArgs::parse(args)?;
            let models = value_after(args, "--models")
                .or_else(|| value_after(args, "--model"))
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>();
            let path = parity::write_app_server_parity(
                models,
                parsed.profile.as_deref(),
                &parsed.codex_version,
            )
            .await?;
            println!("PASS Codex app-server parity: {}", path.display());
        }
        "qualify-issue-7" => {
            let grok_run = value_after(args, "--grok-run")
                .ok_or_else(|| AppError::Message("--grok-run is required".into()))?;
            let glm_run = value_after(args, "--glm-run")
                .ok_or_else(|| AppError::Message("--glm-run is required".into()))?;
            let parity_path = value_after(args, "--parity")
                .map(PathBuf::from)
                .ok_or_else(|| AppError::Message("--parity is required".into()))?;
            let app_parity_path = value_after(args, "--app-parity")
                .map(PathBuf::from)
                .ok_or_else(|| AppError::Message("--app-parity is required".into()))?;
            let path = parity::write_issue7_qualification(
                &grok_run,
                &glm_run,
                &parity_path,
                &app_parity_path,
            )?;
            println!("PASS Issue #7 Stage -1/1 qualification: {}", path.display());
        }
        "prepare" => {
            let parsed = CommonArgs::parse(args)?;
            let suite = parsed
                .suite
                .ok_or_else(|| AppError::Message("--suite is required".into()))?;
            let paths = EvalPaths::discover(parsed.dataset_root)?;
            runner::prepare(&paths, &suite, &parsed.codex_version).await?;
            println!("Prepared suite {suite}");
        }
        "live" => {
            let options = live::parse_live_args(args)?;
            let written = live::run_live(options).await?;
            for path in written {
                println!("LIVE {}", path.display());
            }
        }
        "enhanced-mvp-fixtures" => enhanced_mvp::run(args).await?,
        "enhanced-integration-gate" => enhanced_gate::run(args).await?,
        "enhanced-runtime-status" => enhanced_gate::preflight::run(args)?,
        "zcode-desktop-canary" => zcode_desktop::run(args).await?,
        "sandbox-preflight" => {
            let parsed = CommonArgs::parse(args)?;
            let paths = EvalPaths::discover(parsed.dataset_root)?;
            let check = runner::windows_sandbox_preflight(&paths, &parsed.codex_version).await;
            println!(
                "{} {:<32} {}",
                if check.ok { "PASS" } else { "FAIL" },
                check.name,
                check.detail
            );
            if !check.ok {
                return Err(AppError::Message(
                    "Windows Sandbox executor preflight failed".into(),
                ));
            }
        }
        "run" => {
            let parsed = RunArgs::parse(args, false)?;
            let run_root = runner::run(parsed.into_options()).await?;
            println!("Eval run completed: {}", run_root.display());
        }
        "matrix" => {
            let parsed = RunArgs::parse(args, true)?;
            let run_root = runner::run(parsed.into_options()).await?;
            println!("Eval matrix completed: {}", run_root.display());
        }
        "report" => {
            let run_id = args
                .first()
                .filter(|value| !value.starts_with('-'))
                .ok_or_else(|| AppError::Message("report requires a run id".into()))?;
            let format = value_after(args, "--format").unwrap_or_else(|| "html".into());
            let compare = value_after(args, "--compare");
            let paths = EvalPaths::discover(None)?;
            let run_root = paths.output_root.join(run_id);
            match format.as_str() {
                "html" => {
                    let baseline = compare.as_deref().map(|id| paths.output_root.join(id));
                    let path =
                        report::write_html_report_with_baseline(&run_root, baseline.as_deref())?;
                    println!("{}", path.display());
                }
                "json" => {
                    let baseline = compare.as_deref().map(|id| paths.output_root.join(id));
                    report::print_json_summary_with_baseline(&run_root, baseline.as_deref())?
                }
                value => {
                    return Err(AppError::Message(format!(
                        "unsupported report format: {value}"
                    )))
                }
            }
        }
        "triage" => {
            let run_id = args
                .first()
                .filter(|value| !value.starts_with('-'))
                .ok_or_else(|| AppError::Message("triage requires a run id".into()))?;
            let paths = EvalPaths::discover(None)?;
            report::print_triage(&paths.output_root.join(run_id))?;
        }
        value => {
            print_help();
            return Err(AppError::Message(format!(
                "unknown vellum-eval command: {value}"
            )));
        }
    }
    Ok(())
}

struct CommonArgs {
    suite: Option<String>,
    dataset_root: Option<PathBuf>,
    codex_version: String,
    profile: Option<String>,
    executor: String,
    oauth_account: Option<String>,
    grok_account: Option<String>,
}

impl CommonArgs {
    fn parse(args: &[String]) -> AppResult<Self> {
        Ok(Self {
            suite: value_after(args, "--suite"),
            dataset_root: value_after(args, "--dataset-root").map(PathBuf::from),
            codex_version: value_after(args, "--codex-version")
                .unwrap_or_else(default_codex_version),
            profile: value_after(args, "--profile"),
            oauth_account: value_after(args, "--oauth-account"),
            grok_account: value_after(args, "--grok-account"),
            executor: value_after(args, "--executor").unwrap_or_else(|| {
                if cfg!(target_os = "windows") {
                    "windows-sandbox".into()
                } else {
                    "docker".into()
                }
            }),
        })
    }
}

struct RunArgs {
    suite: String,
    models: Vec<String>,
    repeat: Option<u32>,
    dataset_root: Option<PathBuf>,
    codex_version: String,
    max_tasks: Option<usize>,
    max_wall_seconds: Option<u64>,
    max_total_tokens: Option<u64>,
    natural_context: bool,
    resume_run: Option<String>,
    task_filter: Option<String>,
    category_filter: Option<String>,
    provider_filter: Option<String>,
    tag_filter: Option<String>,
    seed: u64,
    control_model: Option<String>,
    baseline_run: Option<String>,
    compaction_engine: String,
    recovery_mode: String,
    grok_compaction: String,
    profile: Option<String>,
    lane: Option<String>,
    transition: Option<String>,
    schedule: String,
    fail_fast: Option<String>,
    executor: String,
    catalog_mode: String,
    oauth_account: Option<String>,
    grok_account: Option<String>,
    max_official_turns: Option<u32>,
    ablation_profile: Option<String>,
}

impl RunArgs {
    fn parse(args: &[String], matrix: bool) -> AppResult<Self> {
        let suite = value_after(args, "--suite")
            .ok_or_else(|| AppError::Message("--suite is required".into()))?;
        let quick_gate = suite == "canonical-gate-4";
        let profile = value_after(args, "--profile")
            .or_else(|| (matrix && quick_gate).then(|| "canonical-gate-4".into()));
        let models: Vec<String> = if matrix {
            value_after(args, "--models")
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .collect()
        } else {
            value_after(args, "--model").into_iter().collect()
        };
        if models.is_empty() && profile.is_none() {
            return Err(AppError::Message(if matrix {
                "--models or --profile is required".into()
            } else {
                "--model or --profile is required".into()
            }));
        }
        let parsed = Self {
            suite,
            models,
            repeat: parse_after(args, "--repeat")?,
            dataset_root: value_after(args, "--dataset-root").map(PathBuf::from),
            codex_version: value_after(args, "--codex-version")
                .unwrap_or_else(default_codex_version),
            max_tasks: parse_after(args, "--max-tasks")?,
            max_wall_seconds: parse_after(args, "--wall-time-limit")?
                .or(parse_after(args, "--max-wall-seconds")?)
                .or(quick_gate.then_some(600)),
            max_total_tokens: parse_after(args, "--max-total-tokens")?,
            natural_context: args.iter().any(|argument| argument == "--natural-context"),
            resume_run: value_after(args, "--resume"),
            task_filter: value_after(args, "--task"),
            category_filter: value_after(args, "--category"),
            provider_filter: value_after(args, "--provider"),
            tag_filter: value_after(args, "--tag"),
            seed: parse_after(args, "--seed")?.unwrap_or(0),
            control_model: value_after(args, "--control-model"),
            baseline_run: value_after(args, "--baseline"),
            compaction_engine: value_after(args, "--compaction-engine")
                .unwrap_or_else(|| "codex-local-v0-150".into()),
            recovery_mode: value_after(args, "--recovery-mode").unwrap_or_else(|| "shadow".into()),
            grok_compaction: value_after(args, "--grok-compaction")
                .unwrap_or_else(|| "desktop".into()),
            executor: value_after(args, "--executor").unwrap_or_else(runner::default_executor_name),
            catalog_mode: value_after(args, "--catalog-mode")
                .unwrap_or_else(|| "production".into()),
            oauth_account: value_after(args, "--oauth-account"),
            grok_account: value_after(args, "--grok-account"),
            max_official_turns: parse_after(args, "--max-official-turns")?,
            profile,
            lane: value_after(args, "--lane"),
            transition: value_after(args, "--transition"),
            schedule: value_after(args, "--schedule").unwrap_or_else(|| {
                if quick_gate {
                    "round-robin".into()
                } else {
                    "model-major".into()
                }
            }),
            fail_fast: value_after(args, "--fail-fast")
                .or_else(|| quick_gate.then(|| "protocol".into())),
            ablation_profile: value_after(args, "--ablation-profile"),
        };
        if let Some(profile) = parsed.ablation_profile.as_deref() {
            if vellum_enhanced_codex::AblationProfile::parse(profile).is_none() {
                return Err(AppError::Message(
                    "--ablation-profile must be E0, E1, E2, E3, E4, or E5".into(),
                ));
            }
        }
        if parsed.repeat == Some(0)
            || parsed.max_tasks == Some(0)
            || parsed.max_wall_seconds == Some(0)
            || parsed.max_total_tokens == Some(0)
            || parsed.max_official_turns == Some(0)
        {
            return Err(AppError::Message(
                "repeat and run budget values must be greater than zero".into(),
            ));
        }
        // Only the two arms of the switchover A/B remain: the embedded
        // production engine, and Codex core's own native local compact as the
        // oracle it is measured against. The retired Canonical engines are not
        // selectable, so a stale command line fails loudly instead of silently
        // measuring an engine that no longer exists.
        if !matches!(
            parsed.compaction_engine.as_str(),
            "codex-local-v0-150" | "codex-local-native-oracle"
        ) {
            return Err(AppError::Message(
                "--compaction-engine must be codex-local-v0-150 or codex-local-native-oracle"
                    .into(),
            ));
        }
        if !matches!(parsed.recovery_mode.as_str(), "shadow" | "recover") {
            return Err(AppError::Message(
                "--recovery-mode must be shadow or recover".into(),
            ));
        }
        // Grok native compaction is retired; a Grok route compacts with the
        // embedded engine like every other third-party route.
        if parsed.grok_compaction.as_str() != "desktop" {
            return Err(AppError::Message(
                "--grok-compaction must be desktop; Grok native compaction is retired".into(),
            ));
        }

        if !matches!(
            parsed.executor.as_str(),
            "docker" | "windows" | "windows-sandbox"
        ) {
            return Err(AppError::Message(
                "--executor must be docker, windows, or windows-sandbox".into(),
            ));
        }
        if matches!(parsed.executor.as_str(), "windows" | "windows-sandbox")
            && !cfg!(target_os = "windows")
        {
            return Err(AppError::Message(
                "Windows executors are only available on Windows".into(),
            ));
        }
        if !matches!(parsed.catalog_mode.as_str(), "production" | "legacy") {
            return Err(AppError::Message(
                "--catalog-mode must be production or legacy".into(),
            ));
        }
        if !matches!(parsed.schedule.as_str(), "model-major" | "round-robin") {
            return Err(AppError::Message(
                "--schedule must be model-major or round-robin".into(),
            ));
        }
        if parsed
            .fail_fast
            .as_deref()
            .is_some_and(|value| value != "protocol")
        {
            return Err(AppError::Message(
                "--fail-fast currently supports only protocol".into(),
            ));
        }
        Ok(parsed)
    }

    fn into_options(self) -> RunOptions {
        RunOptions {
            suite: self.suite,
            models: self.models,
            repeat: self.repeat,
            dataset_root: self.dataset_root,
            codex_version: self.codex_version,
            max_tasks: self.max_tasks,
            max_wall_seconds: self.max_wall_seconds,
            max_total_tokens: self.max_total_tokens,
            natural_context: self.natural_context,
            resume_run: self.resume_run,
            task_filter: self.task_filter,
            category_filter: self.category_filter,
            provider_filter: self.provider_filter,
            tag_filter: self.tag_filter,
            seed: self.seed,
            control_model: self.control_model,
            baseline_run: self.baseline_run,
            compaction_engine: self.compaction_engine,
            recovery_mode: self.recovery_mode,
            grok_compaction: self.grok_compaction,
            profile: self.profile,
            lane: self.lane,
            transition: self.transition,
            schedule: self.schedule,
            fail_fast: self.fail_fast,
            executor: self.executor,
            catalog_mode: self.catalog_mode,
            oauth_account: self.oauth_account,
            grok_account: self.grok_account,
            max_official_turns: self.max_official_turns,
            ablation_profile: self.ablation_profile,
        }
    }
}

fn value_after(args: &[String], flag: &str) -> Option<String> {
    args.windows(2)
        .find(|pair| pair[0] == flag)
        .map(|pair| pair[1].clone())
        .or_else(|| {
            args.iter().find_map(|argument| {
                argument
                    .strip_prefix(&format!("{flag}="))
                    .map(str::to_string)
            })
        })
}

fn parse_after<T: std::str::FromStr>(args: &[String], flag: &str) -> AppResult<Option<T>> {
    value_after(args, flag)
        .map(|value| {
            value
                .parse()
                .map_err(|_| AppError::Message(format!("invalid value for {flag}: {value}")))
        })
        .transpose()
}

fn print_help() {
    println!(
        r#"Vellum third-party model Harness evaluator

Usage:
  vellum-eval doctor [--profile desktop-six] [--oauth-account EMAIL|ID] [--grok-account EMAIL|ID] [--codex-version 0.142.5]
  vellum-eval models [--include-official]
  vellum-eval investigation-replay [--file PATH] [--mode shadow|recover]
  vellum-eval protocol-replay [--file PATH]
  vellum-eval app-replay --suite desktop-protocol-replay
  vellum-eval parity [--models <id,id,...>|--profile desktop-six] [--executor windows]
  vellum-eval app-server-parity [--models <id,id,...>|--profile issue-7-required]
  vellum-eval qualify-issue-7 --grok-run RUN --glm-run RUN --parity PATH --app-parity PATH
  vellum-eval prepare --suite <smoke|core-30> [--dataset-root PATH]
  vellum-eval live [--route qwen|opencode|grok|grok-attribution|official|brave|all] [--phase candidate] [--artifact-dir PATH]
  vellum-eval enhanced-mvp-fixtures
  vellum-eval enhanced-runtime-status [--json]
  vellum-eval enhanced-integration-gate --mode bridge|installed [--report-root PATH]
  vellum-eval enhanced-integration-gate --mode live --provider-url URL --provider-model ID
  vellum-eval zcode-desktop-canary [--repeat 2] [--settle-ms 15000] [--expected-cjs-sha256 SHA256] [--session-id sess_...]
  vellum-eval sandbox-preflight [--codex-version 0.142.5]
  vellum-eval run --suite <suite> --model <catalog-id> [--repeat N]
  vellum-eval matrix --suite <suite> (--models <id,id,...>|--profile desktop-six) [--repeat N]
  vellum-eval report <run-id> [--format html|json]
  vellum-eval triage <run-id>

Run controls:
  --max-tasks N           Limit scheduled tasks
  --max-wall-seconds N    Stop scheduling after the wall-time budget
  --wall-time-limit N     Alias for --max-wall-seconds
  --max-total-tokens N    Stop scheduling after the run token budget
  --max-official-turns N  Stop scheduling after the Official turn budget for this run
  --natural-context       Use each model's real window instead of forced test windows
  --resume RUN_ID         Continue pending cases and rerun retryable invalid attempts
  --task ID               Run one task id
  --category NAME         Filter by task category
  --provider NAME         Filter selected models by provider
  --tag TAG               Filter by manifest tag
  --seed N                Record the deterministic run seed
  --control-model ID      Add a sampled official control model
  --oauth-account VALUE   Pin Official traffic to native Codex auth or a managed email/ID
  --grok-account VALUE    Pin Grok traffic to a managed email/ID
  --baseline RUN_ID       Record the baseline used for regression comparison
  --compaction-engine E   Eval arm: codex-local-v0-150 (embedded, default) or codex-local-native-oracle
  --recovery-mode MODE    Behavioral guards: shadow (default) or recover
  --ablation-profile Px   Enhanced Codex live matrix profile: E0, E1, E2, E3, E4, or E5 (explicit; never inferred from model name)
  --grok-compaction MODE  Grok compactor path: desktop (Grok native compaction is retired)
  --executor MODE         windows-sandbox (authoritative), windows (diagnostic), or docker (synthetic)
  --catalog-mode MODE     production parity or eval-only legacy compatibility
  --profile NAME          Resolve a portable model profile (desktop-six)
  --lane NAME             Limit desktop-six to grok, luna, nemotron, or laguna
  --transition MODE       directed, cyclic, or all-round-trips
  --schedule MODE         model-major or round-robin
  --fail-fast protocol    Stop after the first protocol-layer failure
  --compare RUN_ID        Compare a generated report with another run
  --codex-version VERSION Select the pinned Codex CLI container version

Live run results are written to target/vellum-evals/<run-id>.
Provider credentials remain in Vellum and are never mounted into task containers."#
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_equals_and_separate_cli_values() {
        let args = vec!["--suite=smoke".into(), "--repeat".into(), "3".into()];
        assert_eq!(value_after(&args, "--suite").as_deref(), Some("smoke"));
        assert_eq!(parse_after::<u32>(&args, "--repeat").unwrap(), Some(3));
    }

    #[test]
    fn matrix_profile_does_not_require_explicit_model_ids() {
        let args = vec![
            "--suite".into(),
            "switch-matrix-6".into(),
            "--profile".into(),
            "desktop-six".into(),
            "--transition".into(),
            "directed".into(),
        ];
        let parsed = RunArgs::parse(&args, true).unwrap();
        assert!(parsed.models.is_empty());
        assert_eq!(parsed.profile.as_deref(), Some("desktop-six"));
        assert_eq!(parsed.transition.as_deref(), Some("directed"));
        assert_eq!(parsed.compaction_engine, "codex-local-v0-150");
    }

    #[test]
    fn parses_the_two_switchover_arms_and_defaults_to_the_embedded_engine() {
        let base = vec![
            "--suite".into(),
            "compaction-engine-ab".into(),
            "--model".into(),
            "grok-4".into(),
        ];
        assert_eq!(
            RunArgs::parse(&base, false).unwrap().compaction_engine,
            "codex-local-v0-150",
            "the embedded production engine is the default arm"
        );
        for arm in ["codex-local-v0-150", "codex-local-native-oracle"] {
            let mut args = base.clone();
            args.push("--compaction-engine".into());
            args.push(arm.into());
            assert_eq!(RunArgs::parse(&args, false).unwrap().compaction_engine, arm);
        }
    }

    #[test]
    fn recovery_mode_is_explicit_and_validated() {
        let base = vec![
            "--suite".into(),
            "compaction-engine-ab".into(),
            "--model".into(),
            "grok-4".into(),
        ];
        assert_eq!(
            RunArgs::parse(&base, false).unwrap().recovery_mode,
            "shadow"
        );

        let mut recover = base.clone();
        recover.extend(["--recovery-mode".into(), "recover".into()]);
        assert_eq!(
            RunArgs::parse(&recover, false).unwrap().recovery_mode,
            "recover"
        );

        let mut invalid = base;
        invalid.extend(["--recovery-mode".into(), "on".into()]);
        let error = match RunArgs::parse(&invalid, false) {
            Ok(_) => panic!("invalid recovery mode must fail"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("shadow or recover"), "{error}");
    }

    #[test]
    fn a_retired_canonical_engine_is_refused_rather_than_silently_measured() {
        for retired in [
            "legacy",
            "canonical",
            "canonical-v1",
            "canonical-v2",
            "codex-local",
        ] {
            let args = vec![
                "--suite".into(),
                "compaction-engine-ab".into(),
                "--model".into(),
                "grok-4".into(),
                "--compaction-engine".into(),
                retired.into(),
            ];
            let error = match RunArgs::parse(&args, false) {
                Ok(_) => panic!("{retired} must not be selectable"),
                Err(error) => error.to_string(),
            };
            assert!(error.contains("codex-local-v0-150"), "{error}");
        }
    }

    #[test]
    fn an_explicit_grok_compactor_is_refused() {
        // Grok native compaction is retired: a Grok route compacts with the
        // embedded engine like every other third-party route, so an explicit
        // Grok compactor cannot change the engine under test.
        let args = vec![
            "--suite".into(),
            "compaction-engine-ab".into(),
            "--model".into(),
            "grok-4".into(),
            "--grok-compaction".into(),
            "canonical".into(),
        ];
        let error = match RunArgs::parse(&args, false) {
            Ok(_) => panic!("an explicit Grok compactor must be refused"),
            Err(error) => error.to_string(),
        };
        assert!(
            error.contains("Grok native compaction is retired"),
            "{error}"
        );
    }

    #[test]
    fn canonical_gate_applies_bounded_round_robin_defaults() {
        let args = vec!["--suite".into(), "canonical-gate-4".into()];
        let parsed = RunArgs::parse(&args, true).unwrap();
        assert_eq!(parsed.profile.as_deref(), Some("canonical-gate-4"));
        assert_eq!(parsed.max_wall_seconds, Some(600));
        assert_eq!(parsed.schedule, "round-robin");
        assert_eq!(parsed.fail_fast.as_deref(), Some("protocol"));
        assert_eq!(
            parsed.executor,
            if cfg!(target_os = "windows") {
                "windows-sandbox"
            } else {
                "docker"
            }
        );
    }

    #[test]
    fn checked_in_suites_are_valid_and_fixed_size() {
        let paths = EvalPaths::discover(None).unwrap();
        let smoke = manifest::SuiteManifest::load(&paths.suite_path("smoke")).unwrap();
        let compact_smoke =
            manifest::SuiteManifest::load(&paths.suite_path("compact-smoke")).unwrap();
        let core = manifest::SuiteManifest::load(&paths.suite_path("core-30")).unwrap();
        let continuity = manifest::SuiteManifest::load(&paths.suite_path("continuity-12")).unwrap();
        let natural = manifest::SuiteManifest::load(&paths.suite_path("natural-compact")).unwrap();
        let stability =
            manifest::SuiteManifest::load(&paths.suite_path("harness-stability-12")).unwrap();
        let switches = manifest::SuiteManifest::load(&paths.suite_path("switch-matrix-6")).unwrap();
        let engine_ab =
            manifest::SuiteManifest::load(&paths.suite_path("compaction-engine-ab")).unwrap();
        let issue7 = manifest::SuiteManifest::load(&paths.suite_path("issue-7-smoke-3")).unwrap();
        let deepseek_luna =
            manifest::SuiteManifest::load(&paths.suite_path("deepseek-luna-swe-roundtrip"))
                .unwrap();
        let swe_gate =
            manifest::SuiteManifest::load(&paths.suite_path("swe-terminal-gate")).unwrap();
        assert_eq!(smoke.tasks.len(), 6);
        assert_eq!(compact_smoke.tasks.len(), 1);
        assert_eq!(compact_smoke.tasks[0].forced_context_window, Some(24_000));
        assert_eq!(core.tasks.len(), 30);
        assert_eq!(continuity.tasks.len(), 3);
        assert_eq!(natural.tasks.len(), 1);
        assert_eq!(stability.tasks.len(), 12);
        assert_eq!(switches.tasks.len(), 3);
        assert_eq!(engine_ab.tasks.len(), 4);
        assert_eq!(engine_ab.tasks[0].id, "engine-ab-single-resume");
        assert!(engine_ab
            .tasks
            .iter()
            .all(|task| task.expected_harness.min_compactions >= 1
                && task.expected_harness.require_session_resume
                && task.forced_context_window == Some(15_000)));
        assert_eq!(engine_ab.tasks[3].expected_harness.min_compactions, 2);
        assert_eq!(issue7.tasks.len(), 3);
        assert_eq!(deepseek_luna.tasks.len(), 1);
        assert_eq!(swe_gate.tasks.len(), 1);
        assert!(swe_gate.tasks[0].grader_image_archive.is_some());
        assert_eq!(
            swe_gate.tasks[0].grader_image.as_deref(),
            Some("sweb.eval.x86_64.psf__requests-1142:cfg-64ac3483a165")
        );
        assert!(
            !swe_gate.tasks[0]
                .grader_image
                .as_deref()
                .unwrap_or_default()
                .contains("ghcr.io"),
            "release SWE gate must not pin an unobtainable GHCR image"
        );
        assert!(deepseek_luna.tasks[0].grader_image_archive.is_some());
        assert_eq!(
            deepseek_luna.tasks[0].phases[0].reasoning_effort.as_deref(),
            Some("xhigh")
        );
        assert!(issue7
            .tasks
            .iter()
            .all(|task| task.expected_harness.require_terminal_sse));
        assert!(switches.tasks.iter().any(|task| matches!(
            task.transition_mode,
            manifest::TransitionMode::AllRoundTrips
        )));
        let profile_models = (0..5).map(|index| format!("m{index}")).collect::<Vec<_>>();
        let directed = switches
            .tasks
            .iter()
            .filter(|task| {
                matches!(
                    task.transition_mode,
                    manifest::TransitionMode::SameModel | manifest::TransitionMode::OrderedPair
                )
            })
            .map(|task| {
                profile_models
                    .iter()
                    .map(|model| runner::switch_targets(task, &profile_models, model).len())
                    .sum::<usize>()
            })
            .sum::<usize>();
        let all_round_trips = switches
            .tasks
            .iter()
            .filter(|task| {
                matches!(
                    task.transition_mode,
                    manifest::TransitionMode::AllRoundTrips
                )
            })
            .map(|task| {
                profile_models
                    .iter()
                    .map(|model| runner::switch_targets(task, &profile_models, model).len())
                    .sum::<usize>()
            })
            .sum::<usize>();
        assert_eq!(directed, 25);
        assert_eq!(all_round_trips, 20);
        let models = vec!["a".into(), "b".into(), "c".into()];
        let continuity_cases = continuity
            .tasks
            .iter()
            .map(|task| {
                models
                    .iter()
                    .map(|model| runner::switch_targets(task, &models, model).len())
                    .sum::<usize>()
            })
            .sum::<usize>();
        assert_eq!(continuity_cases, 12);
        let count = |category: manifest::TaskCategory| {
            core.tasks
                .iter()
                .filter(|task| {
                    std::mem::discriminant(&task.category) == std::mem::discriminant(&category)
                })
                .count()
        };
        assert_eq!(count(manifest::TaskCategory::ShortFix), 10);
        assert_eq!(count(manifest::TaskCategory::MultiFile), 8);
        assert_eq!(count(manifest::TaskCategory::ToolRecovery), 4);
        assert_eq!(count(manifest::TaskCategory::LongContext), 4);
        assert_eq!(count(manifest::TaskCategory::ModelSwitch), 4);
        assert!(core.tasks.iter().any(|task| {
            task.faults
                .iter()
                .any(|fault| matches!(fault.kind, manifest::FaultKind::Http524))
        }));
        assert!(core.tasks.iter().any(|task| {
            task.faults
                .iter()
                .any(|fault| matches!(fault.kind, manifest::FaultKind::FragmentSse))
        }));
    }

    #[test]
    fn enhanced_quick_manifests_have_three_fixed_roles() {
        let paths = EvalPaths::discover(None).unwrap();
        for module in ["e1", "e2", "e3"] {
            let path = paths
                .evals_root
                .join("manifests")
                .join(format!("enhanced-quick-{module}.json"));
            let suite = manifest::SuiteManifest::load(&path).unwrap();
            assert_eq!(suite.tasks.len(), 3);
            assert_eq!(
                suite
                    .tasks
                    .iter()
                    .filter(|task| task.tags.iter().any(|tag| tag == "target"))
                    .count(),
                1
            );
            assert_eq!(
                suite
                    .tasks
                    .iter()
                    .filter(|task| task.tags.iter().any(|tag| tag == "control"))
                    .count(),
                2
            );
            assert!(suite.tasks.iter().all(|task| task
                .expected_harness
                .min_enhanced_events
                .contains_key("enhanced.session.features_applied")));
        }
        let holdout = manifest::SuiteManifest::load(
            &paths
                .evals_root
                .join("manifests")
                .join("enhanced-holdout-pool.json"),
        )
        .unwrap();
        assert_eq!(holdout.tasks.len(), 6);
        for module in ["E1", "E2", "E3"] {
            assert_eq!(
                holdout
                    .tasks
                    .iter()
                    .filter(|task| task
                        .tags
                        .iter()
                        .any(|tag| tag == &format!("holdout-{module}")))
                    .count(),
                3
            );
        }
    }

    #[test]
    fn expanded_swe_suite_fails_closed_without_immutable_grader_images() {
        let paths = EvalPaths::discover(None).unwrap();
        let fixture = paths
            .evals_root
            .join("fixtures")
            .join("invalid-manifest")
            .join("swe-terminal-gate-expanded.json");
        let error = manifest::SuiteManifest::load(&fixture)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("immutable"),
            "expanded suite still uses mutable local tags: {error}"
        );
        assert!(
            !paths.suite_path("swe-terminal-gate-expanded").is_file(),
            "expanded suite must stay out of active suites"
        );
    }
}
