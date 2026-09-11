//! The resolved harness contract shared by catalog generation, prompt
//! generation, tool translation, and the runtime (plan §25.1).
//!
//! Issue #6. Vellum already had Codex *protocol* compatibility: the wire
//! shapes line up and Codex Desktop is happy. What it did not have was Codex
//! *agent-environment* parity — the catalog decided one thing, the adapter
//! decided another, and the prompt was inherited by accident from whichever
//! model happened to sit first in Codex's official catalog. The model then saw
//! a weaker editing contract than the prompt described and fell back to shell
//! one-liners and throwaway Python scripts.
//!
//! The fix is not to spoof GPT-5.6 Sol's provider-specific catalog flags.
//! `tool_mode` and `use_responses_lite` switch request behaviour a translated
//! provider cannot satisfy. `multi_agent_version` is different: it selects the
//! qualified Enhanced Codex host's local agent loop, not an upstream wire
//! capability, so a third-party model may advertise only the host-supported V2.
//! Sol is used
//! here as a *behavioural* contract reference, and every profile below states
//! exactly which parts of that contract Vellum can really execute.
//!
//! This is the *one* `HarnessProfile` type. Desktop and the headless daemon
//! must never grow a `DesktopHarnessProfile` / `RuntimeHarnessProfile` split:
//! only environment resolution differs, and that lives in each host's thin
//! environment adapter (Desktop reads env vars + delegation availability; a
//! remote agent reports its own executor facts).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::RuntimeError;
use crate::route::{RuntimeProviderKind, RuntimeWireFormat};

pub mod multi_agent;
pub mod prompt;
pub mod shell;
pub mod snapshot;
pub mod tools;

/// Which end-to-end harness a route actually runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HarnessProfileKind {
    /// ChatGPT-hosted models. Byte-for-byte passthrough; Vellum translates
    /// nothing and must never synthesise a prompt or a tool schema.
    CodexOfficialNative,
    /// Phase 1 target: Grok over the Responses wire with direct tools, Sol's
    /// behavioural contract, and exact translated schemas.
    GrokSolTranslatedDirect,
    /// Third-party Responses-compatible providers.
    GenericResponses,
    /// Third-party Chat Completions providers.
    GenericChat,
}

impl HarnessProfileKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CodexOfficialNative => "codexOfficialNative",
            Self::GrokSolTranslatedDirect => "grokSolTranslatedDirect",
            Self::GenericResponses => "genericResponses",
            Self::GenericChat => "genericChat",
        }
    }

    pub fn is_translated(self) -> bool {
        !matches!(self, Self::CodexOfficialNative)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PromptFamily {
    /// Codex's own bundled instructions, untouched.
    CodexOfficialNative,
    /// Explicit Sol-compatible instructions generated from the tools and shell
    /// that are actually exposed on this route.
    SolCompatibleGrok,
    /// Conservative translated instructions for providers whose editing tools
    /// have not been verified.
    GenericTranslated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HarnessToolMode {
    /// Codex decides; Vellum does not describe the surface.
    CodexNative,
    /// Flat, model-visible function list translated by Vellum.
    TranslatedDirect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PatchContract {
    /// Codex's freeform custom tool with its Lark grammar.
    NativeFreeform,
    /// Exact `{patch: string}` function that the adapter converts back into a
    /// native `custom_tool_call` before it reaches the apply_patch runtime.
    TranslatedExactFunction,
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ShellContract {
    CodexNativeShellCommand,
    /// argv-safe exec for single programs, shell scripts only for pipelines.
    TranslatedExecAndShell,
    TranslatedShellOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ParallelPolicy {
    ProviderNative,
    /// Read-only calls may fan out and mutations are serialised — asked for in
    /// the prompt, not enforced at dispatch. Vellum does not schedule tool
    /// calls, so it cannot prevent a concurrent mutation.
    PromptAdvisoryReadOnly,
    Serialized,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SearchPolicy {
    Native,
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ToolHistoryContract {
    Native,
    /// argv arrays stay arrays; anything that cannot be represented is marked
    /// unsupported rather than silently flattened into prose.
    LosslessTranslated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FailurePolicy {
    ModelManaged,
    /// The failure ledger and circuit breaker exist as checkable primitives and
    /// the rules are stated in the prompt, but nothing enforces them: a proxy
    /// never observes a tool's exit code, and it cannot refuse a tool call
    /// without fabricating a result, which would break the call/output pairing
    /// this harness is required to keep at 100%.
    PromptAdvisory,
    /// Reserved for a Vellum-side runtime that receives tool results and can
    /// actually block a third same-strategy retry. Not resolvable yet.
    LedgerWithCircuitBreaker,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CompletionPolicy {
    ModelManaged,
    /// Gate definitions and the partial/complete verdict exist and are stated
    /// in the prompt, but no component can withhold a completion claim.
    PromptAdvisory,
    /// Reserved for a runtime that observes gate status and can refuse a
    /// completion claim. Not resolvable yet.
    RequiredGates,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MultiAgentPolicy {
    /// Whatever generation the official catalog entry declares for the
    /// selected model. Vellum does not restate it.
    ProviderNative,
    NativeV2,
    NativeV1,
    /// Vellum's own delegation runtime, proven end to end. Resolvable only
    /// through an explicit opt-in (issue #6 Phase 8).
    VellumDelegated,
    /// No delegation runtime, so no delegation contract is advertised.
    SingleAgent,
}

/// One resolved contract. Catalog generation and the request adapter must both
/// go through [`resolve_with_options`] so they cannot describe different
/// environments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessProfile {
    pub kind: HarnessProfileKind,
    pub prompt_family: PromptFamily,
    pub tool_mode: HarnessToolMode,
    pub patch_contract: PatchContract,
    pub shell_contract: ShellContract,
    pub parallel_policy: ParallelPolicy,
    pub search_policy: SearchPolicy,
    pub history_contract: ToolHistoryContract,
    pub failure_policy: FailurePolicy,
    pub completion_policy: CompletionPolicy,
    pub multi_agent_policy: MultiAgentPolicy,
}

impl HarnessProfile {
    pub const fn official_native() -> Self {
        Self {
            kind: HarnessProfileKind::CodexOfficialNative,
            prompt_family: PromptFamily::CodexOfficialNative,
            tool_mode: HarnessToolMode::CodexNative,
            patch_contract: PatchContract::NativeFreeform,
            shell_contract: ShellContract::CodexNativeShellCommand,
            parallel_policy: ParallelPolicy::ProviderNative,
            search_policy: SearchPolicy::Native,
            history_contract: ToolHistoryContract::Native,
            failure_policy: FailurePolicy::ModelManaged,
            completion_policy: CompletionPolicy::ModelManaged,
            // Sol and Terra ship Multi-Agent V2 while Luna is still V1, and
            // `resolve` only sees provider and wire — not which native model
            // was selected. Naming a generation here would be a guess, so the
            // profile defers to whatever the official catalog entry declares.
            multi_agent_policy: MultiAgentPolicy::ProviderNative,
        }
    }

    /// Sol parity as far as a translated transport can honestly go.
    ///
    /// Enforced by Vellum: the prompt family, the exact tool schemas, the
    /// patch contract, and the lossless history translation — these are
    /// properties of the bytes Vellum emits, so they hold by construction.
    ///
    /// Stated but not enforced: the failure budget, the completion gates, and
    /// the parallelism rule. Vellum is a proxy; it never sees an exit code and
    /// cannot refuse a tool call without fabricating a result. Those three are
    /// marked `PromptAdvisory` so the profile does not advertise a guarantee
    /// the runtime cannot keep — which is the exact class of surface/runtime
    /// split this module exists to remove.
    pub const fn grok_sol_translated_direct() -> Self {
        Self {
            kind: HarnessProfileKind::GrokSolTranslatedDirect,
            prompt_family: PromptFamily::SolCompatibleGrok,
            tool_mode: HarnessToolMode::TranslatedDirect,
            patch_contract: PatchContract::TranslatedExactFunction,
            shell_contract: ShellContract::TranslatedExecAndShell,
            parallel_policy: ParallelPolicy::PromptAdvisoryReadOnly,
            search_policy: SearchPolicy::Disabled,
            history_contract: ToolHistoryContract::LosslessTranslated,
            failure_policy: FailurePolicy::PromptAdvisory,
            completion_policy: CompletionPolicy::PromptAdvisory,
            multi_agent_policy: MultiAgentPolicy::SingleAgent,
        }
    }

    pub const fn generic(wire: RuntimeWireFormat) -> Self {
        Self {
            kind: match wire {
                RuntimeWireFormat::Responses => HarnessProfileKind::GenericResponses,
                RuntimeWireFormat::Chat => HarnessProfileKind::GenericChat,
            },
            prompt_family: PromptFamily::GenericTranslated,
            tool_mode: HarnessToolMode::TranslatedDirect,
            patch_contract: PatchContract::TranslatedExactFunction,
            shell_contract: ShellContract::TranslatedShellOnly,
            parallel_policy: ParallelPolicy::Serialized,
            search_policy: SearchPolicy::Disabled,
            history_contract: ToolHistoryContract::LosslessTranslated,
            failure_policy: FailurePolicy::PromptAdvisory,
            completion_policy: CompletionPolicy::PromptAdvisory,
            multi_agent_policy: MultiAgentPolicy::SingleAgent,
        }
    }

    /// Whether Vellum may synthesise catalog prompt/tool metadata for this
    /// route. Official entries round-trip from Codex's own cache untouched.
    pub fn owns_catalog_prompt(&self) -> bool {
        self.kind.is_translated()
    }

    /// Sol's catalog advertises these; a translated route must not, because
    /// they switch Codex's private request path rather than a prompt feature.
    pub fn advertises_responses_lite(&self) -> bool {
        matches!(self.kind, HarnessProfileKind::CodexOfficialNative)
    }

    pub fn advertises_parallel_tool_calls(&self) -> bool {
        // Bounded read-only parallelism is a *prompt and dispatch* policy that
        // Vellum enforces itself. The catalog flag opts Codex into emitting
        // concurrent calls it expects the provider to accept, which is a
        // different claim, so it stays false until a provider proves it.
        matches!(self.parallel_policy, ParallelPolicy::ProviderNative)
    }

    /// Check a generated catalog entry against this profile.
    ///
    /// The catalog is written at configuration time and read by Codex; the
    /// adapter resolves its profile per request. If the two ever disagree —
    /// a hand-edited entry, a stale file, a future refactor that hardcodes a
    /// flag again — the model gets a surface the runtime cannot execute. That
    /// is the exact failure this module exists to prevent, so it fails closed.
    pub fn verify_catalog_entry(&self, entry: &Value) -> Result<(), RuntimeError> {
        let mismatch = |field: &str, advertised: &Value, allowed: bool| {
            RuntimeError::HarnessMismatch(format!(
                "catalog entry advertises {field}={advertised} but profile {} permits {allowed}; refusing to expose a model-visible surface the runtime cannot execute",
                self.kind.as_str()
            ))
        };
        for (field, allowed) in [
            ("use_responses_lite", self.advertises_responses_lite()),
            (
                "supports_parallel_tool_calls",
                self.advertises_parallel_tool_calls(),
            ),
        ] {
            if let Some(advertised) = entry.get(field) {
                if advertised.as_bool() == Some(true) && !allowed {
                    return Err(mismatch(field, advertised, allowed));
                }
            }
        }
        // A translated provider cannot execute Codex's provider-native tool
        // router. Multi-agent V2, however, runs in the qualified Enhanced
        // Codex host and sends each child through this same ordinary adapter.
        if self.kind.is_translated() {
            if let Some(advertised) = entry.get("tool_mode").filter(|value| !value.is_null()) {
                return Err(mismatch("tool_mode", advertised, false));
            }
            if let Some(advertised) = entry
                .get("multi_agent_version")
                .filter(|value| !value.is_null())
            {
                if advertised.as_str() != Some("v2") {
                    return Err(mismatch("multi_agent_version", advertised, true));
                }
            }
        }
        Ok(())
    }

    /// Fail closed when catalog generation and the adapter disagree. A split
    /// here is precisely the bug class this module exists to prevent, so it is
    /// an error rather than a warning.
    pub fn assert_consistent(catalog: &Self, adapter: &Self) -> Result<(), RuntimeError> {
        if catalog == adapter {
            return Ok(());
        }
        Err(RuntimeError::HarnessMismatch(format!(
            "catalog resolved {} but the request adapter resolved {}; refusing to expose a model-visible surface the runtime cannot execute",
            catalog.kind.as_str(),
            adapter.kind.as_str()
        )))
    }
}

/// Capabilities that have been proven on this installation.
///
/// Issue #6 Phases 7 and 8 are both gated: the issue makes Multi-Agent
/// conditional on single-agent Sol parity passing first, and that has not
/// happened — the failure budget and completion gates are `PromptAdvisory` and
/// the A–F benchmark has not run. Both default to off, so the runtimes exist
/// and are tested without anything being advertised to a model.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HarnessOptions {
    /// The delegation runtime has been proven end to end (Phase 8). Also gates
    /// `ultra`, which is maximum reasoning *with automatic task delegation*.
    pub delegation_verified: bool,
}

/// The single pure resolution point. Every caller — catalog, prompt, adapter,
/// snapshot — must derive its profile here, and every host must pass its own
/// environment facts explicitly:
///
/// * `options` carries host-proven capabilities (opt-in switches).
/// * `delegation_runtime_wired` tells the resolver whether a real delegation
///   runtime exists on this host. Desktop passes its `multi_agent` wiring;
///   a remote daemon reports its own.
///
/// Nothing in here reads the environment or a live host, so Desktop and the
/// daemon resolve identical profiles for identical inputs.
pub fn resolve_with_options(
    provider_kind: RuntimeProviderKind,
    wire: RuntimeWireFormat,
    options: HarnessOptions,
    delegation_runtime_wired: bool,
) -> HarnessProfile {
    match provider_kind {
        RuntimeProviderKind::Official => HarnessProfile::official_native(),
        RuntimeProviderKind::GrokCli => {
            let mut profile = HarnessProfile::grok_sol_translated_direct();
            if options.delegation_verified {
                if delegation_runtime_wired {
                    profile.multi_agent_policy = MultiAgentPolicy::VellumDelegated;
                } else {
                    log::warn!(
                        "[Harness] delegation is verified but no delegation runtime is wired on this host; staying single-agent"
                    );
                }
            }
            profile
        }
        RuntimeProviderKind::OpenAiCompatible => HarnessProfile::generic(wire),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grok_responses() -> HarnessProfile {
        resolve_with_options(
            RuntimeProviderKind::GrokCli,
            RuntimeWireFormat::Responses,
            HarnessOptions::default(),
            false,
        )
    }

    #[test]
    fn grok_resolves_to_the_sol_translated_direct_profile() {
        let profile = grok_responses();
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
            grok_responses(),
            resolve_with_options(
                RuntimeProviderKind::OpenAiCompatible,
                RuntimeWireFormat::Responses,
                HarnessOptions::default(),
                false,
            ),
            resolve_with_options(
                RuntimeProviderKind::OpenAiCompatible,
                RuntimeWireFormat::Chat,
                HarnessOptions::default(),
                false,
            ),
        ] {
            assert!(!profile.advertises_responses_lite());
            assert!(!profile.advertises_parallel_tool_calls());
            assert_eq!(profile.multi_agent_policy, MultiAgentPolicy::SingleAgent);
        }
    }

    #[test]
    fn official_keeps_its_native_contract_and_owns_no_generated_prompt() {
        let profile = resolve_with_options(
            RuntimeProviderKind::Official,
            RuntimeWireFormat::Responses,
            HarnessOptions::default(),
            false,
        );
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
            grok_responses(),
            resolve_with_options(
                RuntimeProviderKind::OpenAiCompatible,
                RuntimeWireFormat::Responses,
                HarnessOptions::default(),
                false,
            ),
            resolve_with_options(
                RuntimeProviderKind::OpenAiCompatible,
                RuntimeWireFormat::Chat,
                HarnessOptions::default(),
                false,
            ),
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
            grok_responses().parallel_policy,
            ParallelPolicy::PromptAdvisoryReadOnly
        );
    }

    #[test]
    fn delegation_stays_off_by_default_and_wires_only_when_verified() {
        let off = resolve_with_options(
            RuntimeProviderKind::GrokCli,
            RuntimeWireFormat::Responses,
            HarnessOptions::default(),
            true,
        );
        assert_eq!(off.multi_agent_policy, MultiAgentPolicy::SingleAgent);

        let verified = resolve_with_options(
            RuntimeProviderKind::GrokCli,
            RuntimeWireFormat::Responses,
            HarnessOptions {
                delegation_verified: true,
            },
            true,
        );
        assert_eq!(
            verified.multi_agent_policy,
            MultiAgentPolicy::VellumDelegated
        );

        // Verified but no runtime to dispatch it: must not advertise.
        let orphaned = resolve_with_options(
            RuntimeProviderKind::GrokCli,
            RuntimeWireFormat::Responses,
            HarnessOptions {
                delegation_verified: true,
            },
            false,
        );
        assert_eq!(orphaned.multi_agent_policy, MultiAgentPolicy::SingleAgent);
    }

    /// Issue #6 Phases 7 and 8 are both opt-in. The issue gates Multi-Agent on
    /// single-agent Sol parity passing first, and it has not — so the default
    /// resolution must advertise neither, however complete the runtimes are.

    #[test]
    fn a_catalog_entry_advertising_private_sol_flags_is_rejected() {
        let profile = grok_responses();
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
        let catalog = grok_responses();
        let adapter = resolve_with_options(
            RuntimeProviderKind::OpenAiCompatible,
            RuntimeWireFormat::Chat,
            HarnessOptions::default(),
            false,
        );
        let error = HarnessProfile::assert_consistent(&catalog, &adapter).unwrap_err();
        assert!(error.to_string().contains("harness profile mismatch"));
        HarnessProfile::assert_consistent(&catalog, &catalog).unwrap();
    }

    #[test]
    fn the_shared_type_serializes_to_the_legacy_desktop_shape() {
        let profile = grok_responses();
        let value = serde_json::to_value(profile).unwrap();
        assert_eq!(value["kind"], "grokSolTranslatedDirect");
        assert_eq!(value["patchContract"], "translatedExactFunction");
        assert_eq!(value["shellContract"], "translatedExecAndShell");
        assert_eq!(value["multiAgentPolicy"], "singleAgent");
    }
}
