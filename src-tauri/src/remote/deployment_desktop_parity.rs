//! Does a remote host reconstruct the routes Desktop is running?
//!
//! A remote deployment does not ship `RuntimeModelRoute`. `plan()` projects
//! each Desktop route into a `RuntimeRouteConfig`, serialises that to TOML,
//! and the remote proxy rebuilds a `RuntimeModelRoute` from it with
//! `RuntimeRouteConfig::to_route()`. Three hand-written translations sit
//! between the two ends, and every one of them is a place a field can be
//! dropped without anything failing to compile:
//!
//! 1. `runtime_route` -> `RuntimeRouteConfig` in `plan()`, written field by
//!    field, so a field with no `RuntimeRouteConfig` counterpart simply is
//!    not copied;
//! 2. the TOML round trip, where a `#[serde(default)]` turns a missing key
//!    into a plausible-looking value rather than an error;
//! 3. `to_route()`, which re-derives some fields from `base_url` and
//!    `upstream_model` instead of reading what Desktop resolved.
//!
//! So the parity claim in `docs/remote-acceptance.md` (7.6) -- that remote
//! and local share one runtime configuration surface -- cannot be checked by
//! reading the types. This module checks it by value: it builds a route
//! matrix, asks Desktop what it would run, asks the plan what the host will
//! run, and compares the two as JSON so a field added later is compared
//! without anyone remembering to add it here.
//!
//! Divergences a remote host is *supposed* to have live in
//! `INTENDED_DIVERGENCES`, each with the reason. Anything else is a bug on
//! one side or the other.

use super::*;
use crate::model::{
    AccessMode, AuthKind, CreateRouteInput, InsecureHttpPolicy, ModelCapability, ProviderKind,
    WireFormat,
};
use serde_json::{Map, Value};

/// Fields whose remote value is deliberately not the local one.
///
/// Kept as data rather than as scattered `assert_ne`s so the list reads as
/// the answer to "what does a remote host do differently", and so `compare`
/// can prove each named field still exists -- a rename would otherwise turn
/// an exemption into a hole that excludes nothing.
///
/// `compaction_policy` used to be on this list. It came off when the engine
/// stopped being part of the policy: the two sides still mean different things
/// -- Desktop leaves compaction to the Codex runtime executing the thread,
/// while a remote host has no such runtime and is itself the compactor -- but
/// the threshold and reserves they publish now coincide for every route in the
/// matrix. An exemption that excludes a field which would compare equal is not
/// documenting a divergence, it is suppressing a guard.
const INTENDED_DIVERGENCES: &[(&str, &str)] = &[(
    "credential_id",
    "Official routes point at SELECTED_OFFICIAL_CREDENTIAL_ID rather than the \
     route id, so switching control account changes one private pointer on the \
     host instead of requiring a config rewrite and a daemon restart.",
)];

fn qualified(model: &str) -> ModelCapability {
    ModelCapability {
        model: model.into(),
        tool_calling: Some(true),
        probe_version: Some(crate::probe::HARNESS_PROBE_VERSION),
        ..Default::default()
    }
}

/// A host record `plan()` will resolve. Loopback and port 1: this module
/// never opens a connection, and `plan()` tolerates an unreachable agent by
/// reporting `agentUnavailable` rather than failing.
fn register_host(state: &AppState, host_id: &str) {
    state
        .remote()
        .import_discovered_host(&crate::remote::discovery::RemoteHostCandidate {
            codex_host_id: None,
            vellum_host_id: host_id.into(),
            display_name: host_id.into(),
            ssh_alias: "127.0.0.1".into(),
            hostname: Some("127.0.0.1".into()),
            user: None,
            port: Some(1),
            source: "openSsh".into(),
            validated: false,
            validation_error: None,
        })
        .expect("register the parity host");
}

/// The route shapes that exercise the translation, one per thing that has to
/// survive it. Deliberately not "every provider": a shape earns a place here
/// only if some field of its `RuntimeModelRoute` is resolved from state the
/// remote host does not have.
fn build_route_matrix(state: &AppState) {
    // OpenCode Zen with an access mode the URL does not imply. `to_route()`
    // infers access mode from the profile and the model name; Desktop reads
    // what the probe actually persisted. `zen-paid-model` is not in
    // CONFIRMED_FREE_ZEN_MODELS, so inference says `credentialed` while the
    // stored capability says otherwise -- which is the point of the case.
    let mut zen = qualified("zen-paid-model");
    zen.access_mode = Some(AccessMode::AnonymousFree);
    state.create_route(
        CreateRouteInput {
            name: "zen".into(),
            base_url: "https://opencode.ai/zen/v1".into(),
            model: "zen-paid-model".into(),
            wire: WireFormat::Responses,
            streaming: true,
            reasoning: true,
            server_side_resume: false,
            provider_kind: Some(ProviderKind::OpenAiCompatible),
            api_key: None,
            models: Some(vec!["zen-paid-model".into()]),
            selected_models: Some(vec!["zen-paid-model".into()]),
            context_window: Some(200_000),
            model_capabilities: vec![zen],
            catalog_scope: None,
        },
        ProviderKind::OpenAiCompatible,
        AuthKind::None,
    );

    // A plaintext-HTTP LAN endpoint with the exemption its owner granted it.
    // This is the shape the acceptance doc's Ollama-compatible lane runs on,
    // and the exemption is a per-route setting with its own Tauri command.
    state.create_route(
        CreateRouteInput {
            name: "lan".into(),
            base_url: "http://192.168.7.11:11434/v1".into(),
            model: "lan-model".into(),
            wire: WireFormat::Chat,
            streaming: true,
            reasoning: false,
            server_side_resume: false,
            provider_kind: Some(ProviderKind::OpenAiCompatible),
            api_key: None,
            models: Some(vec!["lan-model".into()]),
            selected_models: Some(vec!["lan-model".into()]),
            context_window: Some(131_072),
            model_capabilities: vec![qualified("lan-model")],
            catalog_scope: None,
        },
        ProviderKind::OpenAiCompatible,
        AuthKind::None,
    );
    let lan_id = state
        .routes()
        .into_iter()
        .find(|route| route.name == "lan")
        .expect("the lan route was created")
        .id;
    assert!(
        state.set_route_insecure_http_policy(&lan_id, InsecureHttpPolicy::AllowPrivateNetwork),
        "the lan route must accept the plaintext-HTTP exemption"
    );
    // `create_route` admits a new route against `Deny`, so a plaintext-HTTP
    // one is born disabled and the owner enables it after granting the
    // exemption. Both halves, in that order, or the route never reaches a
    // deployment and this case would quietly test nothing.
    assert!(
        state.set_route_enabled(&lan_id, true),
        "the lan route must be enabled once the exemption is granted"
    );

    // Bearer auth over a chat wire with reasoning: the ordinary third-party
    // shape, and the one whose credential_id must survive as the route id.
    state.create_route(
        CreateRouteInput {
            name: "bearer".into(),
            base_url: "https://bearer.invalid/v1".into(),
            model: "bearer-model".into(),
            wire: WireFormat::Chat,
            streaming: true,
            reasoning: true,
            server_side_resume: false,
            provider_kind: Some(ProviderKind::OpenAiCompatible),
            api_key: Some("parity-secret".into()),
            models: Some(vec!["bearer-model".into()]),
            selected_models: Some(vec!["bearer-model".into()]),
            context_window: Some(65_536),
            model_capabilities: vec![qualified("bearer-model")],
            catalog_scope: None,
        },
        ProviderKind::OpenAiCompatible,
        AuthKind::Bearer,
    );
    let bearer_id = state
        .routes()
        .into_iter()
        .find(|route| route.name == "bearer")
        .expect("the bearer route was created")
        .id;
    // `plan()` blocks on `credentialMissing` for a Bearer route with no stored
    // secret, and blocking would leave the comparison with nothing to compare.
    crate::credentials::save(&state.data_root(), &bearer_id, "parity-secret")
        .expect("store the bearer credential");
}

/// Desktop's answer: exactly what `start_proxy` hands the local runtime.
fn desktop_routes(state: &AppState) -> BTreeMap<String, vellum_proxy_runtime::RuntimeModelRoute> {
    let routes = state.routes();
    state
        .model_routes()
        .into_iter()
        .filter_map(|model| {
            let route = routes.iter().find(|route| route.id == model.route_id)?;
            Some((
                model.catalog_id.clone(),
                crate::proxy_runtime_bridge::runtime_route(state, route, &model),
            ))
        })
        .collect()
}

/// The host's answer: the plan's stored config, round-tripped through the
/// same TOML `proxy.configure` delivers, then rebuilt by the same
/// `to_route()` the remote proxy calls at startup.
fn remote_routes(
    state: &AppState,
    host_id: &str,
) -> BTreeMap<String, vellum_proxy_runtime::RuntimeModelRoute> {
    let planned = plan(
        state,
        host_id,
        RemoteModelSelection {
            catalog_ids: vec![],
            policy: RemotePolicyOverrides::default(),
        },
    )
    .expect("planning must succeed");
    let unexpected: Vec<&String> = planned
        .blocked_reasons
        .iter()
        .filter(|reason| {
            !reason.starts_with("agentUnavailable")
                && !reason.starts_with("officialAccountStatusUnavailable")
                && *reason != "desktopOfficialAccountMissing"
        })
        .collect();
    assert!(
        unexpected.is_empty(),
        "only an unreachable agent, unavailable account status, and a missing Desktop account may block \
         this plan: {unexpected:?}"
    );

    let stored = load_plan(&state.data_root(), &planned.plan_id).expect("load the stored plan");
    let config = vellum_proxy_runtime::ProxyRuntimeConfig::from_toml_str(&stored.config_toml)
        .expect("the stored remote config must parse as the runtime's own config");
    config
        .models
        .iter()
        .map(|route| (route.catalog_id.clone(), route.to_route()))
        .collect()
}

fn as_object(route: &vellum_proxy_runtime::RuntimeModelRoute) -> Map<String, Value> {
    match serde_json::to_value(route).expect("a runtime route serialises") {
        Value::Object(map) => map,
        other => panic!("a runtime route must serialise as an object, got {other}"),
    }
}

/// Compare one catalog entry's two answers, reporting *every* differing field
/// rather than stopping at the first: a translation that has drifted usually
/// drops more than one thing.
fn compare(
    catalog_id: &str,
    local: &vellum_proxy_runtime::RuntimeModelRoute,
    remote: &vellum_proxy_runtime::RuntimeModelRoute,
) -> Vec<String> {
    let local = as_object(local);
    let remote = as_object(remote);
    for (field, _) in INTENDED_DIVERGENCES {
        assert!(
            local.contains_key(*field) || remote.contains_key(*field),
            "{catalog_id}: `{field}` is declared an intended divergence but no \
             longer exists on either side -- the exemption now excludes \
             nothing and must be removed or renamed"
        );
    }
    let mut fields: Vec<&String> = local.keys().chain(remote.keys()).collect();
    fields.sort();
    fields.dedup();
    fields
        .into_iter()
        .filter(|field| {
            !INTENDED_DIVERGENCES
                .iter()
                .any(|(intended, _)| intended == field)
        })
        .filter_map(|field| {
            let (left, right) = (local.get(field), remote.get(field));
            (left != right).then(|| {
                format!(
                    "  {catalog_id}.{field}\n    desktop: {}\n    remote:  {}",
                    left.map(ToString::to_string)
                        .unwrap_or_else(|| "<absent>".into()),
                    right
                        .map(ToString::to_string)
                        .unwrap_or_else(|| "<absent>".into()),
                )
            })
        })
        .collect()
}

#[test]
fn a_remote_host_rebuilds_the_routes_desktop_would_have_run() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::with_data_dir(temp.path().to_path_buf());
    build_route_matrix(&state);
    register_host(&state, "host-parity");

    let desktop = desktop_routes(&state);
    let remote = remote_routes(&state, "host-parity");
    assert!(!remote.is_empty(), "the plan deployed no routes to compare");

    // A deployment may carry *fewer* entries than Desktop -- `plan()` drops
    // hidden Official models and the review-only route on purpose. It may
    // never carry one Desktop does not have: that would be a model the host
    // serves and Desktop cannot account for.
    let invented: Vec<&String> = remote
        .keys()
        .filter(|catalog_id| !desktop.contains_key(*catalog_id))
        .collect();
    assert!(
        invented.is_empty(),
        "the deployment carries catalog entries Desktop is not running: {invented:?}"
    );
    // Guard against a comparison that passes because the interesting cases
    // were filtered out somewhere upstream and never reached the host.
    for name in ["zen", "lan", "bearer"] {
        assert!(
            remote.values().any(|route| route.name == name),
            "the `{name}` case never reached the deployment, so comparing \
             what did would prove nothing about it. desktop has {:?}, the \
             deployment has {:?}",
            desktop
                .values()
                .map(|route| &route.name)
                .collect::<Vec<_>>(),
            remote.values().map(|route| &route.name).collect::<Vec<_>>()
        );
    }

    let mut differences = Vec::new();
    for (catalog_id, remote_route) in &remote {
        differences.extend(compare(catalog_id, &desktop[catalog_id], remote_route));
    }
    assert!(
        differences.is_empty(),
        "a remote host would not behave like Desktop for these fields:\n{}\n\n\
         Either carry the value through `RuntimeRouteConfig` so the host can \
         rebuild it, or add the field to INTENDED_DIVERGENCES with the reason \
         a remote host is meant to differ.",
        differences.join("\n")
    );
}

/// The exemptions themselves, asserted positively. Excluding a field from the
/// comparison says nothing about what it should be; this says it.
#[test]
fn the_intended_divergences_are_the_values_they_claim_to_be() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::with_data_dir(temp.path().to_path_buf());
    build_route_matrix(&state);
    register_host(&state, "host-intended");

    let routes = state.routes();
    let desktop = desktop_routes(&state);
    let remote = remote_routes(&state, "host-intended");

    for (catalog_id, remote_route) in &remote {
        let route = routes
            .iter()
            .find(|route| route.id == remote_route.route_id)
            .expect("every remote route came from a Desktop route");
        assert_eq!(
            remote_route.compaction_policy,
            remote_compaction_policy(route.provider_kind),
            "{catalog_id}: the remote host must carry the remote compaction \
             policy, not Desktop's"
        );
        // A non-Official route has no reason to diverge on credential_id, so
        // the exemption must not be quietly covering one that does.
        if route.provider_kind != ProviderKind::Official {
            assert_eq!(
                remote_route.credential_id, desktop[catalog_id].credential_id,
                "{catalog_id}: only Official routes may repoint credential_id"
            );
        }
    }
}
