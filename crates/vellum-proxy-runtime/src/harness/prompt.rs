//! Explicit prompt families.
//!
//! Issue #6 Phase 1. Catalog generation used to copy `base_instructions` from
//! `models.first()` — whichever model Codex happened to list first. That made
//! the prompt contract depend on OpenAI's catalog ordering, and it described a
//! native environment the translated route does not provide. GPT-5.6 Sol
//! currently sits at priority 1, so the copy was often *approximately* right,
//! but by accident, and an accident is not a contract.
//!
//! These instructions are generated from the profile, the tools that are
//! actually exposed, and the shell that was actually detected. If the surface
//! shrinks, the prompt shrinks with it.

use super::shell::TerminalCapabilities;
use super::snapshot::ToolSnapshot;
use super::tools::APPLY_PATCH_TOOL_NAME;
use super::{HarnessProfile, ParallelPolicy, PatchContract, PromptFamily, ShellContract};

pub const BASE_IDENTITY: &str =
    "You are Codex, a coding agent. You and the user share the same workspace and collaborate to achieve the user's goals.";

pub const PARALLEL_INSPECTION_GUIDANCE: &str =
    "When several read-only inspections are already known to be necessary and independent, batch those tool calls in the same turn instead of issuing them sequentially. Do not batch speculative or high-volume searches.";

pub fn prompt_source(profile: &HarnessProfile) -> &'static str {
    match profile.prompt_family {
        PromptFamily::CodexOfficialNative => "codexOfficialBundledCatalog",
        PromptFamily::SolCompatibleGrok => "solCompatibleGrok",
        PromptFamily::GenericTranslated => "genericTranslated",
    }
}

/// Tool names the profile guarantees regardless of what a given request
/// declares. Used for the catalog-time prompt, which is written before any
/// request has arrived.
fn baseline_tool_names(profile: &HarnessProfile) -> Vec<&'static str> {
    let mut names = Vec::new();
    if profile.patch_contract != PatchContract::Unsupported {
        names.push(APPLY_PATCH_TOOL_NAME);
    }
    if profile.shell_contract != ShellContract::CodexNativeShellCommand {
        names.push("shell");
    }
    names
}

/// Generate the instructions for a translated route.
///
/// `tools` is the request-time surface. Passing an empty slice produces the
/// catalog-time prompt, which describes only what the profile guarantees.
pub fn instructions(
    profile: &HarnessProfile,
    capabilities: &TerminalCapabilities,
    tools: &[ToolSnapshot],
) -> String {
    instructions_with_options(profile, capabilities, tools, false, true)
}

pub fn instructions_with_parallel_guidance(
    profile: &HarnessProfile,
    capabilities: &TerminalCapabilities,
    tools: &[ToolSnapshot],
    parallel_guidance: bool,
) -> String {
    instructions_with_options(profile, capabilities, tools, parallel_guidance, true)
}

pub fn instructions_with_options(
    profile: &HarnessProfile,
    capabilities: &TerminalCapabilities,
    tools: &[ToolSnapshot],
    parallel_guidance: bool,
    action_bias_guidance: bool,
) -> String {
    if profile.prompt_family == PromptFamily::CodexOfficialNative {
        // Official routes are passthrough. Vellum has no business writing
        // instructions for a model whose harness it does not translate.
        return BASE_IDENTITY.to_string();
    }

    let mut sections: Vec<String> = vec![BASE_IDENTITY.to_string()];

    // Sorted so the prompt — and therefore its hash — depends on which tools
    // are exposed, not on the order Codex happened to declare them in.
    let mut sorted = tools.iter().collect::<Vec<_>>();
    sorted.sort_by(|left, right| left.name.cmp(&right.name));
    let (read_only, mutating): (Vec<&ToolSnapshot>, Vec<&ToolSnapshot>) =
        sorted.into_iter().partition(|tool| !tool.mutating);
    let has_patch_tool = profile.patch_contract != PatchContract::Unsupported
        && (tools.is_empty() || tools.iter().any(|tool| tool.name == APPLY_PATCH_TOOL_NAME));

    // --- What is on the table -------------------------------------------
    if tools.is_empty() {
        let baseline = baseline_tool_names(profile);
        if !baseline.is_empty() {
            sections.push(format!(
                "Tools available on this route: {}. Only call tools that appear in the current request.",
                baseline.join(", ")
            ));
        }
    } else {
        let mut lines = vec!["Tools available in this request:".to_string()];
        if !read_only.is_empty() {
            lines.push(format!(
                "- read-only: {}",
                read_only
                    .iter()
                    .map(|tool| tool.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !mutating.is_empty() {
            lines.push(format!(
                "- state-changing: {}",
                mutating
                    .iter()
                    .map(|tool| tool.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        lines.push(
            "Every tool listed has an exact schema. Send exactly the declared fields — no extras, no renamed fields.".into(),
        );
        sections.push(lines.join("\n"));
    }

    // --- Editing --------------------------------------------------------
    if has_patch_tool {
        sections.push(format!(
            "Editing files:\n\
             - Use `{APPLY_PATCH_TOOL_NAME}` for every local file change. It is the authoritative edit API on this route.\n\
             - Do not write files through the shell: no output redirection, no `Set-Content`/`Out-File`, no `sed -i`, no here-doc writes.\n\
             - Do not create a temporary Python or PowerShell script to perform a simple read or edit. A one-line change is one patch, not a script.\n\
             - If a patch is rejected, read the parser error, fix that specific problem, and send a corrected patch. Do not switch to shell editing because a patch failed."
        ));
    } else {
        sections.push(
            "This route exposes no patch tool. Make file changes only through tools explicitly declared in the request."
                .into(),
        );
    }

    sections.push(
        "Large or repetitive migrations:\n\
         - Never apply an unanchored regex across many files. Use an AST/codemod, or exact anchored replacements, one file at a time.\n\
         - Before a bulk change, state the intended file list and the exact replacement; after each batch, parse/typecheck the touched files.\n\
         - If a batch breaks parsing or type checking, revert that batch immediately and fall back to per-file exact edits. Do not push forward through a broken tree."
            .into(),
    );

    // --- Reading and searching -----------------------------------------
    let search_tools = read_only
        .iter()
        .filter(|tool| tool.name.contains("search") || tool.name.contains("grep"))
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>();
    let mut reading = vec!["Reading and searching:".to_string()];
    if !search_tools.is_empty() {
        reading.push(format!(
            "- Prefer the dedicated search tools ({}) over shell text processing.",
            search_tools.join(", ")
        ));
    }
    if capabilities.has_unix_utilities {
        reading.push("- When you do search from the shell, use `rg` rather than `grep`/`find`; it is faster and respects ignore files.".into());
    } else {
        reading.push("- `rg` is preferred if it is installed; otherwise use `Select-String`. Do not assume `grep`, `sed`, `awk`, `head`, `tail`, or `wc` exist.".into());
    }
    reading.push("- Read a file before editing it. Do not infer file contents from a filename or from an earlier summary.".into());
    sections.push(reading.join("\n"));

    // --- Shell ----------------------------------------------------------
    if profile.shell_contract != ShellContract::CodexNativeShellCommand {
        sections.push(format!(
            "Shell:\n\
             - {}\n\
             - Prefer the argv array form of `command`, one argument per element: it reaches the process without passing through a quoting layer, so spaces and non-ASCII paths survive. Use a single command string only for genuine shell work — pipelines, redirection, or shell builtins — and quote it yourself.\n\
             - The shell is for running programs (git, package managers, build and test commands). It is not the default file editor.",
            capabilities.guidance()
        ));
    }

    // --- Parallelism ----------------------------------------------------
    match profile.parallel_policy {
        ParallelPolicy::PromptAdvisoryReadOnly => {
            let mut guidance = "Parallelism: independent read-only calls (reads, searches, \
                                status queries) may be issued together in one turn."
                .to_string();
            if parallel_guidance {
                guidance.push(' ');
                guidance.push_str(PARALLEL_INSPECTION_GUIDANCE);
            }
            guidance.push_str(
                " Anything that changes state — patches, writes, installs, commits — must be \
                 issued one at a time and its result read before the next one.",
            );
            sections.push(guidance);
        }
        ParallelPolicy::Serialized => sections.push(
            "Parallelism: issue one tool call at a time and read its result before the next."
                .into(),
        ),
        ParallelPolicy::ProviderNative => {}
    }

    // --- Failure discipline ---------------------------------------------
    sections.push(
        "When something fails:\n\
         - Read the actual error text before retrying. Name what you are changing before you try again.\n\
         - Never run the same failing command a third time with the same strategy. After two consecutive failures on one step, change approach: different tool, smaller scope, or verify an assumption first.\n\
         - Do not re-run an unchanged verification that already passed. Re-run it only after the inputs changed."
            .into(),
    );

    // --- Action-bias execution discipline -------------------------------
    if action_bias_guidance && profile.prompt_family != PromptFamily::CodexOfficialNative {
        sections.push(
            "Execution:\n\
             - Do not keep gathering context after you have enough evidence for a small reversible change.\n\
             - Prefer edit → verify → refine over exhaustive investigation.\n\
             - Additional research is justified only when you can name the specific unknown that blocks the next change.\n\
             - A plausible implementation should be tested by acting, not by searching indefinitely for certainty."
                .into(),
        );
    }

    // --- Workspace and continuity ---------------------------------------
    sections.push(
        "Workspace:\n\
         - The user's working tree may already be dirty. Preserve unrelated uncommitted changes; never revert, stash, or clean them to make your own work easier.\n\
         - Track any temporary file you create and remove it before you finish.\n\
         - After a context compaction, continue the task already in progress. Do not restart from the beginning, re-derive settled decisions, or re-run completed work."
            .into(),
    );

    // --- Completion -----------------------------------------------------
    sections.push(
        "Finishing:\n\
         - Work through the requested scope in full. Do not stop early because context is filling up or the task is long — say what is left instead.\n\
         - Before claiming completion, re-read the request's checklist and confirm every required gate actually passed: the diff was inspected, type checking, tests, and build ran and succeeded, and no unintended generated or temporary files remain.\n\
         - A finished tool call is not a finished task. If a required gate did not pass, say plainly what is incomplete and why; never describe unverified work as done."
            .into(),
    );

    sections.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::shell::{Platform, ShellKind, ShellProbe};
    use crate::harness::{resolve_with_options, snapshot::ToolSnapshot, HarnessOptions};
    use crate::route::{RuntimeProviderKind, RuntimeWireFormat};
    use serde_json::json;

    fn windows_powershell() -> TerminalCapabilities {
        TerminalCapabilities::from_probe(&ShellProbe {
            platform: Some(Platform::Windows),
            powershell: Some("powershell.exe".into()),
            powershell_version: Some("5.1.26200.1".into()),
            ..Default::default()
        })
    }

    fn grok() -> HarnessProfile {
        resolve_with_options(
            RuntimeProviderKind::GrokCli,
            RuntimeWireFormat::Responses,
            HarnessOptions::default(),
            false,
        )
    }

    fn tool(name: &str, mutating: bool) -> ToolSnapshot {
        ToolSnapshot::new(name, "function", mutating, "", json!({"type": "object"}))
    }

    #[test]
    fn sol_prompt_states_the_full_working_contract() {
        let prompt = instructions(&grok(), &windows_powershell(), &[]);
        assert!(prompt.starts_with(BASE_IDENTITY));
        assert!(prompt.contains("apply_patch"));
        assert!(prompt.contains("no `sed -i`"));
        assert!(prompt.contains("temporary Python or PowerShell script"));
        assert!(prompt.contains("argv array"));
        assert!(prompt.contains("unanchored regex"));
        assert!(prompt.contains("read-only calls"));
        assert!(prompt.contains("a third time"));
        assert!(prompt.contains("Preserve unrelated uncommitted changes"));
        assert!(prompt.contains("After a context compaction"));
        assert!(prompt.contains("required gate"));
    }

    #[test]
    fn windows_powershell_prompt_never_teaches_unix_tools_or_and_and() {
        let prompt = instructions(&grok(), &windows_powershell(), &[]);
        assert!(prompt.contains("no `&&`"));
        assert!(prompt.contains("Select-String"));
        assert!(prompt.contains("Do not assume `grep`"));
        assert!(!prompt.contains("use `rg` rather than `grep`"));
    }

    #[test]
    fn posix_prompt_prefers_rg() {
        let capabilities = TerminalCapabilities::from_probe(&ShellProbe {
            platform: Some(Platform::Linux),
            shell_env: Some("/bin/bash".into()),
            ..Default::default()
        });
        let prompt = instructions(&grok(), &capabilities, &[]);
        assert!(prompt.contains("use `rg` rather than `grep`"));
        assert!(!prompt.contains("no `&&`"));
    }

    #[test]
    fn prompt_follows_the_tools_actually_exposed() {
        let tools = vec![
            tool("read_file", false),
            tool("search_files", false),
            tool("apply_patch", true),
            tool("shell", true),
        ];
        let prompt = instructions(&grok(), &windows_powershell(), &tools);
        assert!(prompt.contains("read-only: read_file, search_files"));
        assert!(prompt.contains("state-changing: apply_patch, shell"));
        assert!(prompt.contains("dedicated search tools (search_files)"));

        // A surface without a patch tool must not instruct the model to use one.
        let without_patch = instructions(&grok(), &windows_powershell(), &[tool("shell", true)]);
        assert!(without_patch.contains("exposes no patch tool"));
        assert!(!without_patch.contains("Use `apply_patch` for every local file change"));
    }

    #[test]
    fn official_routes_get_no_generated_instructions() {
        let profile = resolve_with_options(
            RuntimeProviderKind::Official,
            RuntimeWireFormat::Responses,
            HarnessOptions::default(),
            false,
        );
        assert_eq!(prompt_source(&profile), "codexOfficialBundledCatalog");
        assert_eq!(
            instructions(&profile, &windows_powershell(), &[]),
            BASE_IDENTITY
        );
    }

    #[test]
    fn generic_chat_routes_serialize_every_call() {
        let profile = resolve_with_options(
            RuntimeProviderKind::OpenAiCompatible,
            RuntimeWireFormat::Chat,
            HarnessOptions::default(),
            false,
        );
        let prompt = instructions(&profile, &windows_powershell(), &[]);
        assert_eq!(prompt_source(&profile), "genericTranslated");
        assert!(prompt.contains("issue one tool call at a time"));
        assert!(!prompt.contains("may be issued together"));
    }

    #[test]
    fn git_bash_prompt_mentions_path_conversion_guards() {
        let capabilities = TerminalCapabilities::from_probe(&ShellProbe {
            platform: Some(Platform::Windows),
            msystem: Some("MINGW64".into()),
            ..Default::default()
        });
        assert_eq!(capabilities.shell, ShellKind::GitBash);
        let prompt = instructions(&grok(), &capabilities, &[]);
        // Phrased as an instruction to the model: Vellum translates the
        // request, it does not spawn the shell, so it cannot pre-set this.
        assert!(prompt.contains("Set MSYS_NO_PATHCONV=1"));
        assert!(!prompt.contains("conversion is disabled"));
    }

    #[test]
    fn test_parallel_advisory_policy_includes_hardened_guidance() {
        let profile = grok();
        assert_eq!(
            profile.parallel_policy,
            ParallelPolicy::PromptAdvisoryReadOnly
        );
        let prompt =
            instructions_with_parallel_guidance(&profile, &windows_powershell(), &[], true);
        assert!(prompt.contains(PARALLEL_INSPECTION_GUIDANCE));
    }

    #[test]
    fn default_prompt_preserves_baseline_parallel_contract_without_hardened_guidance() {
        let profile = grok();
        let prompt = instructions(&profile, &windows_powershell(), &[]);
        assert!(prompt.contains("independent read-only calls"));
        assert!(!prompt.contains(PARALLEL_INSPECTION_GUIDANCE));
    }

    #[test]
    fn test_serialized_policy_omits_parallel_guidance() {
        let profile = resolve_with_options(
            RuntimeProviderKind::OpenAiCompatible,
            RuntimeWireFormat::Chat,
            HarnessOptions::default(),
            false,
        );
        assert_eq!(profile.parallel_policy, ParallelPolicy::Serialized);
        let prompt =
            instructions_with_parallel_guidance(&profile, &windows_powershell(), &[], true);
        assert!(!prompt.contains(PARALLEL_INSPECTION_GUIDANCE));
    }

    #[test]
    fn test_prompt_generation_is_deterministic_and_no_duplicates() {
        let profile = grok();
        let prompt1 =
            instructions_with_parallel_guidance(&profile, &windows_powershell(), &[], true);
        let prompt2 =
            instructions_with_parallel_guidance(&profile, &windows_powershell(), &[], true);
        assert_eq!(
            prompt1, prompt2,
            "Prompt generation must be strictly deterministic"
        );

        // Guidance appears exactly once in prompt
        let count = prompt1.matches(PARALLEL_INSPECTION_GUIDANCE).count();
        assert_eq!(
            count, 1,
            "Parallel inspection guidance must appear exactly once"
        );
    }

    #[test]
    fn test_blocker_e_grok_production_route_parallel_guidance() {
        let profile = crate::harness::resolve_with_options(
            crate::route::RuntimeProviderKind::GrokCli,
            crate::route::RuntimeWireFormat::Responses,
            crate::harness::HarnessOptions::default(),
            false,
        );
        assert_eq!(
            profile.parallel_policy,
            crate::harness::ParallelPolicy::PromptAdvisoryReadOnly
        );
        let prompt =
            instructions_with_parallel_guidance(&profile, &windows_powershell(), &[], true);
        assert!(prompt.contains("When several read-only inspections are already known to be necessary and independent, batch those tool calls in the same turn instead of issuing them sequentially."));
        assert_eq!(prompt.matches("Parallelism:").count(), 1);
    }

    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn sol_prompt_contains_action_bias_guidance() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let prompt = instructions(&grok(), &windows_powershell(), &[]);
        assert!(prompt.contains("Execution:"));
        assert!(prompt.contains("Prefer edit → verify → refine over exhaustive investigation."));
        assert!(prompt.contains("A plausible implementation should be tested by acting, not by searching indefinitely for certainty."));
    }

    #[test]
    fn official_routes_get_no_action_bias_guidance() {
        let prompt = instructions(
            &resolve_with_options(
                RuntimeProviderKind::Official,
                RuntimeWireFormat::Responses,
                HarnessOptions::default(),
                false,
            ),
            &windows_powershell(),
            &[],
        );
        assert!(!prompt.contains("Prefer edit → verify → refine"));
    }

    #[test]
    fn eval_switch_disables_action_bias_guidance() {
        let prompt = instructions_with_options(&grok(), &windows_powershell(), &[], false, false);
        assert!(!prompt.contains("Prefer edit → verify → refine"));
    }
}
