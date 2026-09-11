use std::path::PathBuf;
use std::time::Duration;

use tokio::time::timeout;
use vellum_zcode_desktop::{
    default_log_dir, ArtifactPin, ControlEvent, TurnOutcome, ZcodeDesktopHost,
};

use crate::error::{AppError, AppResult};

pub async fn run(args: &[String]) -> AppResult<()> {
    let log_dir = value_after(args, "--log-dir")
        .map(PathBuf::from)
        .unwrap_or_else(default_log_dir);
    let bindings = value_after(args, "--bindings")
        .map(PathBuf::from)
        .unwrap_or_else(|| log_dir.join("eval-thread-bindings.sqlite"));
    let repeat = value_after(args, "--repeat")
        .map(|value| value.parse::<usize>())
        .transpose()
        .map_err(|_| AppError::Message("--repeat must be a positive integer".into()))?
        .unwrap_or(2);
    if repeat == 0 {
        return Err(AppError::Message("--repeat must be at least 1".into()));
    }
    let timeout_ms = value_after(args, "--timeout-ms")
        .map(|value| value.parse::<u64>())
        .transpose()
        .map_err(|_| AppError::Message("--timeout-ms must be an integer".into()))?
        .unwrap_or(180_000);
    let settle_ms = value_after(args, "--settle-ms")
        .map(|value| value.parse::<u64>())
        .transpose()
        .map_err(|_| AppError::Message("--settle-ms must be an integer".into()))?
        .unwrap_or(15_000);
    let expected_sha = value_after(args, "--expected-cjs-sha256")
        .or_else(|| std::env::var("ZCODE_TAP_PIN_CJS_SHA256").ok());
    let pin = expected_sha.map(|cjs_sha256| ArtifactPin { cjs_sha256 });
    let requested_session = value_after(args, "--session-id");
    let thread_id = value_after(args, "--thread-id")
        .unwrap_or_else(|| format!("vellum-eval-zcode-{}", std::process::id()));
    let workspace = std::env::current_dir()
        .map_err(|error| AppError::Message(format!("resolve eval workspace: {error}")))?;

    let host = ZcodeDesktopHost::connect(&log_dir, &bindings, pin.as_ref())
        .await
        .map_err(message)?;
    let artifact = host.hello().artifact.clone();
    let bound = host
        .bind_thread(
            &thread_id,
            requested_session.as_deref(),
            &workspace.to_string_lossy(),
        )
        .await
        .map_err(message)?;
    let pinned_session = bound.session_id;

    println!(
        "ZCODE desktop={} cli_sha256={} session={} thread={}",
        artifact.product_version.as_deref().unwrap_or("unknown"),
        artifact.cjs_sha256,
        pinned_session,
        thread_id
    );

    for index in 1..=repeat {
        let expected = format!("PONG-{index}");
        let prompt = format!("Reply with exactly {expected}");
        let turn_id = format!("{thread_id}-turn-{index}");
        let mut events = host.subscribe();
        let started = host
            .start_turn_with_id(&thread_id, &turn_id, &prompt, Some(timeout_ms))
            .await
            .map_err(message)?;
        if started.session_id != pinned_session {
            return Err(AppError::Message(format!(
                "turn {index} escaped the pinned session: expected {pinned_session}, got {}",
                started.session_id
            )));
        }

        let (done, answer) = timeout(Duration::from_millis(timeout_ms + 5_000), async {
            let mut answer = String::new();
            loop {
                match events.recv().await {
                    Ok(ControlEvent::TurnDelta(delta)) if delta.vellum_turn_id == turn_id => {
                        if let Some(text) = delta.text {
                            answer.push_str(&text);
                        }
                    }
                    Ok(ControlEvent::TurnCompleted(done)) if done.vellum_turn_id == turn_id => {
                        return Ok((done, answer));
                    }
                    Ok(ControlEvent::Lifecycle(event)) => {
                        eprintln!(
                            "ZCODE lifecycle={:?} detail={:?}",
                            event.state, event.detail
                        );
                    }
                    Ok(_) => {}
                    Err(error) => {
                        return Err(AppError::Message(format!(
                            "ZCode control event stream closed: {error}"
                        )));
                    }
                }
            }
        })
        .await
        .map_err(|_| AppError::Message(format!("ZCode turn {index} exceeded eval deadline")))??;

        if done.session_id != pinned_session {
            return Err(AppError::Message(format!(
                "completed turn {index} reported a different session: {}",
                done.session_id
            )));
        }
        if done.outcome != TurnOutcome::Completed {
            return Err(AppError::Message(format!(
                "ZCode turn {index} ended {:?}: {} {}",
                done.outcome,
                done.error_code.as_deref().unwrap_or(""),
                done.error_message.as_deref().unwrap_or("")
            )));
        }
        if done.provider_id.as_deref() != Some("builtin:zai-start-plan") {
            return Err(AppError::Message(format!(
                "ZCode turn {index} did not prove the built-in Start Plan path: provider={:?}",
                done.provider_id
            )));
        }
        if !answer.contains(&expected) {
            return Err(AppError::Message(format!(
                "ZCode turn {index} completed without expected answer {expected:?}; received {} characters",
                answer.chars().count()
            )));
        }
        println!(
            "PASS turn={} session={} provider={} model={} answer={}",
            index,
            done.session_id,
            done.provider_id.as_deref().unwrap_or("unknown"),
            done.model_id.as_deref().unwrap_or("unknown"),
            expected
        );
        // ZCode starts a host-owned title/summary model request immediately
        // after the visible turn reaches completedSuccess. Start Plan may
        // serialize that background request with the next user model call.
        if index < repeat && settle_ms > 0 {
            tokio::time::sleep(Duration::from_millis(settle_ms)).await;
        }
    }

    println!(
        "PASS zcode-desktop-canary same-session turns={} session={}",
        repeat, pinned_session
    );
    Ok(())
}

fn value_after(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|argument| argument == flag)
        .and_then(|index| args.get(index + 1))
        .cloned()
        .or_else(|| {
            args.iter().find_map(|argument| {
                argument
                    .strip_prefix(&format!("{flag}="))
                    .map(str::to_owned)
            })
        })
}

fn message(error: impl std::fmt::Display) -> AppError {
    AppError::Message(error.to_string())
}
