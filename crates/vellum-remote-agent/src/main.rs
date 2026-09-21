//! Vellum remote-agent CLI.
//!
//! Invocation model for the first slice: Desktop/SSH runs one request JSON and
//! receives one response JSON on stdout. No arbitrary shell execution surface.
//!
//! Preferred RPC transport is JSON on stdin:
//!   ssh host vellum-remote-agent rpc < request.json

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{Parser, Subcommand};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use vellum_remote_agent::configuration::{
    configure, credential_status, put_credential, remove_credential,
};
use vellum_remote_agent::docker::ProcessDockerClient;
use vellum_remote_agent::host_probe::probe_host;
use vellum_remote_agent::operations::BeginOutcome;
use vellum_remote_agent::profile::ProfileManager;
use vellum_remote_agent::protocol::{
    AgentError, AgentRequest, AgentResponse, HostBlocker, HostInventoryV2, HostManagerSnapshot,
    HostStatus, OperationResult,
};
use vellum_remote_agent::proxy::{ProxyManager, ProxyStartRequest};
use vellum_remote_agent::state::{AgentPaths, AgentStateStore};
use vellum_remote_agent::{AGENT_PROTOCOL_VERSION, AGENT_VERSION};

#[derive(Debug, Parser)]
#[command(
    name = "vellum-remote-agent",
    version,
    about = "Vellum remote host lifecycle agent"
)]
struct Cli {
    /// Optional agent state root override (tests / non-default installs).
    #[arg(long, global = true, env = "VELLUM_REMOTE_STATE_ROOT")]
    state_root: Option<PathBuf>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Execute one JSON request from stdin (preferred) or --request (local/dev only).
    Rpc {
        /// Local/dev convenience only. Production Desktop transport must use stdin
        /// so JSON is never re-parsed by a remote shell.
        #[arg(long)]
        request: Option<String>,
    },
    /// Convenience wrappers for common methods.
    Version,
    Status,
    Doctor,
    /// Refresh the detached Grok account (systemd timer entrypoint).
    RefreshGrok,
}

fn main() {
    if let Err(error) = run() {
        let response = AgentResponse::Error {
            error: AgentError::new("agent.failed", error, true),
        };
        println!(
            "{}",
            serde_json::to_string(&response).unwrap_or_else(|_| {
                r#"{"type":"error","error":{"code":"agent.failed","message":"encode failed","repairable":true}}"#.into()
            })
        );
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let cli = Cli::parse();
    let paths = match cli.state_root {
        Some(root) => AgentPaths::from_root(root),
        None => AgentPaths::discover(),
    };
    paths.ensure()?;
    let store = AgentStateStore::new(paths);
    let manager = ProxyManager::with_docker(store.clone(), Arc::new(ProcessDockerClient::new()));
    let profiles = ProfileManager::new(store.clone());

    match cli.command {
        Commands::Version => {
            print_ok(json!({
                "agentVersion": AGENT_VERSION,
                "agentProtocol": AGENT_PROTOCOL_VERSION,
            }))?;
        }
        Commands::Status => {
            let status = host_status(&store, &manager, &profiles)?;
            print_ok(serde_json::to_value(status).map_err(|e| e.to_string())?)?;
        }
        Commands::Doctor => {
            let status = host_status(&store, &manager, &profiles)?;
            print_ok(json!({
                "status": status,
                "notes": doctor_notes(&status),
            }))?;
        }
        Commands::RefreshGrok => {
            let status = vellum_remote_agent::grok::refresh(store.paths())?;
            print_ok(serde_json::to_value(status).map_err(|error| error.to_string())?)?;
        }
        Commands::Rpc { request } => {
            let raw = match request {
                Some(value) => value,
                None => {
                    let mut buf = String::new();
                    io::stdin()
                        .read_to_string(&mut buf)
                        .map_err(|error| format!("failed reading stdin: {error}"))?;
                    buf
                }
            };
            let req: AgentRequest = serde_json::from_str(&raw)
                .map_err(|error| format!("invalid agent request: {error}"))?;
            let value = dispatch(req, &store, &manager, &profiles)?;
            print_ok(value)?;
        }
    }
    Ok(())
}

fn dispatch(
    request: AgentRequest,
    store: &AgentStateStore,
    manager: &ProxyManager,
    profiles: &ProfileManager,
) -> Result<Value, String> {
    match request {
        AgentRequest::AgentVersion => Ok(json!({
            "agentVersion": AGENT_VERSION,
            "agentProtocol": AGENT_PROTOCOL_VERSION,
        })),
        AgentRequest::HostCapabilities => {
            Ok(serde_json::to_value(probe_host()).map_err(|e| e.to_string())?)
        }
        AgentRequest::HostInventoryV2 => {
            Ok(serde_json::to_value(host_inventory(store, manager)?).map_err(|e| e.to_string())?)
        }
        AgentRequest::HostStatus => {
            Ok(serde_json::to_value(host_status(store, manager, profiles)?)
                .map_err(|e| e.to_string())?)
        }
        AgentRequest::HostManagerSnapshot {
            expected_account_id,
            thread_id,
        } => Ok(serde_json::to_value(host_manager_snapshot(
            store,
            manager,
            profiles,
            expected_account_id.as_deref(),
            thread_id.as_deref(),
        )?)
        .map_err(|e| e.to_string())?),
        AgentRequest::ProxyStatus => {
            Ok(serde_json::to_value(manager.status()?).map_err(|e| e.to_string())?)
        }
        AgentRequest::ProxyInstall {
            operation_id,
            image,
            image_digest,
        } => operation_to_value(manager.install(&operation_id, &image, image_digest)?),
        AgentRequest::ProxyLoadImage {
            operation_id,
            staged_path,
            expected_sha256,
            image,
        } => operation_to_value(manager.load_image_archive(
            &operation_id,
            std::path::Path::new(&staged_path),
            &expected_sha256,
            &image,
        )?),
        AgentRequest::ProxyStart {
            operation_id,
            host_port,
            image,
        } => operation_to_value(manager.start(ProxyStartRequest {
            operation_id,
            host_port,
            image,
        })?),
        AgentRequest::ProxyStop { operation_id } => {
            operation_to_value(manager.stop(&operation_id)?)
        }
        AgentRequest::ProxyRestart { operation_id } => {
            operation_to_value(manager.restart(&operation_id)?)
        }
        AgentRequest::ProxyConfigure {
            operation_id,
            config_toml,
        } => run_mutation(
            store,
            &operation_id,
            "proxy.configure",
            json!({"configSha256": sha256(&config_toml)}),
            || {
                if manager.status()?.running {
                    return Err(
                        "ProxyRunning: stop the proxy before replacing its configuration".into(),
                    );
                }
                let mut state = store.load()?;
                let mut resolved =
                    vellum_proxy_runtime::ProxyRuntimeConfig::from_toml_str(&config_toml)
                        .map_err(|error| format!("InvalidProxyConfig: {error}"))?;
                resolved.identity.host_id = state.host_id.clone();
                if let Some(install) = state.install.as_ref() {
                    resolved.identity.install_id = install.install_id.clone();
                    resolved.identity.image_version = install.image.clone();
                }
                let resolved_toml = toml::to_string_pretty(&resolved)
                    .map_err(|error| format!("failed encoding resolved proxy config: {error}"))?;
                let result = configure(store.paths(), &resolved_toml)?;
                if let Some(install) = state.install.as_mut() {
                    install.config_hash = result.config_hash.clone();
                    install.updated_at = chrono::Utc::now();
                }
                store.save(&state)?;
                serde_json::to_value(result).map_err(|e| e.to_string())
            },
        ),
        AgentRequest::ProxyUpdate {
            operation_id,
            image,
            image_digest,
        } => operation_to_value(manager.update(&operation_id, &image, image_digest)?),
        AgentRequest::ProxyLogs { max_bytes } => {
            Ok(serde_json::to_value(manager.logs(max_bytes)?)
                .map_err(|error| error.to_string())?)
        }
        AgentRequest::ProxyRollback { operation_id } => {
            operation_to_value(manager.rollback(&operation_id)?)
        }
        AgentRequest::CredentialPut {
            operation_id,
            credential_id,
            secret,
        } => run_mutation(
            store,
            &operation_id,
            "credential.put",
            json!({"credentialId": credential_id, "secretSha256": sha256(&secret)}),
            || {
                serde_json::to_value(put_credential(store.paths(), &credential_id, &secret)?)
                    .map_err(|e| e.to_string())
            },
        ),
        AgentRequest::CredentialDelete {
            operation_id,
            credential_id,
        } => run_mutation(
            store,
            &operation_id,
            "credential.delete",
            json!({"credentialId": credential_id}),
            || {
                serde_json::to_value(remove_credential(store.paths(), &credential_id)?)
                    .map_err(|e| e.to_string())
            },
        ),
        AgentRequest::CredentialStatus { credential_id } => Ok(serde_json::to_value(
            credential_status(store.paths(), &credential_id)?,
        )
        .map_err(|e| e.to_string())?),
        AgentRequest::GrokInstallAccount {
            operation_id,
            credential_id,
            auth_json,
            version_json,
            agent_id,
        } => run_mutation(
            store,
            &operation_id,
            "grok.installAccount",
            json!({"credentialId": credential_id, "authSha256": sha256(&auth_json)}),
            || {
                serde_json::to_value(vellum_remote_agent::grok::install_account(
                    store.paths(),
                    &credential_id,
                    &auth_json,
                    version_json.as_deref(),
                    agent_id.as_deref(),
                )?)
                .map_err(|error| error.to_string())
            },
        ),
        AgentRequest::GrokStartLogin { operation_id } => {
            run_mutation(store, &operation_id, "grok.startLogin", json!({}), || {
                serde_json::to_value(vellum_remote_agent::grok::start_login(store.paths())?)
                    .map_err(|error| error.to_string())
            })
        }
        AgentRequest::GrokPollLogin => {
            serde_json::to_value(vellum_remote_agent::grok::poll_login(store.paths())?)
                .map_err(|error| error.to_string())
        }
        AgentRequest::GrokCancelLogin { operation_id } => {
            run_mutation(store, &operation_id, "grok.cancelLogin", json!({}), || {
                serde_json::to_value(vellum_remote_agent::grok::cancel_login(store.paths())?)
                    .map_err(|error| error.to_string())
            })
        }
        AgentRequest::GrokStatus => {
            serde_json::to_value(vellum_remote_agent::grok::status(store.paths())?)
                .map_err(|error| error.to_string())
        }
        AgentRequest::GrokRefresh { operation_id } => {
            run_mutation(store, &operation_id, "grok.refresh", json!({}), || {
                serde_json::to_value(vellum_remote_agent::grok::refresh(store.paths())?)
                    .map_err(|error| error.to_string())
            })
        }
        AgentRequest::GrokRemove { operation_id } => {
            run_mutation(store, &operation_id, "grok.remove", json!({}), || {
                serde_json::to_value(vellum_remote_agent::grok::remove(store.paths())?)
                    .map_err(|error| error.to_string())
            })
        }
        AgentRequest::CodexCreateManagedProfile {
            operation_id,
            profile_id,
            auth_json,
        } => run_mutation(
            store,
            &operation_id,
            "codex.createManagedProfile",
            json!({"profileId": profile_id, "authSha256": auth_json.as_deref().map(sha256)}),
            || {
                serde_json::to_value(profiles.create_managed(&profile_id, auth_json.as_deref())?)
                    .map_err(|e| e.to_string())
            },
        ),
        AgentRequest::CodexAdoptExisting {
            operation_id,
            profile_id,
            codex_home,
            explicit_adopt,
        } => run_mutation(
            store,
            &operation_id,
            "codex.adoptExisting",
            json!({"profileId": profile_id, "codexHome": codex_home, "explicitAdopt": explicit_adopt}),
            || {
                serde_json::to_value(profiles.adopt_existing(
                    &profile_id,
                    std::path::Path::new(&codex_home),
                    explicit_adopt,
                )?)
                .map_err(|e| e.to_string())
            },
        ),
        AgentRequest::CodexDiscoverNative => {
            serde_json::to_value(vellum_remote_agent::native_codex::discover_native()?)
                .map_err(|error| error.to_string())
        }
        AgentRequest::CodexPlanNativeAdopt => serde_json::to_value(
            vellum_remote_agent::native_codex::plan_native_adopt(profiles, &manager.status()?)?,
        )
        .map_err(|error| error.to_string()),
        AgentRequest::CodexApplyNativeAdopt {
            operation_id,
            catalog_json,
        } => run_mutation(
            store,
            &operation_id,
            "codex.applyNativeAdopt",
            json!({"catalogSha256": sha256(&catalog_json)}),
            || {
                serde_json::to_value(vellum_remote_agent::native_codex::apply_native_adopt(
                    profiles,
                    &manager.status()?,
                    &catalog_json,
                )?)
                .map_err(|error| error.to_string())
            },
        ),
        AgentRequest::CodexBootstrapNative { operation_id } => run_mutation(
            store,
            &operation_id,
            "codex.bootstrapNative",
            json!({}),
            || {
                serde_json::to_value(vellum_remote_agent::native_codex::bootstrap_native(
                    store.paths(),
                )?)
                .map_err(|error| error.to_string())
            },
        ),
        AgentRequest::CodexInstallPinned {
            operation_id,
            staged_path,
            expected_sha256,
            expected_version,
        } => run_mutation(
            store,
            &operation_id,
            "codex.installPinned",
            json!({"expectedVersion": expected_version, "expectedSha256": expected_sha256}),
            || {
                let codex_home = resolve_codex_home()?;
                serde_json::to_value(vellum_remote_agent::native_codex::install_pinned_codex(
                    &codex_home,
                    Path::new(&staged_path),
                    &expected_sha256,
                    &expected_version,
                    store.paths(),
                )?)
                .map_err(|error| error.to_string())
            },
        ),
        AgentRequest::CodexUpdatePinned {
            operation_id,
            staged_path,
            expected_sha256,
            pinned_version,
        } => run_mutation(
            store,
            &operation_id,
            "codex.updatePinned",
            json!({"pinnedVersion": pinned_version, "expectedSha256": expected_sha256}),
            || {
                let codex_home = resolve_codex_home()?;
                serde_json::to_value(vellum_remote_agent::native_codex::update_pinned_codex(
                    &codex_home,
                    Path::new(&staged_path),
                    &expected_sha256,
                    &pinned_version,
                    store.paths(),
                )?)
                .map_err(|error| error.to_string())
            },
        ),
        AgentRequest::CodexVerifyInstallation { compatible_range } => {
            let codex_home = resolve_codex_home()?;
            Ok(
                serde_json::to_value(vellum_remote_agent::native_codex::verify_installation(
                    &codex_home,
                    compatible_range.as_deref(),
                )?)
                .map_err(|error| error.to_string())?,
            )
        }
        AgentRequest::CodexSessionStatus { thread_id } => {
            let codex_home = resolve_codex_home()?;
            Ok(
                serde_json::to_value(vellum_remote_agent::native_session::query_session_status(
                    &codex_home,
                    thread_id.as_deref(),
                )?)
                .map_err(|error| error.to_string())?,
            )
        }
        AgentRequest::CodexAccountStatus {
            expected_account_id,
        } => {
            let codex_home = resolve_codex_home()?;
            serde_json::to_value(vellum_remote_agent::native_account::status(
                store.paths(),
                &codex_home,
                expected_account_id.as_deref(),
            )?)
            .map_err(|error| error.to_string())
        }
        AgentRequest::CodexAccountLoginStart {
            operation_id,
            expected_account_id,
        } => run_mutation(
            store,
            &operation_id,
            "codex.accountLoginStart",
            json!({"expectedAccountId": expected_account_id}),
            || {
                let codex_home = resolve_codex_home()?;
                serde_json::to_value(vellum_remote_agent::native_account::start_login(
                    store.paths(),
                    &codex_home,
                    &expected_account_id,
                )?)
                .map_err(|error| error.to_string())
            },
        ),
        AgentRequest::CodexAccountLoginPoll => {
            let codex_home = resolve_codex_home()?;
            serde_json::to_value(vellum_remote_agent::native_account::poll_login(
                store.paths(),
                &codex_home,
            )?)
            .map_err(|error| error.to_string())
        }
        AgentRequest::CodexAccountActivate {
            operation_id,
            account_id,
        } => run_mutation(
            store,
            &operation_id,
            "codex.accountActivate",
            json!({"accountId": account_id}),
            || {
                let codex_home = resolve_codex_home()?;
                let snapshot = vellum_remote_agent::native_account::activation_snapshot(
                    store.paths(),
                    &codex_home,
                )?;
                let account = vellum_remote_agent::native_account::activate(
                    store.paths(),
                    &codex_home,
                    &account_id,
                )?;
                let native = vellum_remote_agent::native_codex::discover_native()?;
                if native.daemon_running {
                    if let Err(error) =
                        vellum_remote_agent::native_codex::restart_native(store.paths())
                    {
                        vellum_remote_agent::native_account::rollback_activation(
                            store.paths(),
                            &codex_home,
                            snapshot,
                        )?;
                        return Err(format!("OfficialAccountRestartFailed: {error}"));
                    }
                }
                serde_json::to_value(account).map_err(|error| error.to_string())
            },
        ),
        AgentRequest::CodexRemoteControlPairStart {
            expected_control_account_hash,
        } => {
            let native = vellum_remote_agent::native_codex::discover_native()?;
            serde_json::to_value(
                vellum_remote_agent::mobile_account::remote_control_pair_start(
                    &native,
                    &expected_control_account_hash,
                )?,
            )
            .map_err(|error| error.to_string())
        }
        AgentRequest::ProxyOfficialAccountLoginStart {
            operation_id,
            display_name,
        } => run_mutation(
            store,
            &operation_id,
            "proxy.officialAccountLoginStart",
            json!({"displayName": display_name}),
            || {
                serde_json::to_value(vellum_remote_agent::official_account::login_start(
                    store.paths(),
                    &display_name,
                )?)
                .map_err(|error| error.to_string())
            },
        ),
        AgentRequest::ProxyOfficialAccountLoginPoll { login_id } => serde_json::to_value(
            vellum_remote_agent::official_account::login_poll(store.paths(), &login_id)?,
        )
        .map_err(|error| error.to_string()),
        AgentRequest::ProxyOfficialAccountList => {
            serde_json::to_value(vellum_remote_agent::official_account::list(store.paths())?)
                .map_err(|error| error.to_string())
        }
        AgentRequest::ProxyOfficialAccountSelect {
            operation_id,
            account_id_hash,
        } => run_mutation(
            store,
            &operation_id,
            "proxy.officialAccountSelect",
            json!({"accountIdHash": account_id_hash}),
            || {
                serde_json::to_value(vellum_remote_agent::official_account::select(
                    store.paths(),
                    &account_id_hash,
                )?)
                .map_err(|error| error.to_string())
            },
        ),
        AgentRequest::ProxyOfficialAccountClearSelection { operation_id } => run_mutation(
            store,
            &operation_id,
            "proxy.officialAccountClearSelection",
            json!({}),
            || {
                serde_json::to_value(vellum_remote_agent::official_account::clear_selection(
                    store.paths(),
                )?)
                .map_err(|error| error.to_string())
            },
        ),
        AgentRequest::ProxyOfficialAccountRemove {
            operation_id,
            account_id_hash,
        } => run_mutation(
            store,
            &operation_id,
            "proxy.officialAccountRemove",
            json!({"accountIdHash": account_id_hash}),
            || {
                serde_json::to_value(vellum_remote_agent::official_account::remove(
                    store.paths(),
                    &account_id_hash,
                )?)
                .map_err(|error| error.to_string())
            },
        ),
        AgentRequest::ServicesReconcile { operation_id } => run_mutation(
            store,
            &operation_id,
            "services.reconcile",
            json!({}),
            || {
                let codex_home = resolve_codex_home()?;
                serde_json::to_value(
                    vellum_remote_agent::native_codex::reconcile_durable_service(
                        &codex_home,
                        store.paths(),
                    )?,
                )
                .map_err(|error| error.to_string())
            },
        ),
        AgentRequest::CodexRestartNative { operation_id } => run_mutation(
            store,
            &operation_id,
            "codex.restartNative",
            json!({}),
            || {
                let runtime = vellum_remote_agent::native_codex::restart_native(store.paths())?;
                profiles
                    .activate_after_restart(vellum_remote_agent::native_codex::NATIVE_PROFILE_ID)?;
                serde_json::to_value(runtime).map_err(|error| error.to_string())
            },
        ),
        AgentRequest::CodexStopAppOwned { operation_id } => run_mutation(
            store,
            &operation_id,
            "codex.stopAppOwned",
            json!({}),
            || {
                serde_json::to_value(vellum_remote_agent::native_codex::stop_app_owned_native()?)
                    .map_err(|error| error.to_string())
            },
        ),
        AgentRequest::CodexRestoreNative { operation_id } => run_mutation(
            store,
            &operation_id,
            "codex.restoreNative",
            json!({}),
            || {
                let codex_home = resolve_codex_home()?;
                let profile = vellum_remote_agent::native_codex::restore_native(profiles)?;
                let account =
                    vellum_remote_agent::native_account::restore(store.paths(), &codex_home)?;
                let mut result =
                    serde_json::to_value(profile).map_err(|error| error.to_string())?;
                result
                    .as_object_mut()
                    .ok_or_else(|| "invalid native restore result".to_string())?
                    .insert("accountRestore".into(), account);
                Ok(result)
            },
        ),
        AgentRequest::CodexStatus { profile_id } => {
            Ok(serde_json::to_value(profiles.inspect(&profile_id)?).map_err(|e| e.to_string())?)
        }
        AgentRequest::CodexPlanInjection { profile_id } => Ok(serde_json::to_value(
            profiles.plan_injection(&profile_id, &manager.status()?)?,
        )
        .map_err(|e| e.to_string())?),
        AgentRequest::CodexInject {
            operation_id,
            profile_id,
            catalog_json,
        } => run_mutation(
            store,
            &operation_id,
            "codex.inject",
            json!({"profileId": profile_id, "catalogSha256": sha256(&catalog_json)}),
            || {
                serde_json::to_value(profiles.inject(
                    &profile_id,
                    &manager.status()?,
                    &catalog_json,
                )?)
                .map_err(|e| e.to_string())
            },
        ),
        AgentRequest::CodexStartManaged {
            operation_id,
            profile_id,
            broker_port,
        } => run_mutation(
            store,
            &operation_id,
            "codex.startManaged",
            json!({"profileId": profile_id, "brokerPort": broker_port}),
            || {
                serde_json::to_value(profiles.start_managed(&profile_id, broker_port)?)
                    .map_err(|e| e.to_string())
            },
        ),
        AgentRequest::CodexStopManaged {
            operation_id,
            profile_id,
        } => run_mutation(
            store,
            &operation_id,
            "codex.stopManaged",
            json!({"profileId": profile_id}),
            || serde_json::to_value(profiles.stop_managed(&profile_id)?).map_err(|e| e.to_string()),
        ),
        AgentRequest::CodexRestore {
            operation_id,
            profile_id,
        } => run_mutation(
            store,
            &operation_id,
            "codex.restore",
            json!({"profileId": profile_id}),
            || serde_json::to_value(profiles.restore(&profile_id)?).map_err(|e| e.to_string()),
        ),
        AgentRequest::LeaseStatus { profile_id } => profiles.lease_status(&profile_id),
        AgentRequest::RuntimeStatus { profile_id } => {
            serde_json::to_value(profiles.runtime_status(&profile_id, None)?)
                .map_err(|e| e.to_string())
        }
        AgentRequest::DoctorRun => {
            let status = host_status(store, manager, profiles)?;
            Ok(json!({
                "status": status,
                "notes": doctor_notes(&status),
            }))
        }
        AgentRequest::RepairRun { operation_id } => {
            operation_to_value(manager.repair(&operation_id)?)
        }
        AgentRequest::SupportBundle => {
            let status = serde_json::to_value(host_status(store, manager, profiles)?)
                .map_err(|error| error.to_string())?;
            serde_json::to_value(vellum_remote_agent::support::create_support_bundle(
                store.paths(),
                status,
            )?)
            .map_err(|error| error.to_string())
        }
        AgentRequest::AgentUpdate {
            operation_id,
            staged_path,
            expected_sha256,
        } => run_mutation(
            store,
            &operation_id,
            "agent.update",
            json!({"stagedPath": staged_path, "expectedSha256": expected_sha256}),
            || {
                serde_json::to_value(vellum_remote_agent::update::install_current(
                    std::path::Path::new(&staged_path),
                    &expected_sha256,
                )?)
                .map_err(|error| error.to_string())
            },
        ),
        AgentRequest::AgentRollback { operation_id } => {
            run_mutation(store, &operation_id, "agent.rollback", json!({}), || {
                serde_json::to_value(vellum_remote_agent::update::rollback_current()?)
                    .map_err(|error| error.to_string())
            })
        }
    }
}

fn sha256(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

fn run_mutation(
    store: &AgentStateStore,
    operation_id: &str,
    method: &str,
    fingerprint: Value,
    work: impl FnOnce() -> Result<Value, String>,
) -> Result<Value, String> {
    let journal = store.operations();
    match journal.begin_or_replay(operation_id, method, &fingerprint)? {
        BeginOutcome::Replay { result } => Ok(result),
        BeginOutcome::Fresh { mut record } | BeginOutcome::Resume { mut record } => {
            journal.mark_executing(&mut record)?;
            match work() {
                Ok(result) => {
                    journal.mark_completed(&mut record, result.clone())?;
                    Ok(result)
                }
                Err(error) => {
                    let _ = journal.mark_failed(&mut record, error.clone());
                    Err(error)
                }
            }
        }
    }
}

fn operation_to_value(result: OperationResult) -> Result<Value, String> {
    serde_json::to_value(result).map_err(|e| e.to_string())
}

fn host_status(
    store: &AgentStateStore,
    manager: &ProxyManager,
    profiles: &ProfileManager,
) -> Result<HostStatus, String> {
    let native = vellum_remote_agent::native_codex::discover_native().ok();
    host_status_with_native(store, manager, profiles, native.as_ref())
}

/// Same status, reusing a `discover_native()` result the caller already has
/// (see `host_manager_snapshot`) instead of the three separate calls this
/// used to make (once for `native_codex`, once more for `chatgpt`, plus
/// whatever the caller already did before calling this at all).
fn host_status_with_native(
    store: &AgentStateStore,
    manager: &ProxyManager,
    profiles: &ProfileManager,
    native: Option<&vellum_remote_agent::native_codex::NativeCodexRuntimeStatus>,
) -> Result<HostStatus, String> {
    let state = store.load()?;
    let native_codex = native.and_then(|status| serde_json::to_value(status.clone()).ok());
    let mut managed_profiles = profiles.aggregate_statuses()?;
    reconcile_native_profile_runtime(&mut managed_profiles, native_codex.as_ref());
    Ok(HostStatus {
        host_id: state.host_id,
        agent_version: AGENT_VERSION.to_string(),
        agent_protocol: AGENT_PROTOCOL_VERSION,
        capabilities: probe_host(),
        proxy: manager.status()?,
        install: state.install,
        configuration: serde_json::to_value(vellum_remote_agent::configuration::public_status(
            store.paths(),
        )?)
        .map_err(|error| error.to_string())?,
        managed_profiles,
        native_codex,
        grok: vellum_remote_agent::grok::status(store.paths())
            .ok()
            .and_then(|status| serde_json::to_value(status).ok()),
        chatgpt: native
            .and_then(|native| {
                vellum_remote_agent::native_account::status(
                    store.paths(),
                    Path::new(&native.codex_home),
                    None,
                )
                .ok()
            })
            .and_then(|status| serde_json::to_value(status).ok()),
    })
}

/// M30: single-call versioned host inventory for the Remote Manager UI.
fn host_inventory(
    store: &AgentStateStore,
    manager: &ProxyManager,
) -> Result<HostInventoryV2, String> {
    let native = vellum_remote_agent::native_codex::discover_native().ok();
    host_inventory_with_native(store, manager, native.as_ref())
}

/// Same inventory, reusing a `discover_native()` result the caller already
/// has. See [`host_status_with_native`].
fn host_inventory_with_native(
    store: &AgentStateStore,
    manager: &ProxyManager,
    native: Option<&vellum_remote_agent::native_codex::NativeCodexRuntimeStatus>,
) -> Result<HostInventoryV2, String> {
    let state = store.load()?;
    let system = vellum_remote_agent::host_probe::probe_system_inventory();
    let platform = vellum_remote_agent::platform::RemotePlatform::from_os_arch(
        &system.os,
        &system.arch,
    )
    .ok();
    let managed_home = dirs::home_dir().map(|home| {
        vellum_remote_agent::platform::managed_codex_home(&home, &system.os)
            .display()
            .to_string()
    });
    let mut inventory = HostInventoryV2 {
        host_id: state.host_id,
        agent_version: AGENT_VERSION.to_string(),
        agent_protocol: AGENT_PROTOCOL_VERSION,
        system,
        docker: vellum_remote_agent::host_probe::probe_docker_inventory(),
        codex: vellum_remote_agent::host_probe::probe_codex_inventory_with_native(native),
        proxy: manager.status()?,
        blockers: Vec::new(),
        available_actions: Vec::new(),
        platform: platform.map(|item| item.artifact_key().to_string()),
        proxy_backend: platform.map(|item| item.proxy_backend_name().to_string()),
        service_manager: platform.map(|item| item.service_manager().to_string()),
        persistence_scope: platform.map(|item| item.persistence_scope().to_string()),
        managed_codex_home: managed_home,
    };
    inventory.blockers = host_blockers(&inventory);
    inventory.available_actions = host_actions(&inventory);
    Ok(inventory)
}

/// M35: `host.managerSnapshot` — one SSH round trip instead of the four
/// (`host.status`, `host.inventoryV2`, `codex.accountStatus`,
/// `codex.sessionStatus`) a Remote Manager refresh used to open in sequence.
/// `discover_native()` itself only runs once and is threaded through every
/// sub-probe that needs it.
fn host_manager_snapshot(
    store: &AgentStateStore,
    manager: &ProxyManager,
    profiles: &ProfileManager,
    expected_account_id: Option<&str>,
    thread_id: Option<&str>,
) -> Result<HostManagerSnapshot, String> {
    let total_start = std::time::Instant::now();

    let t = std::time::Instant::now();
    let native = vellum_remote_agent::native_codex::discover_native().ok();
    let native_discover_ms = t.elapsed().as_millis() as u64;

    let t = std::time::Instant::now();
    let status = host_status_with_native(store, manager, profiles, native.as_ref())?;
    let status_ms = t.elapsed().as_millis() as u64;

    let t = std::time::Instant::now();
    let inventory = host_inventory_with_native(store, manager, native.as_ref())?;
    let inventory_ms = t.elapsed().as_millis() as u64;

    let codex_home = native
        .as_ref()
        .map(|status| PathBuf::from(&status.codex_home));

    let t = std::time::Instant::now();
    let account = expected_account_id.and_then(|expected| {
        let home = codex_home.as_deref()?;
        vellum_remote_agent::native_account::status(store.paths(), home, Some(expected))
            .ok()
            .and_then(|status| serde_json::to_value(status).ok())
    });
    let account_ms = t.elapsed().as_millis() as u64;

    let t = std::time::Instant::now();
    let session = native
        .as_ref()
        .filter(|status| status.daemon_running)
        .and_then(|_| {
            let home = codex_home.as_deref()?;
            vellum_remote_agent::native_session::query_session_status(home, thread_id)
                .ok()
                .and_then(|status| serde_json::to_value(status).ok())
        });
    let session_ms = t.elapsed().as_millis() as u64;

    let state = store.load()?;
    Ok(HostManagerSnapshot {
        host_id: state.host_id,
        agent_version: AGENT_VERSION.to_string(),
        agent_protocol: AGENT_PROTOCOL_VERSION,
        status,
        inventory,
        account,
        session,
        probe_timings_ms: vellum_remote_agent::protocol::HostManagerProbeTimings {
            total_ms: total_start.elapsed().as_millis() as u64,
            native_discover_ms,
            status_ms,
            inventory_ms,
            account_ms,
            session_ms,
        },
    })
}

/// Host-level blockers derived from the live inventory. Deployment blockers
/// (credential, probe qualification, drift) stay on the desktop side; this
/// list is only what the agent can observe about the host itself.
fn host_blockers(inventory: &HostInventoryV2) -> Vec<HostBlocker> {
    let mut blockers = Vec::new();
    let platform = vellum_remote_agent::platform::RemotePlatform::from_os_arch(
        &inventory.system.os,
        &inventory.system.arch,
    );
    if let Err(unsupported) = &platform {
        blockers.push(HostBlocker::new(
            unsupported.code,
            unsupported.message.clone(),
            false,
        ));
    }
    let docker_is_blocker = platform
        .as_ref()
        .map(|item| item.docker_is_blocker())
        .unwrap_or(true);
    if docker_is_blocker && !inventory.docker.available {
        blockers.push(HostBlocker::new(
            "dockerUnavailable",
            "Docker daemon is not available on this host",
            true,
        ));
    }
    if inventory.codex.source == "missing" {
        blockers.push(HostBlocker::new(
            "codexBinaryMissing",
            "Codex binary was not detected on this host",
            true,
        ));
    }
    if inventory.codex.standalone_installed && !inventory.codex.app_cli_discoverable {
        blockers.push(HostBlocker::new(
            "codexAppCliUnavailable",
            "Codex is installed, but its SSH login-shell launcher is missing or stale",
            true,
        ));
    }
    if inventory.proxy.present && inventory.proxy.running && !inventory.proxy.ready {
        blockers.push(HostBlocker::new(
            "proxyNotReady",
            "Proxy is running but has not reported ready",
            true,
        ));
    }
    blockers
}

/// Host-level actions the UI may offer, aligned with existing agent RPCs.
fn host_actions(inventory: &HostInventoryV2) -> Vec<String> {
    let mut actions = Vec::new();
    if inventory.codex.source == "missing" {
        actions.push("installCodex".into());
    }
    if inventory.codex.standalone_installed && !inventory.codex.app_cli_discoverable {
        actions.push("repair".into());
    }
    let docker_is_blocker = vellum_remote_agent::platform::RemotePlatform::from_os_arch(
        &inventory.system.os,
        &inventory.system.arch,
    )
    .map(|item| item.docker_is_blocker())
    .unwrap_or(true);
    if docker_is_blocker && !inventory.docker.available {
        actions.push("repair".into());
    }
    actions.push("exportSupportBundle".into());
    actions.push("updateComponents".into());
    actions
}

fn reconcile_native_profile_runtime(profiles: &mut [Value], native: Option<&Value>) {
    let ready = native.is_some_and(|status| {
        status
            .get("daemonRunning")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            && status
                .get("durable")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            && status
                .get("restartSafe")
                .and_then(Value::as_bool)
                .unwrap_or(false)
    });
    for profile in profiles.iter_mut().filter(|profile| {
        profile
            .pointer("/profile/profileId")
            .and_then(Value::as_str)
            == Some(vellum_remote_agent::native_codex::NATIVE_PROFILE_ID)
    }) {
        if let Some(runtime) = profile.get_mut("runtime").and_then(Value::as_object_mut) {
            runtime.insert("ready".into(), Value::Bool(ready));
            runtime.insert("appServerActive".into(), Value::Bool(ready));
            runtime.insert("brokerActive".into(), Value::Bool(false));
        }
    }
}

fn doctor_notes(status: &HostStatus) -> Vec<String> {
    let mut notes = Vec::new();
    let docker_is_blocker = vellum_remote_agent::platform::RemotePlatform::from_os_arch(
        &status.capabilities.os,
        &status.capabilities.arch,
    )
    .map(|item| item.docker_is_blocker())
    .unwrap_or(true);
    if docker_is_blocker && !status.capabilities.docker_available {
        notes.push("docker unavailable".into());
    }
    if status.proxy.present && status.proxy.running && !status.proxy.ready {
        notes.push("proxy running but not ready".into());
    }
    if let Some(error) = &status.proxy.last_error {
        notes.push(error.clone());
    }
    if status.capabilities.codex_binary.is_none() {
        notes.push("codex binary not detected (ok for proxy-only first slice)".into());
    }
    notes
}

fn resolve_codex_home() -> Result<PathBuf, String> {
    let status = vellum_remote_agent::native_codex::discover_native()?;
    Ok(PathBuf::from(status.codex_home))
}

fn print_ok(result: Value) -> Result<(), String> {
    let response = AgentResponse::Ok { result };
    println!(
        "{}",
        serde_json::to_string(&response).map_err(|error| error.to_string())?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_profile_runtime_uses_native_daemon_authority() {
        let mut profiles = vec![json!({
            "profile": {"profileId": "codex-app-native"},
            "runtime": {"ready": false, "appServerActive": false, "brokerActive": true}
        })];
        let native = json!({"daemonRunning": true, "durable": true, "restartSafe": true});
        reconcile_native_profile_runtime(&mut profiles, Some(&native));
        assert_eq!(profiles[0]["runtime"]["ready"], true);
        assert_eq!(profiles[0]["runtime"]["appServerActive"], true);
        assert_eq!(profiles[0]["runtime"]["brokerActive"], false);
    }

    fn inventory_fixture() -> HostInventoryV2 {
        HostInventoryV2 {
            host_id: "h-1".into(),
            agent_version: "0.1.0".into(),
            agent_protocol: 1,
            system: vellum_remote_agent::protocol::SystemInventory {
                os: "linux".into(),
                arch: "aarch64".into(),
                ..Default::default()
            },
            docker: vellum_remote_agent::protocol::DockerInventory {
                available: true,
                mode: "rootful".into(),
                ..Default::default()
            },
            codex: vellum_remote_agent::protocol::CodexInventory {
                source: "native".into(),
                standalone_installed: true,
                app_cli_discoverable: true,
                ..Default::default()
            },
            proxy: vellum_remote_agent::protocol::ProxyStatusView {
                present: true,
                running: true,
                ready: true,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn healthy_host_has_no_host_level_blockers() {
        let inventory = inventory_fixture();
        assert!(host_blockers(&inventory).is_empty());
        assert_eq!(
            host_actions(&inventory),
            vec![
                "exportSupportBundle".to_string(),
                "updateComponents".to_string()
            ]
        );
    }

    #[test]
    fn missing_codex_and_docker_produce_blockers_and_actions() {
        let mut inventory = inventory_fixture();
        inventory.codex.source = "missing".into();
        inventory.docker.available = false;
        inventory.docker.mode = "unavailable".into();

        let blockers = host_blockers(&inventory);
        let codes: Vec<&str> = blockers
            .iter()
            .map(|blocker| blocker.code.as_str())
            .collect();
        assert!(codes.contains(&"dockerUnavailable"));
        assert!(codes.contains(&"codexBinaryMissing"));
        assert!(blockers.iter().all(|blocker| blocker.repairable));

        let actions = host_actions(&inventory);
        assert!(actions.contains(&"installCodex".to_string()));
        assert!(actions.contains(&"repair".to_string()));
        assert!(actions.contains(&"exportSupportBundle".to_string()));
        assert!(actions.contains(&"updateComponents".to_string()));
    }

    #[test]
    fn proxy_running_but_not_ready_is_a_host_blocker() {
        let mut inventory = inventory_fixture();
        inventory.proxy.ready = false;
        let blockers = host_blockers(&inventory);
        assert!(blockers
            .iter()
            .any(|blocker| blocker.code == "proxyNotReady"));
    }

    #[test]
    fn stale_codex_app_launcher_is_repairable_and_not_reported_healthy() {
        let mut inventory = inventory_fixture();
        inventory.codex.app_cli_discoverable = false;
        let blockers = host_blockers(&inventory);
        assert!(blockers
            .iter()
            .any(|blocker| blocker.code == "codexAppCliUnavailable" && blocker.repairable));
        assert!(host_actions(&inventory).contains(&"repair".to_string()));
    }

    #[test]
    fn macos_without_docker_is_not_a_blocker() {
        let mut inventory = inventory_fixture();
        inventory.system.os = "macos".into();
        inventory.system.arch = "aarch64".into();
        inventory.docker.available = false;
        inventory.docker.mode = "unavailable".into();
        inventory.platform = Some("darwin-arm64".into());
        inventory.proxy_backend = Some("native".into());
        let blockers = host_blockers(&inventory);
        let codes: Vec<&str> = blockers
            .iter()
            .map(|blocker| blocker.code.as_str())
            .collect();
        assert!(!codes.contains(&"dockerUnavailable"));
        assert!(!host_actions(&inventory).contains(&"repair".to_string()) || inventory.codex.standalone_installed);
        let intel = {
            let mut item = inventory.clone();
            item.system.arch = "x86_64".into();
            item
        };
        assert!(host_blockers(&intel)
            .iter()
            .any(|blocker| blocker.code == "intelMacUnsupported"));
    }
}
