//! Canonical evidence operations projected from completed tool exchanges.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::investigation::{extract_investigation_targets, is_self_transcript_search};
use crate::tool_output_normalizer::{normalize_output_text, normalize_tool_output, ErrorFamily};
use crate::tool_semantics::{
    classify_command, extract_command_string, extract_exit_code, CommandInvocation, CommandKind,
    SearchOutcome,
};

fn sha256_hex(payload: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(payload.as_bytes()))
}

/// High-level kind of an evidence-producing operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub enum EvidenceOperationKind {
    Query,
    Inspect,
    Verify,
    Mutate,
    Execute,
}

/// Evidence source channel. Mutable coverage, never investigation identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub enum EvidenceChannel {
    Workspace,
    Session,
    Vcs,
    Database,
    Process,
    External,
    Unknown,
}

/// Subject kind used for symbolic linking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub enum CanonicalSubjectKind {
    File,
    Symbol,
    TaskLiteral,
    ErrorFamily,
    Test,
    SessionState,
    RepositoryState,
    Other,
}

/// A normalized subject an investigation is about.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub struct CanonicalSubject {
    pub kind: CanonicalSubjectKind,
    pub key: String,
}

impl CanonicalSubject {
    pub fn new(kind: CanonicalSubjectKind, key: impl Into<String>) -> Self {
        Self {
            kind,
            key: key.into(),
        }
    }

    pub fn display(&self) -> String {
        format!("{:?}:{}", self.kind, self.key)
    }
}

/// Provenance of a structured fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub enum FactProvenance {
    Independent,
    SelfGeneratedConversation,
}

/// Result class of an evidence operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EvidenceResult {
    Found,
    NoResult,
    Error { family: ErrorFamily },
    Completed,
    Unknown,
}

/// Identity tier for a stable evidence signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub enum EvidenceSignatureLevel {
    StructuredFact,
    NormalizedDigest,
}

/// Stable, auditable evidence identity. Embedding vectors are never stored.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceSignature {
    pub level: EvidenceSignatureLevel,
    pub digest: String,
    pub provenance: FactProvenance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fact: Option<String>,
}

/// Canonical event projected from a completed tool exchange.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceOperation {
    pub source_index: usize,
    pub occurrence_id: String,
    pub operation: EvidenceOperationKind,
    pub channel: EvidenceChannel,
    pub subjects: Vec<CanonicalSubject>,
    pub result: EvidenceResult,
    pub stable_evidence: Vec<EvidenceSignature>,
    pub descriptor: String,
    pub completed: bool,
}

/// Predicate used as the durable investigation identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub enum InvestigationPredicate {
    Locate,
    Exists,
    Inspect,
    Verify,
    Diagnose,
    Resolve,
    Unknown,
}

/// Map a completed command invocation into a canonical evidence operation.
pub fn project_evidence_operation(
    invocation: &CommandInvocation,
    output_text: &str,
    occurrence_id: &str,
    task_literals: &[String],
    completed: bool,
) -> EvidenceOperation {
    let normalized_text = normalize_output_text(output_text);
    let channel = classify_channel(&invocation.command, invocation.kind);
    let operation = classify_operation_kind(&invocation.command, invocation.kind);
    let subjects = extract_subjects(&invocation.command, task_literals);
    let result = classify_result(invocation, &normalized_text);
    let self_transcript = is_self_generated_session_fact(
        &invocation.command,
        channel,
        task_literals,
        &normalized_text,
    );
    let stable_evidence = build_signatures(
        invocation,
        &normalized_text,
        &subjects,
        &result,
        self_transcript,
    );
    let descriptor = bounded_descriptor(&invocation.command, operation, channel, &subjects);
    EvidenceOperation {
        source_index: invocation.source_index,
        occurrence_id: occurrence_id.to_string(),
        operation,
        channel,
        subjects,
        result,
        stable_evidence,
        descriptor,
        completed,
    }
}

/// Project from a raw call/output pair.
pub fn project_from_tool_pair(
    source_index: usize,
    call: &Value,
    outputs: &[&Value],
    occurrence_id: &str,
    task_literals: &[String],
    completed: bool,
) -> Option<EvidenceOperation> {
    let command = extract_command_string(call)?;
    let kind = classify_command(&command);
    let normalized = normalize_tool_output(outputs);
    let exit_code = normalized
        .exit_code
        .or_else(|| outputs.iter().find_map(|out| extract_exit_code(out)));
    let search_outcome = if is_search_like(&command, kind) {
        Some(crate::tool_semantics::classify_search_outcome(
            &command,
            &normalized.text,
            exit_code,
        ))
    } else {
        None
    };
    let invocation = CommandInvocation {
        source_index,
        call_id: call
            .get("call_id")
            .or_else(|| call.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string),
        command,
        kind,
        exit_code,
        output_hash: Some(sha256_hex(&normalized.text)),
        search_outcome,
    };
    Some(project_evidence_operation(
        &invocation,
        &normalized.text,
        occurrence_id,
        task_literals,
        completed,
    ))
}

pub fn classify_channel(command: &str, kind: CommandKind) -> EvidenceChannel {
    let lower = command.to_ascii_lowercase().replace('\\', "/");
    if is_session_command(&lower) {
        return EvidenceChannel::Session;
    }
    if looks_like_database(&lower) {
        return EvidenceChannel::Database;
    }
    if kind == CommandKind::Git || first_token(&lower) == "git" {
        return EvidenceChannel::Vcs;
    }
    if kind == CommandKind::Process {
        return EvidenceChannel::Process;
    }
    if lower.contains("curl ") || lower.contains("wget ") || lower.contains("invoke-webrequest") {
        return EvidenceChannel::External;
    }
    match kind {
        CommandKind::Search | CommandKind::Read | CommandKind::Write | CommandKind::Test => {
            EvidenceChannel::Workspace
        }
        CommandKind::Other if is_python(&lower) => EvidenceChannel::Workspace,
        _ => {
            if is_python(&lower) {
                EvidenceChannel::Workspace
            } else {
                EvidenceChannel::Unknown
            }
        }
    }
}

pub fn classify_operation_kind(command: &str, kind: CommandKind) -> EvidenceOperationKind {
    let lower = command.to_ascii_lowercase();
    if is_git_mutation(&lower) {
        return EvidenceOperationKind::Mutate;
    }
    match kind {
        CommandKind::Write => EvidenceOperationKind::Mutate,
        CommandKind::Test => EvidenceOperationKind::Verify,
        CommandKind::Search => EvidenceOperationKind::Query,
        CommandKind::Read => {
            if lower.contains("test-path") {
                EvidenceOperationKind::Query
            } else {
                EvidenceOperationKind::Inspect
            }
        }
        CommandKind::Git => {
            if lower.contains(" grep") || lower.contains(" log") || lower.contains(" show") {
                EvidenceOperationKind::Query
            } else {
                EvidenceOperationKind::Inspect
            }
        }
        CommandKind::Process => EvidenceOperationKind::Execute,
        CommandKind::Build | CommandKind::Format | CommandKind::Lint => {
            EvidenceOperationKind::Execute
        }
        CommandKind::Other => {
            if is_python(&lower) {
                EvidenceOperationKind::Query
            } else {
                EvidenceOperationKind::Execute
            }
        }
    }
}

pub fn predicate_for_operation(
    operation: EvidenceOperationKind,
    command: &str,
) -> InvestigationPredicate {
    let lower = command.to_ascii_lowercase();
    if lower.contains("test-path") {
        return InvestigationPredicate::Exists;
    }
    let channel = classify_channel(command, classify_command(command));
    if matches!(
        channel,
        EvidenceChannel::Session | EvidenceChannel::Vcs | EvidenceChannel::Database
    ) && matches!(
        operation,
        EvidenceOperationKind::Query | EvidenceOperationKind::Inspect
    ) {
        return InvestigationPredicate::Locate;
    }
    match operation {
        EvidenceOperationKind::Query => InvestigationPredicate::Locate,
        EvidenceOperationKind::Inspect => InvestigationPredicate::Inspect,
        EvidenceOperationKind::Verify => InvestigationPredicate::Verify,
        EvidenceOperationKind::Mutate => InvestigationPredicate::Resolve,
        EvidenceOperationKind::Execute => InvestigationPredicate::Diagnose,
    }
}

pub fn extract_subjects(command: &str, task_literals: &[String]) -> Vec<CanonicalSubject> {
    let mut subjects: Vec<CanonicalSubject> = extract_investigation_targets(command, task_literals)
        .into_iter()
        .map(|target| subject_from_target(&target))
        .collect();

    let channel = classify_channel(command, classify_command(command));
    // A session-store probe has one safe generic subject even when the
    // command does not repeat a task literal. Do not attach every prompt
    // literal to all VCS/database operations: that lets unrelated commands
    // merge merely because they occurred during the same task and can mint
    // false facts for subjects the command never addressed.
    if channel == EvidenceChannel::Session {
        subjects.push(CanonicalSubject::new(
            CanonicalSubjectKind::SessionState,
            "prior_task_state",
        ));
    }
    if classify_command(command) == CommandKind::Git {
        subjects.push(CanonicalSubject::new(
            CanonicalSubjectKind::RepositoryState,
            "repository",
        ));
    }

    subjects.sort();
    subjects.dedup();
    subjects
}

pub fn normalize_file_subject(path: &str) -> String {
    let mut p = path.replace('\\', "/");
    while p.starts_with("./") {
        p = p[2..].to_string();
    }
    if p.len() >= 2 && p.as_bytes()[1] == b':' {
        let drive = p[..1].to_ascii_lowercase();
        p.replace_range(..1, &drive);
    }
    let parts: Vec<&str> = p
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    if parts.is_empty() {
        return path.to_string();
    }
    if parts.len() == 1 {
        return parts[0].to_string();
    }
    // Without an explicit workspace root, stripping an arbitrary leading
    // directory is unsafe: C:\repo-a\x and C:\repo-b\x are distinct
    // subjects. False negatives are preferable to merging unrelated files.
    parts.join("/")
}

fn subject_from_target(target: &str) -> CanonicalSubject {
    let looks_like_file = target.contains('.')
        && !target.contains(' ')
        && target.chars().all(|ch| {
            ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-' | '/' | '\\' | ':')
        });
    if looks_like_file {
        CanonicalSubject::new(CanonicalSubjectKind::File, normalize_file_subject(target))
    } else if target
        .chars()
        .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
        && target.contains('_')
    {
        CanonicalSubject::new(CanonicalSubjectKind::TaskLiteral, target.to_string())
    } else if target.contains('.') || target.contains("::") {
        CanonicalSubject::new(CanonicalSubjectKind::Symbol, target.to_string())
    } else {
        CanonicalSubject::new(CanonicalSubjectKind::TaskLiteral, target.to_string())
    }
}

fn classify_result(invocation: &CommandInvocation, normalized_text: &str) -> EvidenceResult {
    if is_successful_negative_search(invocation, normalized_text) {
        return EvidenceResult::NoResult;
    }
    if invocation.exit_code.is_some_and(|code| code != 0) {
        return EvidenceResult::Error {
            family: error_family_from_text(normalized_text, invocation.exit_code)
                .unwrap_or(ErrorFamily::Other),
        };
    }
    if let Some(family) = error_family_from_text(normalized_text, invocation.exit_code) {
        if !is_successful_negative_search(invocation, normalized_text) {
            return EvidenceResult::Error { family };
        }
    }
    match invocation.search_outcome {
        Some(SearchOutcome::Match) => EvidenceResult::Found,
        Some(SearchOutcome::NoMatch) => EvidenceResult::NoResult,
        Some(SearchOutcome::Error) => EvidenceResult::Error {
            family: error_family_from_text(normalized_text, invocation.exit_code)
                .unwrap_or(ErrorFamily::Other),
        },
        Some(SearchOutcome::Unknown) => EvidenceResult::Unknown,
        None => {
            if invocation.kind == CommandKind::Write
                || invocation.kind == CommandKind::Test
                || invocation.kind == CommandKind::Build
            {
                EvidenceResult::Completed
            } else if normalized_text.trim().is_empty() {
                EvidenceResult::NoResult
            } else {
                EvidenceResult::Completed
            }
        }
    }
}

fn is_successful_negative_search(invocation: &CommandInvocation, _text: &str) -> bool {
    matches!(invocation.search_outcome, Some(SearchOutcome::NoMatch))
        || invocation.exit_code == Some(1) && is_grep_like(&invocation.command)
}

fn is_grep_like(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    let mut tokens = lower.split_whitespace();
    let first = tokens
        .next()
        .unwrap_or("")
        .trim_matches(|c| c == '"' || c == '\'');
    matches!(first, "rg" | "grep" | "ripgrep") || (first == "git" && tokens.next() == Some("grep"))
}

fn error_family_from_text(text: &str, exit_code: Option<i32>) -> Option<ErrorFamily> {
    crate::tool_output_normalizer::classify_error_family_from_parts(text, exit_code, None)
}

fn build_signatures(
    invocation: &CommandInvocation,
    normalized_text: &str,
    subjects: &[CanonicalSubject],
    result: &EvidenceResult,
    self_transcript: bool,
) -> Vec<EvidenceSignature> {
    let provenance = if self_transcript {
        FactProvenance::SelfGeneratedConversation
    } else {
        FactProvenance::Independent
    };
    let mut sigs = Vec::new();
    let result_label = match result {
        EvidenceResult::Found => "found",
        EvidenceResult::NoResult => "no_result",
        EvidenceResult::Error { family } => match family {
            ErrorFamily::PermissionDenied => "error:permission_denied",
            ErrorFamily::NotFound => "error:not_found",
            ErrorFamily::Locked => "error:locked",
            ErrorFamily::InvalidObject => "error:invalid_object",
            ErrorFamily::ParseFailed => "error:parse_failed",
            ErrorFamily::Timeout => "error:timeout",
            ErrorFamily::Other => "error:other",
        },
        EvidenceResult::Completed => "completed",
        EvidenceResult::Unknown => "unknown",
    };
    for subject in subjects {
        let fact = format!(
            "{:?}:{}@{}/{}={}",
            subject.kind,
            subject.key,
            format!(
                "{:?}",
                classify_channel(&invocation.command, invocation.kind)
            )
            .to_ascii_lowercase(),
            result_label,
            if matches!(result, EvidenceResult::Found) {
                "true"
            } else if matches!(result, EvidenceResult::NoResult) {
                "false"
            } else {
                result_label
            }
        );
        let channel = classify_channel(&invocation.command, invocation.kind);
        let canonical = format!(
            "v1|{:?}|{}|{:?}|{}|{:?}",
            subject.kind, subject.key, channel, result_label, provenance
        );
        sigs.push(EvidenceSignature {
            level: EvidenceSignatureLevel::StructuredFact,
            digest: sha256_hex(&canonical),
            provenance,
            fact: Some(fact.chars().take(160).collect()),
        });
    }
    if sigs.is_empty() {
        let canonical = format!("v1|digest|{}", normalized_text);
        sigs.push(EvidenceSignature {
            level: EvidenceSignatureLevel::NormalizedDigest,
            digest: sha256_hex(&canonical),
            provenance,
            fact: None,
        });
    } else if !normalized_text.trim().is_empty()
        && !matches!(result, EvidenceResult::NoResult)
        && provenance == FactProvenance::Independent
        && classify_channel(&invocation.command, invocation.kind) != EvidenceChannel::Session
    {
        sigs.push(EvidenceSignature {
            level: EvidenceSignatureLevel::NormalizedDigest,
            digest: sha256_hex(&format!("v1|payload|{normalized_text}")),
            provenance,
            fact: None,
        });
    }
    sigs.sort();
    sigs.dedup();
    sigs
}

fn is_self_generated_session_fact(
    command: &str,
    channel: EvidenceChannel,
    task_literals: &[String],
    output: &str,
) -> bool {
    if is_self_transcript_search(command) {
        return true;
    }
    if channel != EvidenceChannel::Session {
        return false;
    }
    if task_literals.iter().any(|lit| command.contains(lit)) {
        return true;
    }
    task_literals
        .iter()
        .any(|lit| !lit.is_empty() && output.contains(lit))
}

fn is_high_confidence_session_path(normalized: &str) -> bool {
    normalized.contains("sessions/")
        || normalized.contains("/.codex")
        || normalized.contains("codex-home")
        || normalized.contains("codex_home")
        || (normalized.contains("rollout-") && normalized.contains(".jsonl"))
}

fn is_heuristic_session_probe(lower: &str) -> bool {
    if !is_python(lower) {
        return false;
    }
    let inspect_or_parse = lower.contains("inspect_")
        || lower.contains("inspect-")
        || lower.contains("parse_")
        || lower.contains("parse-");
    let session_store =
        lower.contains("session") || lower.contains("rollout") || lower.contains("transcript");
    inspect_or_parse && session_store
}

fn is_session_command(lower: &str) -> bool {
    let normalized = lower.replace('\\', "/");
    is_high_confidence_session_path(&normalized) || is_heuristic_session_probe(&normalized)
}

fn looks_like_database(lower: &str) -> bool {
    lower.contains(".sqlite")
        || lower.contains(".db")
        || lower.contains("sqlite3")
        || (lower.contains("select ") && lower.contains(" from "))
}

fn is_python(lower: &str) -> bool {
    let first = first_token(lower);
    first == "python" || first == "python3" || first == "py"
}

fn is_git_mutation(lower: &str) -> bool {
    if first_token(lower) != "git" {
        return false;
    }
    let tokens: Vec<&str> = lower.split_whitespace().collect();
    let sub = tokens.get(1).copied().unwrap_or("");
    matches!(
        sub,
        "commit"
            | "checkout"
            | "switch"
            | "reset"
            | "merge"
            | "rebase"
            | "cherry-pick"
            | "stash"
            | "branch"
            | "tag"
            | "add"
            | "rm"
            | "mv"
    )
}

fn is_search_like(command: &str, kind: CommandKind) -> bool {
    kind == CommandKind::Search
        || command.to_ascii_lowercase().contains("test-path")
        || (kind == CommandKind::Git && command.to_ascii_lowercase().contains(" grep"))
}

fn first_token(lower: &str) -> &str {
    lower
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_matches(|c| c == '"' || c == '\'')
}

fn bounded_descriptor(
    command: &str,
    operation: EvidenceOperationKind,
    channel: EvidenceChannel,
    subjects: &[CanonicalSubject],
) -> String {
    let subject_keys: Vec<&str> = subjects.iter().map(|s| s.key.as_str()).take(4).collect();
    let raw = format!(
        "{operation:?}/{channel:?} [{}] {}",
        subject_keys.join(","),
        command
    );
    raw.chars().take(240).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_normalization_is_separator_stable_and_root_conservative() {
        assert_eq!(normalize_file_subject(r".\DECISIONS.md"), "DECISIONS.md");
        assert_eq!(normalize_file_subject("DECISIONS.md"), "DECISIONS.md");
        assert_eq!(
            normalize_file_subject(r"C:\repo\DECISIONS.md"),
            "c:/repo/DECISIONS.md"
        );
        assert_ne!(
            normalize_file_subject(r"C:\repo-a\DECISIONS.md"),
            normalize_file_subject(r"C:\repo-b\DECISIONS.md")
        );
        assert_ne!(
            normalize_file_subject(r"C:\repo\DECISIONS.md"),
            normalize_file_subject(r"D:\repo\DECISIONS.md")
        );
        assert_eq!(
            normalize_file_subject("src/a/DECISIONS.md"),
            "src/a/DECISIONS.md"
        );
        assert_eq!(
            normalize_file_subject("src/b/DECISIONS.md"),
            "src/b/DECISIONS.md"
        );
    }

    #[test]
    fn python_rollout_is_session_channel_without_hardcoded_filename_only() {
        let cmd = r#"python parse.py $env:CODEX_HOME\sessions\2026\rollout-abc.jsonl"#;
        assert_eq!(
            classify_channel(cmd, CommandKind::Other),
            EvidenceChannel::Session
        );
        let workspace_jsonl = "python parse.py .\\fixtures\\events.jsonl";
        assert_eq!(
            classify_channel(workspace_jsonl, CommandKind::Other),
            EvidenceChannel::Workspace
        );
        assert_eq!(
            classify_channel("python inspect_latest_session.py", CommandKind::Other),
            EvidenceChannel::Session
        );
        assert_eq!(
            classify_channel("python session_migration.py", CommandKind::Other),
            EvidenceChannel::Workspace,
            "bare 'session' in a workspace script must not become a session store"
        );
        assert_eq!(
            classify_channel("Get-Content session_config.json", CommandKind::Read),
            EvidenceChannel::Workspace
        );
        assert_eq!(
            classify_channel("cargo test session_manager", CommandKind::Test),
            EvidenceChannel::Workspace
        );
    }

    #[test]
    fn session_output_echoing_task_literal_is_self_generated_without_command_literal() {
        let inv = CommandInvocation {
            source_index: 1,
            call_id: Some("c1".into()),
            command: "python inspect_latest_session.py".into(),
            kind: CommandKind::Other,
            exit_code: Some(0),
            output_hash: None,
            search_outcome: None,
        };
        let op = project_evidence_operation(
            &inv,
            "prompt: Implement normalize_records and preserve CIPHERTEXT_RECOVERY_EXACT",
            "occ:1",
            &["CIPHERTEXT_RECOVERY_EXACT".into(), "DECISIONS.md".into()],
            true,
        );
        assert_eq!(op.channel, EvidenceChannel::Session);
        assert!(op.stable_evidence.iter().all(|s| {
            s.provenance == FactProvenance::SelfGeneratedConversation
                || s.level != EvidenceSignatureLevel::NormalizedDigest
        }));
        assert!(
            !op.stable_evidence.iter().any(|s| {
                s.level == EvidenceSignatureLevel::NormalizedDigest
                    && s.provenance == FactProvenance::Independent
            }),
            "session transcript payload must not mint an independent content digest"
        );
    }

    #[test]
    fn git_log_is_vcs_query_not_mutation() {
        assert_eq!(
            classify_channel("git log -- DECISIONS.md", CommandKind::Git),
            EvidenceChannel::Vcs
        );
        assert_eq!(
            classify_operation_kind("git log -- DECISIONS.md", CommandKind::Git),
            EvidenceOperationKind::Query
        );
        assert!(!is_git_mutation("git log --oneline"));
        assert!(is_git_mutation("git commit -m msg"));
    }

    #[test]
    fn empty_envelope_projects_no_result_not_found() {
        let inv = CommandInvocation {
            source_index: 1,
            call_id: Some("c1".into()),
            command: "Select-String -Pattern needle".into(),
            kind: CommandKind::Search,
            exit_code: Some(0),
            output_hash: None,
            search_outcome: Some(SearchOutcome::NoMatch),
        };
        let op =
            project_evidence_operation(&inv, "Final output:\n", "occ:1", &["needle".into()], true);
        assert_eq!(op.result, EvidenceResult::NoResult);
        assert_eq!(op.channel, EvidenceChannel::Workspace);
        assert_eq!(op.operation, EvidenceOperationKind::Query);
    }

    #[test]
    fn permission_denied_is_error_not_no_result() {
        let inv = CommandInvocation {
            source_index: 1,
            call_id: Some("c1".into()),
            command: "Get-Content secret.txt".into(),
            kind: CommandKind::Read,
            exit_code: Some(1),
            output_hash: None,
            search_outcome: Some(SearchOutcome::Error),
        };
        let op = project_evidence_operation(
            &inv,
            "Permission denied: secret.txt",
            "occ:1",
            &["secret.txt".into()],
            true,
        );
        assert!(matches!(
            op.result,
            EvidenceResult::Error {
                family: ErrorFamily::PermissionDenied
            }
        ));
    }

    #[test]
    fn non_search_exit_one_is_error_not_negative_result() {
        let inv = CommandInvocation {
            source_index: 1,
            call_id: Some("c1".into()),
            command: "python inspect_state.py".into(),
            kind: CommandKind::Other,
            exit_code: Some(1),
            output_hash: None,
            search_outcome: None,
        };
        let op = project_evidence_operation(&inv, "", "occ:1", &["inspect_state.py".into()], true);
        assert!(matches!(
            op.result,
            EvidenceResult::Error {
                family: ErrorFamily::Other
            }
        ));
    }

    #[test]
    fn git_grep_exit_one_is_a_stable_negative_result() {
        let inv = CommandInvocation {
            source_index: 1,
            call_id: Some("c1".into()),
            command: "git grep MISSING_SYMBOL".into(),
            kind: CommandKind::Git,
            exit_code: Some(1),
            output_hash: None,
            search_outcome: Some(SearchOutcome::Error),
        };
        let op = project_evidence_operation(&inv, "", "occ:1", &["MISSING_SYMBOL".into()], true);
        assert_eq!(op.result, EvidenceResult::NoResult);
    }
}
