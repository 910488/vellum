//! Interactive acceptance harness using the production manager. It never reads
//! Desktop auth files. Supply an explicit project and isolated data directory.
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use vellum_lib::claude_code::CodeManager;

#[tokio::main]
async fn main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let mut project = None;
    let mut root = None;
    let mut passthrough = false;
    let mut authorize_only = false;
    let mut login_once = false;
    let mut evidence_file = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--project" => project = args.next().map(PathBuf::from),
            "--data-root" => root = args.next().map(PathBuf::from),
            "--pass-through" => passthrough = true,
            "--authorize-only" => authorize_only = true,
            "--evidence-file" => evidence_file = args.next().map(PathBuf::from),
            "--login-once" => {
                authorize_only = true;
                login_once = true;
            }
            _ => {
                return Err(
                    "usage: --data-root ISOLATED_PATH [--project PATH] [--pass-through] [--authorize-only] [--login-once]".into(),
                )
            }
        }
    }
    let manager = CodeManager::new(root.ok_or("--data-root is required")?);
    if login_once {
        let prior: std::collections::HashSet<String> = manager
            .accounts
            .view()
            .await?
            .accounts
            .into_iter()
            .map(|account| account.id)
            .collect();
        let url = manager.accounts.begin().await?;
        println!("{}", serde_json::json!({"authorizeUrl": url}));
        loop {
            let view = manager.accounts.view().await?;
            if !view.login_pending {
                if let Some(error) = view.login_error {
                    return Err(error);
                }
                let added: Vec<_> = view
                    .accounts
                    .iter()
                    .filter(|account| !prior.contains(&account.id))
                    .map(|account| &account.id)
                    .collect();
                if added.len() != 1 {
                    return Err("Authorization did not add one distinct Claude account".into());
                }
                println!(
                    "{}",
                    serde_json::json!({
                        "authorizedAccountIds": added,
                        "selected": view.selected,
                    })
                );
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }
    if passthrough {
        manager.accounts.select(None).await?;
    }
    if !authorize_only {
        manager
            .start(project.clone().ok_or("--project is required")?)
            .await?;
    }
    if let Some(path) = evidence_file {
        let manager = Arc::downgrade(&manager);
        tokio::spawn(async move {
            loop {
                let Some(manager) = manager.upgrade() else {
                    break;
                };
                if let Ok(status) = manager.status().await {
                    // Bounded routing metadata only: no tokens, account labels,
                    // project paths, prompts, tools or response contents.
                    let evidence = serde_json::json!({
                        "observedAt": std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs(),
                        "running": status.running,
                        "selectedAccountId": status.accounts.selected,
                        "routing": status.routing,
                        "trust": status.trust,
                    });
                    let temporary = path.with_extension("tmp");
                    let written = serde_json::to_vec_pretty(&evidence)
                        .ok()
                        .is_some_and(|bytes| {
                            std::fs::write(&temporary, bytes).is_ok()
                                && std::fs::rename(&temporary, &path).is_ok()
                        });
                    if !written {
                        eprintln!("Cannot persist routing acceptance evidence");
                        break;
                    }
                }
                drop(manager);
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        });
    }
    println!(
        "{}",
        serde_json::to_string(&manager.status().await?).map_err(|e| e.to_string())?
    );
    println!("Commands (one JSON per line): status, login, cancel_login, select(accountId), usage(accountId), stop, quit. Stop/quit drain safely; force:true explicitly cancels active work. No tokens are printed.");
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        let line = tokio::select! {
            line=lines.next_line()=>line.map_err(|e|e.to_string())?,
            _=tokio::signal::ctrl_c()=>None,
        };
        let Some(line) = line else {
            manager.stop(true).await?;
            break;
        };
        let value = match serde_json::from_str::<serde_json::Value>(&line) {
            Ok(v) => v,
            Err(_) => {
                println!("{{\"error\":\"Expected a JSON command\"}}");
                continue;
            }
        };
        let op = value["op"].as_str().unwrap_or("");
        let result = match op {
            "status" => Ok(()),
            "prepare_certificate" => manager.prepare_certificate().await,
            "trust_certificate" => {
                manager
                    .trust_certificate(value["sha256"].as_str().unwrap_or("").to_owned())
                    .await
            }
            "restore_trust" => manager.restore_trust().await,
            "start" => match project.clone() {
                Some(project) => manager.start(project).await,
                None => Err("--project is required for routing".into()),
            },
            "login" => match manager.accounts.begin().await {
                Ok(url) => {
                    println!("{}", serde_json::json!({"authorizeUrl":url}));
                    Ok(())
                }
                Err(e) => Err(e),
            },
            "cancel_login" => {
                manager.accounts.cancel_login().await;
                Ok(())
            }
            "select" => {
                manager
                    .accounts
                    .select(value["accountId"].as_str().map(str::to_owned))
                    .await
            }
            "usage" => {
                manager
                    .accounts
                    .refresh_usage(value["accountId"].as_str().unwrap_or(""))
                    .await
            }
            "stop" | "quit" => {
                manager
                    .stop(value["force"].as_bool().unwrap_or(false))
                    .await
            }
            _ => Err("Unknown command".into()),
        };
        if let Err(error) = result {
            println!("{}", serde_json::json!({"error":error}));
            continue;
        }
        println!(
            "{}",
            serde_json::to_string(&manager.status().await?).map_err(|e| e.to_string())?
        );
        if op == "quit" {
            break;
        }
    }
    Ok(())
}
