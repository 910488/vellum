//! The resolved harness contract shared by catalog generation, prompt
//! generation, tool translation, and the runtime.
//!
//! Issue #6. Vellum already had Codex *protocol* compatibility: the wire
//! shapes line up and Codex Desktop is happy. What it did not have was Codex
//! *agent-environment* parity — the catalog decided one thing, the adapter
//! decided another, and the prompt was inherited by accident from whichever
//! model happened to sit first in Codex's official catalog. The model then saw
//! a weaker editing contract than the prompt described and fell back to shell
//! one-liners and throwaway Python scripts.
//!
//! M3A: the shared contract now lives in `vellum-proxy-runtime::harness`
//! (plan §25.1). This module re-exports that single type set so every existing
//! `crate::harness::*` call site keeps resolving identically, and keeps only
//! the *environment resolution* that is Desktop-specific: reading the
//! opt-in environment variables, reporting live delegation availability, and
//! mapping Desktop value types onto the runtime's value types. The pure
//! profile resolver belongs to the runtime; Desktop never re-implements it.

pub mod failure;
pub mod gates;
pub mod multi_agent;
pub mod prompt;
pub mod shell;
pub mod snapshot;
pub mod tools;

pub use vellum_proxy_runtime::environment::ExecutionEnvironment;
pub use vellum_proxy_runtime::harness::{
    CompletionPolicy, FailurePolicy, HarnessOptions, HarnessProfile, HarnessProfileKind,
    HarnessToolMode, MultiAgentPolicy, ParallelPolicy, PatchContract, PromptFamily, SearchPolicy,
    ShellContract, ToolHistoryContract,
};

pub use failure::{FailureCategory, FailureLedger, RetryVerdict, ToolFailure};
pub use gates::{AcceptanceGate, CompletionLedger, CompletionVerdict, GateStatus};
pub use shell::{
    CommandOutcome, CommandPlan, ExecInput, ShellKind, ShellProbe, TerminalCapabilities,
};
pub use snapshot::{ModelVisibleHarnessSnapshot, ToolSnapshot};

use crate::model::{AuthKind, ProviderKind, ReasoningEffortTransport, WireFormat};
use vellum_proxy_runtime::route::{
    RuntimeAuthKind, RuntimeProviderKind, RuntimeReasoningEffortTransport, RuntimeWireFormat,
};

impl From<WireFormat> for RuntimeWireFormat {
    fn from(value: WireFormat) -> Self {
        match value {
            WireFormat::Responses => RuntimeWireFormat::Responses,
            WireFormat::Chat => RuntimeWireFormat::Chat,
        }
    }
}

impl From<ProviderKind> for RuntimeProviderKind {
    fn from(value: ProviderKind) -> Self {
        match value {
            ProviderKind::Official => RuntimeProviderKind::Official,
            ProviderKind::OpenAiCompatible => RuntimeProviderKind::OpenAiCompatible,
            ProviderKind::GrokCli => RuntimeProviderKind::GrokCli,
        }
    }
}

impl From<AuthKind> for RuntimeAuthKind {
    fn from(value: AuthKind) -> Self {
        match value {
            AuthKind::ChatGpt => RuntimeAuthKind::ChatGpt,
            AuthKind::Bearer => RuntimeAuthKind::Bearer,
            AuthKind::GrokSession => RuntimeAuthKind::GrokSession,
            AuthKind::None => RuntimeAuthKind::None,
        }
    }
}

impl From<ReasoningEffortTransport> for RuntimeReasoningEffortTransport {
    fn from(value: ReasoningEffortTransport) -> Self {
        match value {
            ReasoningEffortTransport::ResponsesObject => {
                RuntimeReasoningEffortTransport::ResponsesObject
            }
            ReasoningEffortTransport::ChatField => RuntimeReasoningEffortTransport::ChatField,
            ReasoningEffortTransport::ChatObject => RuntimeReasoningEffortTransport::ChatObject,
            ReasoningEffortTransport::ProviderSpecific => {
                RuntimeReasoningEffortTransport::ProviderSpecific
            }
            ReasoningEffortTransport::None => RuntimeReasoningEffortTransport::None,
        }
    }
}

/// Desktop's thin environment resolver: reads the host opt-in switches, asks
/// the delegation module whether a real runtime is wired, and hands the pure
/// runtime resolver the explicit facts. Catalog, prompt, adapter, and snapshot
/// must all derive their profile here.
pub fn resolve(provider_kind: ProviderKind, wire: WireFormat) -> HarnessProfile {
    resolve_with_options(provider_kind, wire, harness_options_from_env())
}

pub fn resolve_with_options(
    provider_kind: ProviderKind,
    wire: WireFormat,
    options: HarnessOptions,
) -> HarnessProfile {
    vellum_proxy_runtime::harness::resolve_with_options(
        provider_kind.into(),
        wire.into(),
        options,
        multi_agent::RUNTIME_WIRED,
    )
}

/// Host-proven capabilities read from the environment. This is Desktop's thin
/// environment adapter; the pure resolver in the runtime never reads env vars
/// itself. Absent or unset means off — a capability is never enabled by the
/// code merely existing.
pub fn harness_options_from_env() -> HarnessOptions {
    let enabled = |name: &str| {
        std::env::var(name)
            .ok()
            .is_some_and(|value| matches!(value.trim(), "1" | "true" | "on"))
    };
    HarnessOptions {
        delegation_verified: enabled("VELLUM_DELEGATION_VERIFIED"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::WireFormat;

    #[test]
    fn grok_resolves_to_the_sol_translated_direct_profile() {
        let profile = resolve(ProviderKind::GrokCli, WireFormat::Responses);
        assert_eq!(profile.kind, HarnessProfileKind::GrokSolTranslatedDirect);
        assert_eq!(profile.prompt_family, PromptFamily::SolCompatibleGrok);
        assert_eq!(
            profile.patch_contract,
            PatchContract::TranslatedExactFunction
        );
        assert_eq!(
            profile.shell_contract,
            ShellContract::TranslatedExecAndShell
        );
        assert_eq!(
            profile.history_contract,
            ToolHistoryContract::LosslessTranslated
        );
    }

    #[test]
    fn translated_profiles_never_advertise_private_sol_flags() {
        for profile in [
            resolve(ProviderKind::GrokCli, WireFormat::Responses),
            resolve(ProviderKind::OpenAiCompatible, WireFormat::Responses),
            resolve(ProviderKind::OpenAiCompatible, WireFormat::Chat),
        ] {
            assert!(!profile.advertises_responses_lite());
            assert!(!profile.advertises_parallel_tool_calls());
            assert_eq!(profile.multi_agent_policy, MultiAgentPolicy::SingleAgent);
        }
    }

    #[test]
    fn official_keeps_its_native_contract_and_owns_no_generated_prompt() {
        let profile = resolve(ProviderKind::Official, WireFormat::Responses);
        assert!(!profile.owns_catalog_prompt());
        assert!(profile.advertises_responses_lite());
        assert_eq!(profile.prompt_family, PromptFamily::CodexOfficialNative);
        // Sol and Terra are Multi-Agent V2 while Luna is V1, and `resolve` does
        // not see which model was selected. Naming a generation here would be
        // a guess that contradicts the catalog for one of them.
        assert_eq!(profile.multi_agent_policy, MultiAgentPolicy::ProviderNative);
    }

    /// Issue #6 review, P0-4: a policy must not claim enforcement the runtime
    /// cannot provide. Vellum is a proxy — it never observes an exit code and
    /// cannot refuse a tool call without breaking call/output pairing.
    #[test]
    fn translated_profiles_declare_unenforceable_policies_as_advisory() {
        for profile in [
            resolve(ProviderKind::GrokCli, WireFormat::Responses),
            resolve(ProviderKind::OpenAiCompatible, WireFormat::Responses),
            resolve(ProviderKind::OpenAiCompatible, WireFormat::Chat),
        ] {
            assert_eq!(profile.failure_policy, FailurePolicy::PromptAdvisory);
            assert_eq!(profile.completion_policy, CompletionPolicy::PromptAdvisory);
            assert_ne!(profile.parallel_policy, ParallelPolicy::ProviderNative);
            assert_ne!(
                profile.failure_policy,
                FailurePolicy::LedgerWithCircuitBreaker
            );
            assert_ne!(profile.completion_policy, CompletionPolicy::RequiredGates);
        }
        assert_eq!(
            resolve(ProviderKind::GrokCli, WireFormat::Responses).parallel_policy,
            ParallelPolicy::PromptAdvisoryReadOnly
        );
    }

    /// Issue #6 Phases 7 and 8 are both opt-in. The issue gates Multi-Agent on
    /// single-agent Sol parity passing first, and it has not — so the default
    /// resolution must advertise neither, however complete the runtimes are.

    #[test]
    fn a_catalog_entry_advertising_private_sol_flags_is_rejected() {
        let profile = resolve(ProviderKind::GrokCli, WireFormat::Responses);
        // What the generator actually produces.
        profile
            .verify_catalog_entry(&serde_json::json!({
                "use_responses_lite": false,
                "supports_parallel_tool_calls": false
            }))
            .unwrap();

        for bad in [
            serde_json::json!({"use_responses_lite": true}),
            serde_json::json!({"supports_parallel_tool_calls": true}),
            serde_json::json!({"tool_mode": "code_mode_only"}),
            serde_json::json!({"multi_agent_version": "v1"}),
        ] {
            let error = profile.verify_catalog_entry(&bad).unwrap_err().to_string();
            assert!(error.contains("harness profile mismatch"), "{error}");
        }
        profile
            .verify_catalog_entry(&serde_json::json!({"multi_agent_version": "v2"}))
            .unwrap();

        // Official entries legitimately carry all of them.
        HarnessProfile::official_native()
            .verify_catalog_entry(&serde_json::json!({
                "use_responses_lite": true,
                "tool_mode": "code_mode_only",
                "multi_agent_version": "v2"
            }))
            .unwrap();
    }

    #[test]
    fn mismatched_catalog_and_adapter_profiles_fail_closed() {
        let catalog = resolve(ProviderKind::GrokCli, WireFormat::Responses);
        let adapter = resolve(ProviderKind::OpenAiCompatible, WireFormat::Chat);
        let error = HarnessProfile::assert_consistent(&catalog, &adapter).unwrap_err();
        assert!(error.to_string().contains("harness profile mismatch"));
        HarnessProfile::assert_consistent(&catalog, &catalog).unwrap();
    }

    #[test]
    fn desktop_resolve_maps_onto_the_runtime_value_types() {
        let desktop_profile = resolve(ProviderKind::OpenAiCompatible, WireFormat::Chat);
        let runtime_profile = vellum_proxy_runtime::harness::resolve_with_options(
            ProviderKind::OpenAiCompatible.into(),
            WireFormat::Chat.into(),
            HarnessOptions::default(),
            true,
        );
        assert_eq!(desktop_profile, runtime_profile);
    }
}
