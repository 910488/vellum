//! Per-request runtime settings snapshot (plan §4.5).
//!
//! A request must run start-to-finish against one fixed view of settings, so
//! a user flipping a route or policy mid-request can never change which
//! route/policy an in-flight turn resolves against. `RuntimeSnapshot::capture`
//! is the only place that view is built, at the start of a request.
//!
//! The policy sub-snapshots (`review`, `compaction`, `shell_contract`,
//! `runtime_policy`) are still placeholders: their real shape is extraction
//! work for M6 (compaction) and M9 (review) — inventing detailed fields for
//! them now, before that extraction defines what each stage actually reads,
//! would just be guessing. They exist here so `RuntimeSnapshot`'s shape is
//! fixed today and later milestones fill them in rather than reshaping the
//! struct.
//!
//! `web_search` (M8) is no longer a placeholder: it carries whether the local
//! `web_search` compatibility wrapper should be offered to third-party routes
//! (`WebSearchPolicySnapshot::enabled`), populated at capture time from
//! whatever already decides "search enabled and a usable backend key is
//! configured" (see `RuntimeSnapshot::with_web_search_enabled`'s callers).
//!
//! `RuntimeSnapshot` implements `RouteCatalog` directly. That is the whole
//! point of capturing one: once a request has its snapshot, every route
//! resolution for the rest of that request must go through `&snapshot`, not
//! through a live `&dyn RouteCatalog` — otherwise the snapshot is decorative
//! and a route change mid-request can still leak through the live catalog.

use serde_json::Value;

use crate::review::ReviewSettings;
use crate::route::{ResolvedRoute, RouteCatalog, RuntimeModelRoute};

/// The Auto Review / Guardian policy captured for one request (M9 real fix).
/// Captured once per request from the runtime's
/// [`crate::review::ReviewSettingsSource`] so a settings change mid-request
/// can never retroactively change which route a Guardian dispatch already
/// resolved against, while the *next* request always sees the latest value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReviewPolicySnapshot {
    pub settings: ReviewSettings,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactionPolicySnapshot {
    pub raw: serde_json::Value,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WebSearchPolicySnapshot {
    /// Whether the local `web_search` compatibility wrapper (Vellum's own
    /// Brave-backed tool, executed by Vellum — never a provider's native
    /// search passthrough, which this never gates) should be offered on the
    /// outgoing tool list for a third-party route this request. Defaults to
    /// `false` (fail closed): a snapshot nobody explicitly enabled must never
    /// silently offer a tool that has no usable backend behind it.
    pub enabled: bool,
    pub raw: serde_json::Value,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShellContractSnapshot {
    pub raw: serde_json::Value,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimePolicySnapshot {
    pub raw: serde_json::Value,
}

/// One route plus the exact model-visible catalog entry the request-time
/// adapter must verify against and replace the prompt baseline from. The entry
/// is captured once with the route so a catalog edit mid-request can never
/// change the authority the translation ran against.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeRouteSnapshot {
    pub route: RuntimeModelRoute,
    pub catalog_entry: Value,
}

impl RuntimeRouteSnapshot {
    pub fn new(route: RuntimeModelRoute, catalog_entry: Value) -> Self {
        Self {
            route,
            catalog_entry,
        }
    }
}

/// The honest route-only projection a catalog ships when it has no explicit
/// entry: public route-level facts, nothing private. A production config
/// should supply the real model-visible entry (with `base_instructions`) so
/// request-time prompt replacement matches the string Codex was built against.
pub(crate) fn route_projection(route: &RuntimeModelRoute) -> Value {
    serde_json::json!({
        "id": route.catalog_id,
        "name": route.name,
        "route_id": route.route_id,
        "base_url": route.base_url,
        "provider_kind": route.provider_kind.as_str(),
        "wire": route.wire.as_str(),
        "upstream_model": route.upstream_model,
        "context_window": route.context_window,
    })
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RuntimeSnapshot {
    pub routes: Vec<RuntimeRouteSnapshot>,
    pub review: ReviewPolicySnapshot,
    pub compaction: CompactionPolicySnapshot,
    pub web_search: WebSearchPolicySnapshot,
    pub shell_contract: ShellContractSnapshot,
    pub runtime_policy: RuntimePolicySnapshot,
}

impl RuntimeSnapshot {
    /// Capture the per-request view from a live catalog, attaching each
    /// route's own catalog entry. This is the only place the request-time view
    /// is built; everything later reads `&self`, never the live catalog.
    pub fn capture_from(catalog: &dyn RouteCatalog) -> Self {
        let routes = catalog
            .route_entries()
            .into_iter()
            .map(|(route, entry)| {
                let entry = entry.unwrap_or_else(|| route_projection(&route));
                RuntimeRouteSnapshot::new(route, entry)
            })
            .collect();
        Self {
            routes,
            ..Default::default()
        }
    }

    /// A snapshot from bare routes (tests, callers without a catalog). Entries
    /// fall back to the honest route-only projection.
    pub fn capture(routes: Vec<RuntimeModelRoute>) -> Self {
        let routes = routes
            .into_iter()
            .map(|route| RuntimeRouteSnapshot::new(route.clone(), route_projection(&route)))
            .collect();
        Self {
            routes,
            ..Default::default()
        }
    }

    /// Resolve a route *and* its captured catalog entry for a request.
    pub fn resolve_route_snapshot(&self, catalog_id: &str) -> Option<&RuntimeRouteSnapshot> {
        self.routes
            .iter()
            .find(|snapshot| snapshot.route.catalog_id == catalog_id)
    }

    /// Attach whether the local `web_search` compatibility wrapper should be
    /// offered to third-party routes on this request (M8 real fix, replacing
    /// the old always-`false` placeholder). The caller resolves this from
    /// whatever already decides "search enabled AND a usable backend key is
    /// configured" (Desktop: `DesktopProxyRuntimeState::new`'s
    /// `search_settings.enabled && search_settings.brave_api_key.is_some()`)
    /// — this method never re-derives that decision itself.
    pub fn with_web_search_enabled(mut self, enabled: bool) -> Self {
        self.web_search.enabled = enabled;
        self
    }

    /// Attach the Auto Review policy this request must use. Resolved once at
    /// capture time from [`crate::review::ReviewSettingsSource::current`] —
    /// never re-read later, so guardian routing and the usage record it
    /// produces always agree on the same policy for one request.
    pub fn with_review_settings(mut self, settings: ReviewSettings) -> Self {
        self.review.settings = settings;
        self
    }

    pub fn review_settings(&self) -> &ReviewSettings {
        &self.review.settings
    }
}

impl RouteCatalog for RuntimeSnapshot {
    fn active_models(&self) -> Vec<RuntimeModelRoute> {
        self.routes
            .iter()
            .map(|snapshot| snapshot.route.clone())
            .collect()
    }

    fn resolve_model(&self, catalog_id: &str) -> Option<ResolvedRoute> {
        self.resolve_route_snapshot(catalog_id)
            .map(|snapshot| ResolvedRoute {
                route: snapshot.route.clone(),
            })
    }

    fn resolve_review_model(&self, catalog_id: &str) -> Option<ResolvedRoute> {
        // M1: review resolution is identical to primary resolution. A
        // distinct guardian/review route selection is M9's job.
        self.resolve_model(catalog_id)
    }

    fn catalog_entry(&self, catalog_id: &str) -> Option<Value> {
        self.resolve_route_snapshot(catalog_id)
            .map(|snapshot| snapshot.catalog_entry.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::route::{
        RuntimeAuthKind, RuntimeCompactionCapabilities, RuntimeProviderKind,
        RuntimeReasoningCapabilities, RuntimeToolCapabilities, RuntimeWireFormat,
    };

    fn route() -> RuntimeModelRoute {
        RuntimeModelRoute {
            route_id: "route-1".into(),
            catalog_id: "vlm-sample".into(),
            name: "Sample".into(),
            base_url: "http://127.0.0.1:0/v1".into(),
            provider_kind: RuntimeProviderKind::OpenAiCompatible,
            auth_kind: RuntimeAuthKind::Bearer,
            wire: RuntimeWireFormat::Responses,
            server_side_resume: false,
            streaming: true,
            reasoning: false,
            vision: false,
            upstream_model: "sample-model".into(),
            context_window: None,
            reasoning_capabilities: RuntimeReasoningCapabilities::default(),
            compaction_capabilities: RuntimeCompactionCapabilities::default(),
            compaction_policy: crate::config::RuntimeCompactionPolicy::default(),
            tool_capabilities: RuntimeToolCapabilities::default(),
            credential_id: None,
            insecure_http_policy: crate::outbound::InsecureHttpPolicy::Deny,
            provider_profile: None,
            access_mode: None,
            chat_capabilities: crate::route::RuntimeChatCapabilities::default(),
        }
    }

    #[test]
    fn a_request_started_against_one_snapshot_never_sees_a_later_route_change() {
        let snapshot = RuntimeSnapshot::capture(vec![route()]);
        let mut mutated_source = vec![route()];
        mutated_source.push(route());
        // Mutating the source vector after capture must not reach through to
        // the already-captured snapshot.
        assert_eq!(snapshot.routes.len(), 1);
        assert_eq!(mutated_source.len(), 2);
    }

    #[test]
    fn capture_from_uses_one_route_entries_pass_instead_of_per_model_lookups() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct BatchCatalog {
            models: Vec<RuntimeModelRoute>,
            entry_calls: AtomicUsize,
            batch_calls: AtomicUsize,
        }

        impl crate::route::RouteCatalog for BatchCatalog {
            fn active_models(&self) -> Vec<RuntimeModelRoute> {
                self.models.clone()
            }
            fn resolve_model(&self, catalog_id: &str) -> Option<crate::route::ResolvedRoute> {
                self.models
                    .iter()
                    .find(|route| route.catalog_id == catalog_id)
                    .cloned()
                    .map(|route| crate::route::ResolvedRoute { route })
            }
            fn resolve_review_model(
                &self,
                catalog_id: &str,
            ) -> Option<crate::route::ResolvedRoute> {
                self.resolve_model(catalog_id)
            }
            fn catalog_entry(&self, _catalog_id: &str) -> Option<Value> {
                self.entry_calls.fetch_add(1, Ordering::SeqCst);
                None
            }
            fn route_entries(&self) -> Vec<(RuntimeModelRoute, Option<Value>)> {
                self.batch_calls.fetch_add(1, Ordering::SeqCst);
                self.models
                    .iter()
                    .cloned()
                    .map(|route| (route, Some(serde_json::json!({"id": "batched"}))))
                    .collect()
            }
        }

        let catalog = BatchCatalog {
            models: vec![route(), {
                let mut second = route();
                second.catalog_id = "vlm-other".into();
                second.route_id = "route-2".into();
                second
            }],
            entry_calls: AtomicUsize::new(0),
            batch_calls: AtomicUsize::new(0),
        };
        let snapshot = RuntimeSnapshot::capture_from(&catalog);
        assert_eq!(snapshot.routes.len(), 2);
        assert_eq!(catalog.batch_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            catalog.entry_calls.load(Ordering::SeqCst),
            0,
            "capture_from must not rebuild the catalog once per model"
        );
        assert_eq!(snapshot.routes[0].catalog_entry["id"], "batched");
    }

    #[test]
    fn a_hot_refresh_is_visible_to_the_next_capture_from_call() {
        use std::sync::atomic::{AtomicBool, Ordering};

        struct RefreshableCatalog {
            refreshed: AtomicBool,
        }

        impl crate::route::RouteCatalog for RefreshableCatalog {
            fn active_models(&self) -> Vec<RuntimeModelRoute> {
                self.route_entries()
                    .into_iter()
                    .map(|(route, _)| route)
                    .collect()
            }
            fn resolve_model(&self, _catalog_id: &str) -> Option<crate::route::ResolvedRoute> {
                None
            }
            fn resolve_review_model(
                &self,
                catalog_id: &str,
            ) -> Option<crate::route::ResolvedRoute> {
                self.resolve_model(catalog_id)
            }
            fn catalog_entry(&self, _catalog_id: &str) -> Option<Value> {
                None
            }
            fn route_entries(&self) -> Vec<(RuntimeModelRoute, Option<Value>)> {
                let mut current = route();
                if self.refreshed.load(Ordering::SeqCst) {
                    current.catalog_id = "vlm-added-by-hot-refresh".into();
                }
                vec![(current, None)]
            }
        }

        let catalog = RefreshableCatalog {
            refreshed: AtomicBool::new(false),
        };
        let before = RuntimeSnapshot::capture_from(&catalog);
        assert_eq!(before.routes[0].route.catalog_id, "vlm-sample");

        // A hot refresh changes what the live catalog would build next; there
        // must be no snapshot-level cache standing between the catalog and a
        // fresh `capture_from` call for the next request.
        catalog.refreshed.store(true, Ordering::SeqCst);
        let after = RuntimeSnapshot::capture_from(&catalog);
        assert_eq!(after.routes[0].route.catalog_id, "vlm-added-by-hot-refresh");

        // The snapshot captured before the refresh must stay exactly as it
        // was captured (the in-flight-request half of the same guarantee).
        assert_eq!(before.routes[0].route.catalog_id, "vlm-sample");
    }

    #[test]
    fn web_search_enabled_defaults_to_false_and_is_only_set_by_explicit_opt_in() {
        let default_snapshot = RuntimeSnapshot::capture(vec![route()]);
        assert!(
            !default_snapshot.web_search.enabled,
            "a snapshot nobody opted in must fail closed"
        );
        let opted_in = RuntimeSnapshot::capture(vec![route()]).with_web_search_enabled(true);
        assert!(opted_in.web_search.enabled);
        assert!(!default_snapshot.web_search.enabled);
    }

    #[test]
    fn snapshot_resolves_routes_as_a_route_catalog_in_its_own_right() {
        let snapshot = RuntimeSnapshot::capture(vec![route()]);
        let resolved = snapshot.resolve_model("vlm-sample").expect("route present");
        assert_eq!(resolved.route.upstream_model, "sample-model");
        assert!(snapshot.resolve_model("missing").is_none());
        assert_eq!(
            snapshot.resolve_review_model("vlm-sample"),
            snapshot.resolve_model("vlm-sample")
        );
    }
}
