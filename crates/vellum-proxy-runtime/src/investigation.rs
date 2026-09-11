//! Vellum Canonical V2 deterministic investigation state & no-progress tracking.
//!
//! Models successful but zero-information-gain searches across tool exchanges,
//! forming bounded negative memory that survives compaction and prevents
//! infinite loops on identical investigation targets.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

use crate::evidence_operation::{CanonicalSubject, CanonicalSubjectKind, InvestigationPredicate};
use crate::investigation_linker::InvestigationIdentity;
use crate::investigation_reducer::{
    InvestigationLedgerEntry, InvestigationLedgerState, LinkProvenance, MaterialProgressState,
    INVESTIGATION_LEDGER_SCHEMA_VERSION,
};
use crate::tool_semantics::SearchOutcome;

/// Stable sha256 helper.
#[allow(dead_code)]
fn sha256_hex(payload: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(payload.as_bytes()))
}

/// Category of an investigation action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InvestigationKind {
    Search,
    ExistenceCheck,
    Read,
}

/// Outcome of a single investigation observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvestigationOutcome {
    Match,
    NoMatch,
    Error,
    Unknown,
}

impl From<SearchOutcome> for InvestigationOutcome {
    fn from(outcome: SearchOutcome) -> Self {
        match outcome {
            SearchOutcome::Match => InvestigationOutcome::Match,
            SearchOutcome::NoMatch => InvestigationOutcome::NoMatch,
            SearchOutcome::Error => InvestigationOutcome::Error,
            SearchOutcome::Unknown => InvestigationOutcome::Unknown,
        }
    }
}

fn default_information_gain() -> InformationGain {
    InformationGain::Unknown
}

/// Single observation of an executed investigation action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InvestigationObservation {
    pub target: String,
    pub kind: InvestigationKind,
    pub outcome: InvestigationOutcome,
    #[serde(default = "default_information_gain")]
    pub information_gain: InformationGain,
    pub source_index: usize,
    pub evidence_hash: Option<String>,
}

/// Information gain assessment for an investigation observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InformationGain {
    NewEvidence,
    NoNewEvidence,
    Unknown,
}

/// Aggregated no-progress state for a specific investigation target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NoProgressState {
    pub target: String,
    pub repeated_attempts: usize,
    pub last_outcome: InvestigationOutcome,
    pub information_gain: InformationGain,
    pub do_not_repeat_equivalent_search: bool,
}

fn default_investigation_schema_version() -> u32 {
    0
}

/// Bounded collection of investigation observations, no-progress states, and V3 ledgers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InvestigationState {
    /// 0 = legacy V2 payload; 3 = Investigation Ledger.
    #[serde(default = "default_investigation_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub ledgers: Vec<InvestigationLedgerEntry>,
    #[serde(default)]
    pub progress: MaterialProgressState,
    #[serde(default)]
    pub next_seq: u64,
    #[serde(default)]
    pub no_progress: Vec<NoProgressState>,
    #[serde(default)]
    pub recent_observations: Vec<InvestigationObservation>,
    #[serde(default)]
    pub task_stall: crate::task_stall::TaskStallState,
}

impl Default for InvestigationState {
    fn default() -> Self {
        Self {
            schema_version: INVESTIGATION_LEDGER_SCHEMA_VERSION,
            ledgers: Vec::new(),
            progress: MaterialProgressState::default(),
            next_seq: 0,
            no_progress: Vec::new(),
            recent_observations: Vec::new(),
            task_stall: crate::task_stall::TaskStallState::default(),
        }
    }
}

impl InvestigationState {
    pub fn from_ledger(ledger: InvestigationLedgerState) -> Self {
        Self {
            schema_version: INVESTIGATION_LEDGER_SCHEMA_VERSION,
            ledgers: ledger.ledgers,
            progress: ledger.progress,
            next_seq: ledger.next_seq,
            no_progress: Vec::new(),
            recent_observations: Vec::new(),
            task_stall: ledger.task_stall,
        }
    }

    pub fn as_ledger(&self) -> InvestigationLedgerState {
        InvestigationLedgerState {
            schema_version: self.schema_version.max(INVESTIGATION_LEDGER_SCHEMA_VERSION),
            ledgers: self.ledgers.clone(),
            progress: self.progress.clone(),
            next_seq: self.next_seq,
            task_stall: self.task_stall.clone(),
        }
    }

    /// Conservative V2 → V3 migration. Legacy literal targets never gain hard-recovery authority.
    pub fn migrate_legacy(&mut self) {
        self.task_stall.migrate();
        if !self.ledgers.is_empty() {
            if self.schema_version < INVESTIGATION_LEDGER_SCHEMA_VERSION {
                self.schema_version = INVESTIGATION_LEDGER_SCHEMA_VERSION;
            }
            return;
        }
        if self.no_progress.is_empty() {
            self.schema_version = INVESTIGATION_LEDGER_SCHEMA_VERSION;
            return;
        }
        for (i, np) in self.no_progress.iter().enumerate() {
            let subject = legacy_subject(&np.target);
            let identity = InvestigationIdentity {
                predicate: InvestigationPredicate::Locate,
                subjects: vec![subject],
                task_revision: self.progress.user_instruction_revision,
            };
            self.ledgers.push(InvestigationLedgerEntry {
                ledger_id: format!("inv:legacy:{i}"),
                identity,
                channels_seen: BTreeSet::new(),
                depends_on: BTreeSet::new(),
                evidence_frontier: Vec::new(),
                attempts_without_frontier_progress: np.repeated_attempts.min(2) as u32,
                redundant_revisits: 0,
                last_seen_progress: Default::default(),
                recovery_count: 0,
                recovery_state: crate::investigation_reducer::RecoveryState::None,
                last_activity_seq: i as u64,
                link_provenance: LinkProvenance::LegacyLiteral,
                bounded_descriptor: np.target.chars().take(160).collect(),
                resolved: false,
            });
        }
        self.schema_version = INVESTIGATION_LEDGER_SCHEMA_VERSION;
    }
}

fn legacy_subject(target: &str) -> CanonicalSubject {
    if target.contains('.') {
        CanonicalSubject::new(CanonicalSubjectKind::File, target.to_string())
    } else {
        CanonicalSubject::new(CanonicalSubjectKind::TaskLiteral, target.to_string())
    }
}

/// Extract salient task literal tokens from prompt/instructions (e.g. UPPER_SNAKE_CASE, filenames, quoted items).
pub fn extract_task_literals(text: &str) -> Vec<String> {
    let mut literals = BTreeSet::new();

    // 1. Backtick literals `...`
    let mut in_backtick = false;
    let mut current_literal = String::new();
    for ch in text.chars() {
        if ch == '`' {
            if in_backtick {
                let trimmed = current_literal.trim();
                if !trimmed.is_empty() && trimmed.len() <= 64 {
                    literals.insert(trimmed.to_string());
                }
                current_literal.clear();
                in_backtick = false;
            } else {
                in_backtick = true;
            }
        } else if in_backtick {
            current_literal.push(ch);
        }
    }

    // 2. Tokenize by whitespace and punctuation for UPPER_SNAKE_CASE or filenames
    for token in text.split(|c: char| {
        c.is_whitespace() || c == ',' || c == ';' || c == '(' || c == ')' || c == '[' || c == ']'
    }) {
        let trimmed = token.trim_matches(|c: char| c == '"' || c == '\'' || c == '`' || c == '.');
        let is_upper_snake = trimmed.len() >= 3
            && trimmed
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
            && trimmed.contains('_');
        let is_filename = (trimmed.ends_with(".md")
            || trimmed.ends_with(".py")
            || trimmed.ends_with(".rs")
            || trimmed.ends_with(".txt")
            || trimmed.ends_with(".json"))
            && !trimmed.contains('/')
            && !trimmed.contains('\\');
        if is_upper_snake || is_filename {
            literals.insert(trimmed.to_string());
        }
    }

    literals.into_iter().collect()
}

/// Extract target identifiers from a command line using task literals and pattern arguments.
pub fn extract_investigation_targets(command: &str, task_literals: &[String]) -> Vec<String> {
    let mut targets = BTreeSet::new();

    // 1. Check known task literals present in command line
    for lit in task_literals {
        if command.contains(lit) {
            if let Some(target) = normalize_target(lit) {
                targets.insert(target);
            }
        }
    }

    // 2. Check flags like -Filter, -Pattern, rg <pattern>, grep <pattern>
    let tokens: Vec<&str> = command.split_whitespace().collect();
    for i in 0..tokens.len() {
        let tok = tokens[i];
        let tok_lower = tok.to_ascii_lowercase();
        if tok_lower == "-filter" || tok_lower == "-pattern" {
            if let Some(next) = tokens.get(i + 1) {
                let clean = next.trim_matches(|c| c == '\'' || c == '"');
                if !clean.is_empty() {
                    if let Some(target) = normalize_target(clean) {
                        targets.insert(target);
                    }
                }
            }
        } else if (tok == "rg" || tok == "grep" || tok == "ripgrep") && i + 1 < tokens.len() {
            // Find first non-flag argument after command
            for next in &tokens[i + 1..] {
                if !next.starts_with('-') {
                    let clean = next.trim_matches(|c| c == '\'' || c == '"');
                    if !clean.is_empty() {
                        if let Some(target) = normalize_target(clean) {
                            targets.insert(target);
                        }
                        break;
                    }
                }
            }
        }
    }

    // 3. Fallback: extract filenames ending in common extensions
    for tok in &tokens {
        let clean = tok.trim_matches(|c| c == '\'' || c == '"' || c == '`');
        if (clean.ends_with(".md")
            || clean.ends_with(".py")
            || clean.ends_with(".txt")
            || clean.ends_with(".json"))
            && !clean.contains('/')
            && !clean.contains('\\')
        {
            if let Some(target) = normalize_target(clean) {
                targets.insert(target);
            }
        }
    }

    targets.into_iter().collect()
}

fn normalize_target(target: &str) -> Option<String> {
    let normalized = target
        .chars()
        .filter(|ch| !ch.is_control() && *ch != '`')
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let bounded = normalized.chars().take(128).collect::<String>();
    (!bounded.is_empty()).then_some(bounded)
}

/// A hit in Codex's own rollout transcript only proves that the model already
/// saw the task literal. It is not independent workspace evidence.
pub fn is_self_transcript_search(command: &str) -> bool {
    let lower = command.to_ascii_lowercase().replace('\\', "/");
    lower.contains("sessions/")
        && (lower.contains("rollout-")
            || (lower.contains(".jsonl")
                && (lower.contains("codex-home")
                    || lower.contains("codex_home")
                    || lower.contains(".codex"))))
}

/// Determine information gain for an investigation observation.
pub fn determine_information_gain(
    target: &str,
    outcome: InvestigationOutcome,
    prior_observations: &[InvestigationObservation],
    evidence_hash: Option<&str>,
    _workspace_mutated: bool,
    _test_state_changed: bool,
) -> InformationGain {
    match outcome {
        InvestigationOutcome::Match => {
            let prior_match = prior_observations.iter().any(|obs| {
                obs.target.eq_ignore_ascii_case(target)
                    && obs.outcome == InvestigationOutcome::Match
                    && obs.evidence_hash.as_deref() == evidence_hash
            });
            if prior_match {
                InformationGain::NoNewEvidence
            } else {
                InformationGain::NewEvidence
            }
        }
        InvestigationOutcome::NoMatch => {
            if prior_observations
                .iter()
                .any(|obs| obs.target.eq_ignore_ascii_case(target))
            {
                InformationGain::NoNewEvidence
            } else {
                InformationGain::Unknown
            }
        }
        InvestigationOutcome::Error | InvestigationOutcome::Unknown => InformationGain::Unknown,
    }
}

/// Update no-progress state list with new observations.
/// Update no-progress state list with new observations, respecting progress event ordering.
/// If progress occurred (file mutation or test state change), existing targets are reopened
/// for one probe (`do_not_repeat = false`) without wiping historical attempt counts.
/// If a search took place AFTER the latest progress event, that probe was executed and failed,
/// so reaching the threshold re-locks `do_not_repeat = true`.
/// If the progress event occurred AFTER the search, the target has not yet been probed against
/// the new state, so it remains OPEN (`do_not_repeat = false`).
pub fn update_no_progress_states_with_ordering(
    current: &[NoProgressState],
    observations: &[InvestigationObservation],
    last_progress_source_index: Option<usize>,
    repeat_threshold: usize,
) -> Vec<NoProgressState> {
    let mut state_map: BTreeMap<String, NoProgressState> = BTreeMap::new();

    // Carry forward current non-resolved states
    for s in current {
        let mut carried = s.clone();
        // If workspace or test state progressed in current window, reopen target for ONE probe
        // without erasing historical negative memory.
        if last_progress_source_index.is_some() {
            carried.do_not_repeat_equivalent_search = false;
        }
        state_map.insert(carried.target.to_ascii_lowercase(), carried);
    }

    // Process new observations driven authoritatively by information gain, outcome, and temporal ordering
    for obs in observations {
        let key = obs.target.to_ascii_lowercase();
        if obs.information_gain == InformationGain::NewEvidence {
            // Match found: resolve/remove no-progress for this target
            state_map.remove(&key);
        } else if matches!(
            obs.outcome,
            InvestigationOutcome::NoMatch | InvestigationOutcome::Match
        ) && obs.information_gain == InformationGain::NoNewEvidence
        {
            // Confirmed repeated search with zero information gain: increment counter
            let entry = state_map
                .entry(key.clone())
                .or_insert_with(|| NoProgressState {
                    target: obs.target.clone(),
                    repeated_attempts: 0,
                    last_outcome: InvestigationOutcome::NoMatch,
                    information_gain: InformationGain::NoNewEvidence,
                    do_not_repeat_equivalent_search: false,
                });
            entry.repeated_attempts += 1;
            entry.last_outcome = obs.outcome;
            entry.information_gain = InformationGain::NoNewEvidence;

            // Temporal ordering check:
            // If progress occurred in this window:
            // - Search executed AFTER progress: consumed the probe! If threshold reached, re-lock.
            // - Progress occurred AFTER search: target has not yet been probed against new state, remains open.
            if let Some(progress_idx) = last_progress_source_index {
                if obs.source_index > progress_idx {
                    if entry.repeated_attempts >= repeat_threshold {
                        entry.do_not_repeat_equivalent_search = true;
                    }
                } else {
                    entry.do_not_repeat_equivalent_search = false;
                }
            } else if entry.repeated_attempts >= repeat_threshold {
                entry.do_not_repeat_equivalent_search = true;
            }
        } else if obs.outcome == InvestigationOutcome::NoMatch
            && obs.information_gain == InformationGain::Unknown
        {
            // Preserve the first observation without claiming information
            // gain. A later equivalent observation can then be identified as
            // zero-gain and increment this bounded history.
            state_map.entry(key).or_insert_with(|| NoProgressState {
                target: obs.target.clone(),
                repeated_attempts: 1,
                last_outcome: InvestigationOutcome::NoMatch,
                information_gain: InformationGain::Unknown,
                do_not_repeat_equivalent_search: false,
            });
        }
        // Other unknown/error observations leave state conservatively unchanged.
    }

    // Return bounded list (at most 8 targets)
    state_map.into_values().take(8).collect()
}

pub fn update_no_progress_states(
    current: &[NoProgressState],
    observations: &[InvestigationObservation],
    workspace_mutated: bool,
    test_state_changed: bool,
    repeat_threshold: usize,
) -> Vec<NoProgressState> {
    let last_progress_idx = if workspace_mutated || test_state_changed {
        Some(0)
    } else {
        None
    };
    update_no_progress_states_with_ordering(
        current,
        observations,
        last_progress_idx,
        repeat_threshold,
    )
}

/// Format bounded investigation state into model-visible Markdown sections.
pub fn format_investigation_markdown(state: &InvestigationState) -> String {
    let mut sections = Vec::new();

    if !state.ledgers.is_empty() {
        let mut known = Vec::new();
        let mut missing = Vec::new();
        let mut do_not = Vec::new();
        for entry in &state.ledgers {
            if entry.resolved {
                continue;
            }
            let subjects: Vec<&str> = entry
                .identity
                .subjects
                .iter()
                .map(|s| s.key.as_str())
                .collect();
            let channels: Vec<String> = entry
                .channels_seen
                .iter()
                .map(|c| format!("{c:?}"))
                .collect();
            if !entry.evidence_frontier.is_empty() {
                for fact in entry
                    .evidence_frontier
                    .iter()
                    .filter_map(|s| s.fact.as_deref())
                    .take(3)
                {
                    known.push(format!("- {fact}"));
                }
            } else if !subjects.is_empty() {
                known.push(format!(
                    "- `{}`: explored {} without independent confirmation.",
                    subjects.join(", "),
                    if channels.is_empty() {
                        "prior sources".into()
                    } else {
                        channels.join("/")
                    }
                ));
            }
            if !subjects.is_empty() {
                missing.push(format!(
                    "- independent evidence for `{}`",
                    subjects.join(", ")
                ));
            }
            if entry.redundant_revisits > 0
                || entry.recovery_state != crate::investigation_reducer::RecoveryState::None
            {
                do_not.push(format!(
                    "- Do not repeat equivalent investigation of `{}` on already-explored sources unless new independent evidence appears.",
                    subjects.join(", ")
                ));
            }
        }
        if !known.is_empty() {
            sections.push(format!("## Investigation State\n\n{}", known.join("\n")));
        }
        if !missing.is_empty() {
            sections.push(format!("## Still Missing\n\n{}", missing.join("\n")));
        }
        if !do_not.is_empty() {
            sections.push(format!(
                "## Do Not Repeat Equivalent Investigation\n\n{}",
                do_not.join("\n")
            ));
        }
        if !sections.is_empty() {
            return sections.join("\n\n");
        }
    }

    let mut state_lines = Vec::new();
    for np in &state.no_progress {
        match np.last_outcome {
            InvestigationOutcome::NoMatch => {
                if np.repeated_attempts > 1 {
                    state_lines.push(format!(
                        "- `{}`: repeated equivalent searches ({} attempts) produced no new evidence.",
                        np.target, np.repeated_attempts
                    ));
                } else {
                    state_lines.push(format!("- `{}`: not found in prior search.", np.target));
                }
            }
            InvestigationOutcome::Match
                if np.information_gain == InformationGain::NoNewEvidence =>
            {
                state_lines.push(format!(
                    "- `{}`: transcript matches repeated already-known task text without new workspace evidence ({} attempts).",
                    np.target, np.repeated_attempts
                ));
            }
            _ => {}
        }
    }

    if !state_lines.is_empty() {
        sections.push(format!(
            "## Investigation State

{}",
            state_lines.join(
                "
"
            )
        ));
    }

    let mut do_not_repeat_lines = Vec::new();
    for np in &state.no_progress {
        if np.do_not_repeat_equivalent_search {
            do_not_repeat_lines.push(format!(
                "- Do not search session/workspace again for `{}` unless new evidence appears.",
                np.target
            ));
        }
    }

    if !do_not_repeat_lines.is_empty() {
        sections.push(format!(
            "## Do Not Repeat Equivalent Investigation

{}",
            do_not_repeat_lines.join(
                "
"
            )
        ));
    }

    sections.join(
        "

",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_task_literals_extracts_upper_snake_case_and_files() {
        let prompt =
            "Implement normalize_records and preserve CIPHERTEXT_RECOVERY_EXACT in DECISIONS.md.";
        let literals = extract_task_literals(prompt);
        assert!(literals.contains(&"CIPHERTEXT_RECOVERY_EXACT".to_string()));
        assert!(literals.contains(&"DECISIONS.md".to_string()));
        assert!(literals.contains(&"normalize_records".to_string()));
    }

    #[test]
    fn extract_investigation_targets_extracts_matching_literals_and_patterns() {
        let literals = vec!["CIPHERTEXT_RECOVERY_EXACT".into(), "DECISIONS.md".into()];
        let cmd = "Select-String -Path . -Pattern CIPHERTEXT_RECOVERY_EXACT";
        let targets = extract_investigation_targets(cmd, &literals);
        assert!(targets.contains(&"CIPHERTEXT_RECOVERY_EXACT".to_string()));

        let cmd2 = "Get-ChildItem -Recurse -Filter DECISIONS.md";
        let targets2 = extract_investigation_targets(cmd2, &literals);
        assert!(targets2.contains(&"DECISIONS.md".to_string()));

        let hostile = format!("Select-String -Pattern '{} ` injected'", "x".repeat(200));
        let bounded = extract_investigation_targets(&hostile, &[]);
        assert!(bounded.iter().all(|target| target.chars().count() <= 128));
        assert!(bounded.iter().all(|target| !target.contains('`')));
    }

    #[test]
    fn first_no_match_is_unknown_and_repeat_has_no_new_evidence() {
        let first = determine_information_gain(
            "needle",
            InvestigationOutcome::NoMatch,
            &[],
            None,
            false,
            false,
        );
        assert_eq!(first, InformationGain::Unknown);
        let prior = InvestigationObservation {
            target: "needle".into(),
            kind: InvestigationKind::Search,
            outcome: InvestigationOutcome::NoMatch,
            information_gain: first,
            source_index: 1,
            evidence_hash: None,
        };
        assert_eq!(
            determine_information_gain(
                "needle",
                InvestigationOutcome::NoMatch,
                &[prior],
                None,
                false,
                false,
            ),
            InformationGain::NoNewEvidence,
        );
    }

    #[test]
    fn identifies_codex_rollout_self_search_without_matching_normal_workspace_jsonl() {
        assert!(is_self_transcript_search(
            "Select-String -Path $env:CODEX_HOME\\sessions\\2026\\rollout-abc.jsonl -Pattern needle"
        ));
        assert!(!is_self_transcript_search(
            "Select-String -Path .\\fixtures\\events.jsonl -Pattern needle"
        ));
    }

    #[test]
    fn no_progress_state_promotes_do_not_repeat_at_threshold() {
        let obs1 = InvestigationObservation {
            target: "CIPHERTEXT_RECOVERY_EXACT".into(),
            kind: InvestigationKind::Search,
            outcome: InvestigationOutcome::NoMatch,
            information_gain: InformationGain::NoNewEvidence,
            source_index: 0,
            evidence_hash: None,
        };
        let mut states = update_no_progress_states(&[], &[obs1.clone()], false, false, 3);
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].repeated_attempts, 1);
        assert!(!states[0].do_not_repeat_equivalent_search);

        states = update_no_progress_states(&states, &[obs1.clone()], false, false, 3);
        assert_eq!(states[0].repeated_attempts, 2);
        assert!(!states[0].do_not_repeat_equivalent_search);

        states = update_no_progress_states(&states, &[obs1.clone()], false, false, 3);
        assert_eq!(states[0].repeated_attempts, 3);
        assert!(
            states[0].do_not_repeat_equivalent_search,
            "Must promote do_not_repeat at threshold 3"
        );
    }

    #[test]
    fn match_resolves_no_progress() {
        let initial = vec![NoProgressState {
            target: "CIPHERTEXT_RECOVERY_EXACT".into(),
            repeated_attempts: 3,
            last_outcome: InvestigationOutcome::NoMatch,
            information_gain: InformationGain::NoNewEvidence,
            do_not_repeat_equivalent_search: true,
        }];
        let match_obs = InvestigationObservation {
            target: "CIPHERTEXT_RECOVERY_EXACT".into(),
            kind: InvestigationKind::Search,
            outcome: InvestigationOutcome::Match,
            information_gain: InformationGain::NewEvidence,
            source_index: 5,
            evidence_hash: Some("sha256:abc".into()),
        };
        let updated = update_no_progress_states(&initial, &[match_obs], false, false, 3);
        assert!(
            updated.is_empty(),
            "Finding a match must resolve no-progress state"
        );
    }
}
