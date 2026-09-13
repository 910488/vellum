//! Portable Enhanced Codex MVP modules.
//!
//! These modules are the source that should land in a pinned OpenAI Codex
//! fork at `codex-rs/core/src/enhanced/`. They must not depend on Vellum
//! proxy-runtime, Canonical compaction, Action Conversion, or any other
//! historical Vellum agent loop.
//!
//! Official Codex stays unmodified. Each port is independently gated.

pub mod bounded_continuation;
pub mod config;
pub mod context_pruner;
pub mod context_recovery;
pub mod digest;
pub mod gateway;
pub mod hooks;
pub mod lockfile;
pub mod notifications;
pub mod telemetry;
pub mod tool_observation;
pub mod tool_reliability;

pub use bounded_continuation::{
    commit_continuation, plan_turn_stop, release_continuation, trailing_intent_to_continue,
    AutoContinuationBudget, ContinuationDecision, ContinuationPlan, ReservedContinuation,
    TurnStopContext, UnfinishedSignal, CONTINUE_NUDGE, MAX_AUTO_CONTINUATIONS,
};
pub use config::{
    AblationProfile, EnhancedRuntimeFeatures, FeatureFlagError, FEATURE_FLAG_KEYS,
};
pub use context_pruner::{
    apply_pressure_prune, ContentBlock, ModelVisibleSurface, PruneOutcome, SurfaceItem,
    ToolResultPrunePolicy,
};
pub use context_recovery::{
    plan_context_pressure, plan_overflow_retry, CompactDecision, OverflowAttempt, OverflowDecision,
    OverflowPlan, PressurePlan, MAX_CONTEXT_OVERFLOW_RETRIES,
};
pub use digest::{
    compute_runtime_digest, is_pinned_git_sha, is_sha256_digest, DigestError, DigestInputs,
};
pub use gateway::{
    ForbiddenGatewayOperation, GatewayIsolationError, GatewayMutationGuard, GatewayOperation,
};
pub use hooks::{EnhancedTurnHooks, HookDecision};
pub use lockfile::{
    EnhancedRuntimeLockFile, BUILD_PROFILE_ENHANCED_MVP_V1, CODEX_UPSTREAM_COMMIT,
    DEEPSEEK_HARNESS_SOURCE_COMMIT, QWEN_CODE_SOURCE_COMMIT,
};
pub use notifications::{
    event_notification, identity_notification, EnhancedPortFlags, EnhancedRuntimeIdentityParams,
    ENHANCED_EVENT_NOTIFICATION, ENHANCED_IDENTITY_NOTIFICATION,
};
pub use telemetry::{
    field_name_is_forbidden, hash_identifier, EnhancedEvent, EnhancedEventFields,
    EnhancedEventKind, MemoryTelemetry,
};
pub use tool_observation::{
    fingerprint_original_result, is_native_wait_poll, observation_events, ObservationDecision,
    ObservationInput, ObservationReason, ObservationResultStatus, ObservationState,
    NATIVE_WAIT_POLL_TOOLS, REPETITION_NOTICE, REPETITION_THRESHOLD,
};
pub use tool_reliability::{
    canonical_namespace, AdmitDecision, InputEncoding, LateResultDecision, ProviderToolCallIdentity,
    SyntheticResultKind, SyntheticToolResult, ToolCallLedger, ToolCallResolution, ToolKind,
};

#[cfg(test)]
mod isolation_tests {
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn crate_sources_do_not_import_legacy_vellum_harness() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
        let forbidden = [
            "use vellum_proxy_runtime",
            "vellum_proxy_runtime::",
            "use crate::compaction",
            "canonical_v2::",
            "rolling_local::",
            "semantic_frontier::",
            "action_conversion::",
        ];
        for entry in fs::read_dir(&root).unwrap() {
            let path = entry.unwrap().path();
            let name = path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("");
            if name == "lib.rs" {
                continue;
            }
            if path.extension().and_then(|value| value.to_str()) != Some("rs") {
                continue;
            }
            let source = fs::read_to_string(&path).unwrap();
            for needle in forbidden {
                assert!(
                    !source.contains(needle),
                    "{} must not import {needle}",
                    path.display()
                );
            }
            for line in source.lines() {
                let trimmed = line.trim_start();
                if trimmed.starts_with("use crate::") {
                    panic!(
                        "{} must use super:: so it can nest under Codex core::enhanced (found `{trimmed}`)",
                        path.display()
                    );
                }
            }
        }
    }
}
