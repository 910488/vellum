//! 審查指令與唯讀的 runtime 壓縮觀測（doc/06、doc/07）。
//!
//! 指令只搬資料／轉錯誤。審查邏輯在 `crate::review`，
//! 壓縮預覽只讀既有 journal / Codex rollout，不會觸發或還原壓縮。

use crate::compaction::{
    build_item_comparison_preview, build_item_preview, classify_item, estimate_tokens,
    parse_structured_summary, ItemRole, StructuredSummary, DEFAULT_KEEP_RECENT_TURNS,
    OFFICIAL_SUMMARY_PREFIX,
};
use crate::error::{AppError, AppResult};
use crate::history::HistoryStore;
use crate::model::{
    CompactionPreview, CompactionSegment, Finding, ProviderKind, ReviewSettings,
    ReviewSettingsUpdate, ReviewStats,
};
use crate::probe::default_client;
use crate::review::{run_review_with_client, ReviewContext};
use crate::state::AppState;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use tauri::State;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionDetail {
    origin_available: bool,
    unavailable_reason: Option<String>,
    checkpoint_id: Option<String>,
    created_at: Option<i64>,
    trigger: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    source_label: Option<String>,
    canonical_kind: Option<String>,
    schema_version: Option<u32>,
    source_tokens: u64,
    canonical_tokens: Option<u64>,
    canonical_hash: Option<String>,
    exact_recovery: bool,
    portable_available: bool,
    encrypted_bytes: u64,
    official_mode: Option<&'static str>,
    canonical_items: Vec<CanonicalItemMetadata>,
    items: Vec<CompactionDetailItem>,
    summary: Option<StructuredSummary>,
}

/// One atomic read for the two halves of the redesigned Context screen.
/// Both values are derived from the same selected journal/client event, so a
/// compaction arriving between two filesystem scans cannot split the screen
/// across different events.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionSnapshot {
    preview: CompactionPreview,
    detail: CompactionDetail,
}

/// One handover: metadata for a compaction that happened in this conversation.
///
/// The Context screen lists the last few of these instead of dissecting one.
/// A single compaction cannot answer the question people actually have —
/// *how often is this conversation compacted, and by how much?* The endpoint
/// deliberately does not serialize the compaction contents.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionHandoff {
    id: String,
    created_at: i64,
    /// Who compacted: `Official`, `Local`, `LegacyRecovered`, `CodexClient`.
    kind: String,
    trigger: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    label: Option<String>,
    before_tokens: u64,
    /// `None` when the replacement window is opaque to Vellum — OpenAI
    /// ciphertext, or a compaction Codex Desktop performed on its own. A zero
    /// would read as "everything went", which is a different claim.
    after_tokens: Option<u64>,
}

struct LatestCompaction {
    journal: Option<crate::history::CompactionJournal>,
    client_event: Option<crate::codex::CodexCompactionEvent>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CanonicalItemMetadata {
    index: usize,
    item_type: String,
    id: Option<String>,
    encrypted_bytes: u64,
    sha256: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CompactionDetailItem {
    id: String,
    role: &'static str,
    disposition: &'static str,
    tokens: u64,
    text: Option<String>,
}

#[tauri::command]
pub fn get_review_settings(state: State<'_, AppState>) -> AppResult<ReviewSettings> {
    Ok(state.review_settings())
}

#[tauri::command]
pub fn set_review_settings(
    settings: ReviewSettings,
    state: State<'_, AppState>,
) -> AppResult<ReviewSettingsUpdate> {
    let canonical = state.set_review_settings(settings)?;
    let remote_hosts_pending_reapply =
        crate::remote::deployment::count_hosts_pending_review_reapply(&state) as u32;
    Ok(ReviewSettingsUpdate {
        settings: canonical,
        // The shared runtime always re-reads Auto Review policy from
        // `AppState` at the start of every request (`ReviewSettingsSource`),
        // so this is never provisional — the very next local Guardian
        // request already sees it, with no proxy restart.
        local_applied: true,
        remote_hosts_pending_reapply,
    })
}

#[tauri::command]
pub fn get_review_stats(state: State<'_, AppState>) -> AppResult<ReviewStats> {
    let mut stats = state.usage_store().review_stats()?;
    let names = state.route_display_names();
    for provider in &mut stats.providers {
        crate::commands::overview::resolve_provider_name(
            &names,
            &provider.route_id,
            &mut provider.provider,
        );
    }
    Ok(stats)
}

/// doc/06：跑一次自動審查。回排序去重後的 findings（沒問題就空）。
///
/// `content` 是要審的內容；`model` 空字串 → 用 ReviewSettings.model；
/// `endpoint` / `apiKey` 指定審查用的線路。
#[tauri::command(rename_all = "camelCase")]
pub async fn run_review(
    content: String,
    model: Option<String>,
    endpoint: Option<String>,
    api_key: Option<String>,
    state: State<'_, AppState>,
) -> AppResult<Vec<Finding>> {
    // model 為空 → 退到設定檔的 model；再空就用同一條線路的模型。
    let model = model
        .filter(|m| !m.trim().is_empty())
        .or_else(|| {
            let m = state.review_settings().model;
            if m.trim().is_empty() {
                None
            } else {
                Some(m)
            }
        })
        .unwrap_or_else(|| state.current_route().map(|r| r.model).unwrap_or_default());

    let client = default_client()?;
    if endpoint.as_deref().is_none_or(str::is_empty) && state.proxy_status().running {
        let settings = state.review_settings();
        let selected = state
            .model_routes()
            .into_iter()
            .find(|candidate| {
                (settings.route_id.is_empty() || candidate.route_id == settings.route_id)
                    && (model.is_empty()
                        || candidate.catalog_id == model
                        || candidate.upstream_model.eq_ignore_ascii_case(&model))
            })
            .ok_or_else(|| AppError::Message("找不到自動審查所選的 provider／模型".into()))?;
        return crate::review::run_review_via_proxy(
            &client,
            &content,
            &selected.catalog_id,
            &state.data_root(),
        )
        .await;
    }
    let ctx = ReviewContext {
        content,
        model,
        endpoint: endpoint.unwrap_or_default(),
        api_key,
    };
    run_review_with_client(&client, &ctx).await
}

/// doc/07：壓縮前的預覽。
#[tauri::command(rename_all = "camelCase")]
pub fn get_compaction_preview(
    session_id: Option<String>,
    state: State<'_, AppState>,
) -> AppResult<CompactionPreview> {
    crate::history::catch_history_panic(std::panic::AssertUnwindSafe(|| {
        preview_for_session(&state, session_id.as_deref())
    }))
}

/// Return both redesigned Context cards from one selected compaction event.
#[tauri::command(rename_all = "camelCase")]
pub async fn get_compaction_snapshot(
    session_id: Option<String>,
    state: State<'_, AppState>,
) -> AppResult<CompactionSnapshot> {
    let owned = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        crate::history::catch_history_panic(std::panic::AssertUnwindSafe(|| {
            compaction_snapshot_inner(&owned, session_id.as_deref())
        }))
    })
    .await
    .map_err(|error| AppError::Message(format!("compaction snapshot task failed: {error}")))?
}

/// One compaction, verbatim: the instruction that was sent and the text that
/// came back.
///
/// The Context screen's last card. `preview` answers *how much went*; this
/// answers *what the conversation is carrying now* — and only the raw text
/// answers that, because the quality of a summary is not visible in a token
/// count.
///
/// Both halves are `Option`, for different reasons, and the screen says which:
/// OpenAI keeps its replacement as ciphertext, and a compaction Codex Desktop
/// performed on its own never sent its instruction through the proxy. When
/// neither is readable, `unavailable_reason` must say so rather than leaving
/// two empty boxes that look like a bug.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionTranscript {
    checkpoint_id: Option<String>,
    created_at: Option<i64>,
    prompt: Option<String>,
    result: Option<String>,
    unavailable_reason: Option<String>,
}

/// The instruction and the replacement text for one conversation's newest
/// compaction. Codex Local 0.150 uses one vendored, immutable instruction and
/// stores the model's verbatim answer inside the final replacement summary
/// item, so both halves can be recovered without a second journal schema.
#[tauri::command(rename_all = "camelCase")]
pub async fn get_compaction_transcript(
    session_id: Option<String>,
    state: State<'_, AppState>,
) -> AppResult<CompactionTranscript> {
    let owned = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        crate::history::catch_history_panic(std::panic::AssertUnwindSafe(|| {
            compaction_transcript_inner(&owned, session_id.as_deref())
        }))
    })
    .await
    .map_err(|error| AppError::Message(format!("compaction transcript task failed: {error}")))?
}

fn compaction_transcript_inner(
    state: &AppState,
    session_id: Option<&str>,
) -> AppResult<CompactionTranscript> {
    let latest = read_latest_compaction(state, session_id)?;
    Ok(compaction_transcript_from_latest(&latest))
}

fn compaction_transcript_from_latest(latest: &LatestCompaction) -> CompactionTranscript {
    if let Some(event) = latest.client_event.as_ref() {
        let result = event.replacement_text.clone();
        return CompactionTranscript {
            checkpoint_id: Some(format!("codex-client-{}", event.event_id.unsigned_abs())),
            created_at: Some(event.created_at),
            prompt: None,
            result: result.clone(),
            unavailable_reason: result.is_none().then(|| "codexDesktopOpaque".into()),
        };
    }

    let Some(journal) = latest.journal.as_ref() else {
        return CompactionTranscript {
            checkpoint_id: None,
            created_at: None,
            prompt: None,
            result: None,
            unavailable_reason: Some("notCompacted".into()),
        };
    };

    if matches!(journal_shape(journal), JournalShape::ProviderOwned { .. }) {
        return CompactionTranscript {
            checkpoint_id: Some(journal.compaction_id.clone()),
            created_at: Some(journal.created_at),
            prompt: None,
            result: None,
            unavailable_reason: Some("officialOpaque".into()),
        };
    }

    let codex_local =
        journal.engine_id.as_deref() == Some(vellum_proxy_runtime::codex_local_v0_150::ENGINE_ID);
    let prompt = codex_local
        .then(|| vellum_proxy_runtime::codex_local_v0_150::SUMMARIZATION_PROMPT.to_string());
    let result = codex_local
        .then(|| codex_local_result(&journal.canonical_items))
        .flatten();
    let unavailable_reason = if prompt.is_some() && result.is_some() {
        None
    } else {
        Some("legacyUnreadable".into())
    };

    CompactionTranscript {
        checkpoint_id: Some(journal.compaction_id.clone()),
        created_at: Some(journal.created_at),
        prompt,
        result,
        unavailable_reason,
    }
}

fn codex_local_result(items: &[Value]) -> Option<String> {
    let prefix = format!(
        "{}\n",
        vellum_proxy_runtime::codex_local_v0_150::SUMMARY_PREFIX.trim_end_matches('\n')
    );
    items.iter().rev().find_map(|item| {
        vellum_proxy_runtime::compaction::item_text(item)
            .and_then(|text| text.strip_prefix(&prefix).map(str::to_string))
    })
}

/// The last few handovers in one conversation, newest first.
#[tauri::command(rename_all = "camelCase")]
pub async fn get_compaction_handoffs(
    session_id: Option<String>,
    limit: Option<u32>,
    state: State<'_, AppState>,
) -> AppResult<Vec<CompactionHandoff>> {
    let owned = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        crate::history::catch_history_panic(std::panic::AssertUnwindSafe(|| {
            compaction_handoffs_inner(&owned, session_id.as_deref(), limit.unwrap_or(3).min(3))
        }))
    })
    .await
    .map_err(|error| AppError::Message(format!("compaction handoff task failed: {error}")))?
}

fn compaction_handoffs_inner(
    state: &AppState,
    session_id: Option<&str>,
    limit: u32,
) -> AppResult<Vec<CompactionHandoff>> {
    let limit = limit.clamp(1, 3) as usize;
    let store = history_store(state)?;
    let journals = match session_id {
        Some(session_id) => store.recent_compactions_for_conversation(session_id, limit as u32)?,
        None => store.latest_compaction()?.into_iter().collect(),
    };
    let mut handoffs = journals
        .iter()
        .map(handoff_from_journal)
        .collect::<Vec<_>>();

    // Codex Desktop compacts some conversations by itself, outside the proxied
    // exchange, so those handovers exist only in its rollout. They are merged
    // in rather than shown separately: from the user's side it is one
    // conversation losing context, no matter which process did the losing.
    if let Some(session_id) = session_id {
        handoffs.extend(
            crate::codex::read_recent_compaction_events(64)
                .into_iter()
                .filter(|event| {
                    event.thread_id.as_deref().is_some_and(|thread| {
                        crate::codex::conversation_key_matches(session_id, thread)
                    })
                })
                .map(handoff_from_codex_client_event),
        );
    }

    handoffs.sort_by_key(|handoff| std::cmp::Reverse(handoff.created_at));
    handoffs.dedup_by(|a, b| a.id == b.id);
    handoffs.truncate(limit);
    Ok(handoffs)
}

fn handoff_from_journal(journal: &crate::history::CompactionJournal) -> CompactionHandoff {
    let (source, canonical) = match journal_shape(journal) {
        JournalShape::Comparison { source, canonical } => (source, canonical),
        JournalShape::ProviderOwned { source } => (source, &[] as &[Value]),
    };
    let provider_owned = matches!(journal_shape(journal), JournalShape::ProviderOwned { .. });
    CompactionHandoff {
        id: journal.compaction_id.clone(),
        created_at: journal.created_at,
        kind: if provider_owned {
            "Official".to_string()
        } else {
            format!("{:?}", journal.kind)
        },
        trigger: Some(journal.trigger.clone()),
        provider: journal.producer_provider.clone(),
        model: journal.producer_model.clone(),
        label: None,
        before_tokens: estimate_tokens(source),
        after_tokens: (!provider_owned).then(|| estimate_tokens(canonical)),
    }
}

fn handoff_from_codex_client_event(event: crate::codex::CodexCompactionEvent) -> CompactionHandoff {
    CompactionHandoff {
        id: format!("codex-client-{}", event.event_id.unsigned_abs()),
        created_at: event.created_at,
        kind: "CodexClient".into(),
        trigger: Some("codex_auto".into()),
        provider: Some("Codex Desktop".into()),
        model: None,
        label: event.label,
        before_tokens: event.tokens_before.unwrap_or_default(),
        after_tokens: event.tokens_after,
    }
}

/// Compatibility command for callers that only consume the detail card.
#[tauri::command(rename_all = "camelCase")]
pub async fn get_compaction_detail(
    session_id: Option<String>,
    state: State<'_, AppState>,
) -> AppResult<CompactionDetail> {
    // A bug decoding one stored journal must not crash the whole command —
    // contain the panic and report it as a clear history error.
    let owned = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        crate::history::catch_history_panic(std::panic::AssertUnwindSafe(|| {
            let latest = read_latest_compaction(&owned, session_id.as_deref())?;
            detail_from_latest(&owned, &latest)
        }))
    })
    .await
    .map_err(|error| AppError::Message(format!("compaction detail task failed: {error}")))?
}

fn read_latest_compaction(
    state: &AppState,
    session_id: Option<&str>,
) -> AppResult<LatestCompaction> {
    let store = history_store(state)?;
    let journal = match session_id {
        Some(session_id) => store.latest_compaction_for_conversation(session_id)?,
        None => store.latest_compaction()?,
    };
    let client_event = latest_codex_client_event_if_newer(journal.as_ref(), session_id);
    Ok(LatestCompaction {
        journal,
        client_event,
    })
}

fn compaction_snapshot_inner(
    state: &AppState,
    session_id: Option<&str>,
) -> AppResult<CompactionSnapshot> {
    let latest = read_latest_compaction(state, session_id)?;
    snapshot_from_latest(state, &latest)
}

fn snapshot_from_latest(
    state: &AppState,
    latest: &LatestCompaction,
) -> AppResult<CompactionSnapshot> {
    Ok(CompactionSnapshot {
        preview: preview_from_latest(latest),
        detail: detail_from_latest(state, latest)?,
    })
}

fn detail_from_latest(state: &AppState, latest: &LatestCompaction) -> AppResult<CompactionDetail> {
    if let Some(event) = latest.client_event.as_ref() {
        return Ok(CompactionDetail {
            origin_available: false,
            unavailable_reason: Some(
                "This compaction was performed inside Codex Desktop. Vellum can show when it happened and the observed token transition, but Codex did not expose the exact source and replacement items to the proxy."
                    .into(),
            ),
            checkpoint_id: Some(format!("codex-client-{}", event.event_id.unsigned_abs())),
            created_at: Some(event.created_at),
            trigger: Some("codex_auto".into()),
            provider: Some("Codex Desktop".into()),
            model: None,
            source_label: event.label.clone(),
            canonical_kind: Some("CodexClient".into()),
            schema_version: None,
            source_tokens: event.tokens_before.unwrap_or_default(),
            canonical_tokens: event.tokens_after,
            canonical_hash: None,
            exact_recovery: false,
            portable_available: false,
            encrypted_bytes: 0,
            official_mode: None,
            canonical_items: Vec::new(),
            items: Vec::new(),
            summary: None,
        });
    }
    let Some(journal) = latest.journal.as_ref() else {
        if state
            .current_route()
            .is_some_and(|route| route.provider_kind == ProviderKind::Official)
        {
            return Ok(CompactionDetail {
                origin_available: false,
                unavailable_reason: Some(
                    "官方 Codex 壓縮內容由 OpenAI 管理；目前沒有 Vellum 可顯示的本機壓縮紀錄。"
                        .into(),
                ),
                checkpoint_id: None,
                created_at: None,
                trigger: None,
                provider: None,
                model: None,
                source_label: None,
                canonical_kind: None,
                schema_version: None,
                source_tokens: 0,
                canonical_tokens: None,
                canonical_hash: None,
                exact_recovery: false,
                portable_available: false,
                encrypted_bytes: 0,
                official_mode: None,
                canonical_items: Vec::new(),
                items: Vec::new(),
                summary: None,
            });
        }
        return Ok(empty_compaction_detail());
    };
    // Which half of the row is which depends on who compacted. See
    // `JournalShape`.
    let (source, canonical) = match journal_shape(journal) {
        JournalShape::Comparison { source, canonical } => (source, canonical),
        JournalShape::ProviderOwned { source } => (source, &[] as &[Value]),
    };
    let provider_owned = matches!(journal_shape(journal), JournalShape::ProviderOwned { .. });
    let dispositions = detail_dispositions(source, canonical);
    let items = source
        .iter()
        .enumerate()
        .map(|(index, item)| CompactionDetailItem {
            id: item
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("item-{index}")),
            role: detail_role(item),
            disposition: dispositions[index],
            tokens: estimate_tokens(std::slice::from_ref(item)),
            text: (item.get("type").and_then(Value::as_str) != Some("reasoning"))
                .then(|| item_text(item))
                .flatten(),
        })
        .collect();

    let source_tokens = estimate_tokens(source);
    // OpenAI canonical state is opaque. Counting serialized ciphertext bytes
    // as tokens produces a large but meaningless number, so only plaintext
    // local/recovered windows expose a token estimate.
    let canonical_tokens = (!provider_owned).then(|| estimate_tokens(canonical));
    let canonical_kind = if provider_owned {
        // A row this Vellum wrote before the runtime flagged it still says
        // `Local`; the screen must name who actually compacted, not what the
        // column happens to hold.
        "Official".to_string()
    } else {
        format!("{:?}", journal.kind)
    };
    let encrypted_bytes = canonical
        .iter()
        .filter_map(|item| item.get("encrypted_content").and_then(Value::as_str))
        .map(|value| value.len() as u64)
        .sum();
    let canonical_items = canonical
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let encrypted_bytes = item
                .get("encrypted_content")
                .and_then(Value::as_str)
                .map(|value| value.len() as u64)
                .unwrap_or_default();
            CanonicalItemMetadata {
                index,
                item_type: item
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string(),
                id: item.get("id").and_then(Value::as_str).map(str::to_owned),
                encrypted_bytes,
                sha256: format!(
                    "{:x}",
                    Sha256::digest(serde_json::to_vec(item).unwrap_or_default())
                ),
            }
        })
        .collect();
    let official_mode =
        provider_owned.then_some(if journal.trigger == "official_context_management" {
            "server_side"
        } else {
            "standalone"
        });
    // A provider-owned row has no replacement window to summarise: the
    // replacement is the ciphertext, and `canonical_items` is the input.
    let summary = summary_from_replacement(canonical);
    Ok(CompactionDetail {
        origin_available: true,
        unavailable_reason: None,
        checkpoint_id: Some(journal.compaction_id.clone()),
        created_at: Some(journal.created_at),
        trigger: Some(journal.trigger.clone()),
        provider: journal.producer_provider.clone(),
        model: journal.producer_model.clone(),
        source_label: None,
        canonical_kind: Some(canonical_kind),
        schema_version: Some(journal.schema_version),
        source_tokens,
        canonical_tokens,
        canonical_hash: (!journal.canonical_sha256.is_empty())
            .then(|| journal.canonical_sha256.clone()),
        exact_recovery: provider_owned && journal.canonical_available,
        portable_available: journal.portable_items.is_some(),
        encrypted_bytes,
        official_mode,
        canonical_items,
        items,
        summary,
    })
}

/// What a journal row actually records.
///
/// The proxy writes two different things into the same table.
///
/// * A Vellum compaction stores both halves — the items it replaced
///   (`source_items`) and the window it installed in their place
///   (`canonical_items`).
/// * A provider compaction stores one half. OpenAI keeps the result as
///   ciphertext Vellum never reads, so what is journalled under
///   `canonical_items` is the *pre*-compaction history: that is the field
///   `materialize_local_compactions` expands the opaque marker back into when
///   a third-party route later meets the same conversation.
///
/// Reading the second shape with the first shape's rules reports the
/// compaction backwards — nothing before, the whole conversation after, and
/// 舊版 Vellum 壓縮紀錄 as the engine that did it. Since the Enhanced core
/// took over local compaction, the Official path is the only one still
/// writing journals on an ordinary install, so that was the Context screen's
/// answer for every recent compaction.
enum JournalShape<'a> {
    Comparison {
        source: &'a [Value],
        canonical: &'a [Value],
    },
    ProviderOwned {
        source: &'a [Value],
    },
}

/// Ids Vellum mints for its own compactions (`new_local_compaction_id`), and
/// the same prefix `materialize_local_compactions` uses to tell a Vellum
/// marker from a provider one.
const VELLUM_COMPACTION_PREFIX: &str = "cmp_vellum_";

fn journal_shape(journal: &crate::history::CompactionJournal) -> JournalShape<'_> {
    // `provider_owned` on the runtime record now becomes this kind. Rows
    // written before that flag existed still say `Local`, so they are matched
    // on shape instead, against an id the provider issued rather than one
    // Vellum minted.
    //
    // Two writers produced those rows. One left `source_items` empty; the
    // other stored the same window in both halves, because materialization
    // validates a journal row's hashes before installing it and an empty half
    // would fail. Neither is a before/after. A real Vellum compaction cannot
    // be mistaken for either: it mints a `cmp_vellum_` id, and a compaction
    // whose output equals its input is refused rather than journalled.
    let provider_issued_id = !journal.compaction_id.starts_with(VELLUM_COMPACTION_PREFIX);
    let legacy_provider_row = provider_issued_id
        && (journal.source_items.is_empty() || journal.source_items == journal.canonical_items);
    if journal.kind == crate::history::CompactionJournalKind::Official || legacy_provider_row {
        return JournalShape::ProviderOwned {
            source: &journal.canonical_items,
        };
    }
    JournalShape::Comparison {
        source: &journal.source_items,
        canonical: &journal.canonical_items,
    }
}

fn detail_dispositions<'a>(original: &[Value], replacement: &[Value]) -> Vec<&'a str> {
    let mut retained = replacement
        .iter()
        .filter(|item| {
            item_text(item)
                .as_deref()
                .is_none_or(|text| !text.starts_with(OFFICIAL_SUMMARY_PREFIX))
        })
        .filter_map(detail_item_key)
        .fold(HashMap::<String, usize>::new(), |mut counts, key| {
            *counts.entry(key).or_default() += 1;
            counts
        });

    // Match from newest to oldest. If two user messages have identical text,
    // the compact endpoint preserves the most recent one.
    let mut dispositions = original
        .iter()
        .rev()
        .map(|item| {
            if matches!(classify_item(item), ItemRole::System) {
                return "kept";
            }
            let Some(key) = detail_item_key(item) else {
                return "summarized";
            };
            let Some(count) = retained.get_mut(&key) else {
                return "summarized";
            };
            if *count == 0 {
                "summarized"
            } else {
                *count -= 1;
                "kept"
            }
        })
        .collect::<Vec<_>>();
    dispositions.reverse();
    dispositions
}

fn detail_item_key(item: &Value) -> Option<String> {
    if let Some(id) = item.get("id").and_then(Value::as_str) {
        return Some(format!("id:{id}"));
    }
    let role = item.get("role").and_then(Value::as_str).unwrap_or_default();
    let kind = item.get("type").and_then(Value::as_str).unwrap_or_default();
    item_text(item).map(|text| format!("{kind}\u{0}{role}\u{0}{text}"))
}

fn empty_compaction_detail() -> CompactionDetail {
    CompactionDetail {
        origin_available: true,
        unavailable_reason: None,
        checkpoint_id: None,
        created_at: None,
        trigger: None,
        provider: None,
        model: None,
        source_label: None,
        canonical_kind: None,
        schema_version: None,
        source_tokens: 0,
        canonical_tokens: None,
        canonical_hash: None,
        exact_recovery: false,
        portable_available: false,
        encrypted_bytes: 0,
        official_mode: None,
        canonical_items: Vec::new(),
        items: Vec::new(),
        summary: None,
    }
}

fn detail_role(item: &Value) -> &'static str {
    match classify_item(item) {
        ItemRole::System => "system",
        ItemRole::User => "user",
        ItemRole::Assistant => "assistant",
        ItemRole::Tool => "tool",
        ItemRole::Other => "other",
    }
}

fn item_text(item: &Value) -> Option<String> {
    if let Some(text) = item.as_str() {
        return Some(text.to_owned());
    }
    if let Some(text) = item.get("content").and_then(Value::as_str) {
        return Some(text.to_owned());
    }
    let text = item
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|part| {
            part.get("text")
                .and_then(Value::as_str)
                .or_else(|| part.get("output_text").and_then(Value::as_str))
                .or_else(|| part.get("input_text").and_then(Value::as_str))
        })
        .collect::<Vec<_>>()
        .join("\n");
    if !text.is_empty() {
        return Some(text);
    }
    item.get("output")
        .and_then(Value::as_str)
        .or_else(|| item.get("arguments").and_then(Value::as_str))
        .map(str::to_owned)
}

fn summary_from_replacement(items: &[Value]) -> Option<StructuredSummary> {
    items.iter().find_map(|item| {
        let text = item_text(item)?;
        let payload = text.strip_prefix(OFFICIAL_SUMMARY_PREFIX)?.trim();
        let value = serde_json::from_str(payload).ok()?;
        parse_structured_summary(&value)
    })
}

/// Derive the aggregate card from the exact event selected for the detail
/// card. Selection and filesystem access happen in `read_latest_compaction`.
fn preview_for_session(state: &AppState, session_id: Option<&str>) -> AppResult<CompactionPreview> {
    let latest = read_latest_compaction(state, session_id)?;
    Ok(preview_from_latest(&latest))
}

fn preview_from_latest(latest: &LatestCompaction) -> CompactionPreview {
    if let Some(event) = latest.client_event.as_ref() {
        return preview_from_codex_client_event(event);
    }
    if let Some(journal) = latest.journal.as_ref() {
        // Which half of the row is the "before" depends on who compacted.
        // See `JournalShape`.
        let (source, canonical) = match journal_shape(journal) {
            JournalShape::Comparison { source, canonical } => (source, Some(canonical)),
            JournalShape::ProviderOwned { source } => (source, None),
        };
        let mut preview = build_item_comparison_preview(
            source,
            &canonical
                .map(observable_canonical_items)
                .unwrap_or_default(),
            DEFAULT_KEEP_RECENT_TURNS,
        );
        // There is no plaintext "after" for a provider-owned compaction, so
        // the screen must say Opaque rather than draw an empty bar as if the
        // conversation had been reduced to nothing.
        preview.after_tokens_exact = canonical.is_some();
        preview.engine = match (canonical.is_some(), journal.kind) {
            (false, _) | (_, crate::history::CompactionJournalKind::Official) => {
                "official_canonical"
            }
            (_, crate::history::CompactionJournalKind::Local) => "local_canonical",
            (_, crate::history::CompactionJournalKind::LegacyRecovered) => "legacy_recovered",
        }
        .into();
        preview.readable_replay_tokens = preview
            .segments
            .iter()
            .find(|segment| segment.kind == "readable_reasoning")
            .map(|segment| u64::from(segment.after))
            .unwrap_or_default();
        preview.cross_session_tokens = journal
            .portable_items
            .as_deref()
            .map(estimate_tokens)
            .unwrap_or_default();
        preview.cross_session_available = journal
            .portable_items
            .as_ref()
            .is_some_and(|items| !items.is_empty());
        return preview;
    }
    // No journal means there is nothing to observe. Do not project current
    // proxy history through the retired Vellum compactor to manufacture a
    // hypothetical preview; the executing Codex runtime owns that decision.
    preview_from_items(&[])
}

/// Both cards on the Context screen must select the same newest compaction.
/// Keeping the provenance decision here prevents the aggregate preview from
/// drifting back to a proxy journal while the detail card shows a newer Codex
/// Desktop event.
fn latest_codex_client_event_if_newer(
    journal: Option<&crate::history::CompactionJournal>,
    session_id: Option<&str>,
) -> Option<crate::codex::CodexCompactionEvent> {
    select_codex_client_event(
        crate::codex::read_recent_compaction_events(if session_id.is_some() { 64 } else { 1 }),
        journal,
        session_id,
    )
}

fn select_codex_client_event(
    events: Vec<crate::codex::CodexCompactionEvent>,
    journal: Option<&crate::history::CompactionJournal>,
    session_id: Option<&str>,
) -> Option<crate::codex::CodexCompactionEvent> {
    events
        .into_iter()
        .find(|event| {
            session_id.is_none_or(|session_id| {
                event.thread_id.as_deref().is_some_and(|thread| {
                    crate::codex::conversation_key_matches(session_id, thread)
                })
            })
        })
        .filter(|event| journal.is_none_or(|journal| event.created_at > journal.created_at))
}

/// Codex Desktop reports only the aggregate token transition for its local
/// compaction. Keep that event on the same screen as its detail row instead of
/// leaving the preview pinned to an older proxy journal. The single segment is
/// intentionally named as an observed total: Codex did not expose enough data
/// to invent a Canonical/reasoning/tool/context breakdown.
fn preview_from_codex_client_event(
    event: &crate::codex::CodexCompactionEvent,
) -> CompactionPreview {
    let before = event.tokens_before.unwrap_or_default();
    let after = event.tokens_after.unwrap_or_default();
    let exact = event.tokens_before.is_some() && event.tokens_after.is_some();
    let clamp = |tokens: u64| tokens.min(u64::from(u32::MAX)) as u32;
    CompactionPreview {
        before_tokens: before,
        after_tokens: after,
        after_tokens_exact: exact,
        segments: if event.tokens_before.is_some() {
            vec![CompactionSegment {
                kind: "observed_total".into(),
                label: "Codex client observed window".into(),
                before: clamp(before),
                after: clamp(after),
                tone: "var(--sage)".into(),
            }]
        } else {
            Vec::new()
        },
        keep_recent_turns: 0,
        engine: "codex_client".into(),
        readable_replay_tokens: 0,
        cross_session_tokens: 0,
        cross_session_available: false,
    }
}

/// Return only the observable portion of a canonical window. Opaque
/// ciphertext is excluded from token estimates and is reported separately as
/// encrypted byte size in `CompactionDetail`.
fn observable_canonical_items(items: &[Value]) -> Vec<Value> {
    items
        .iter()
        .map(|item| {
            let mut item = item.clone();
            if let Some(object) = item.as_object_mut() {
                object.remove("encrypted_content");
            }
            item
        })
        .collect()
}

fn preview_from_items(items: &[serde_json::Value]) -> CompactionPreview {
    build_item_preview(items, DEFAULT_KEEP_RECENT_TURNS)
}

fn history_store(state: &AppState) -> AppResult<std::sync::Arc<HistoryStore>> {
    Ok(state.history_store())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compaction::summary_item;
    use serde_json::json;

    #[test]
    fn detail_text_reads_responses_content() {
        let item = json!({
            "type": "message",
            "role": "assistant",
            "content": [
                {"type": "output_text", "text": "first"},
                {"type": "output_text", "text": "second"}
            ]
        });
        assert_eq!(item_text(&item).as_deref(), Some("first\nsecond"));
        assert_eq!(detail_role(&item), "assistant");
    }

    #[test]
    fn detail_summary_round_trips_from_checkpoint_item() {
        let summary: StructuredSummary = serde_json::from_value(json!({
            "goal": "Finish wiring",
            "done": ["Registered the command"],
            "nextSteps": ["Run tests"]
        }))
        .unwrap();
        assert_eq!(
            summary_from_replacement(&[summary_item(&summary)]),
            Some(summary)
        );
    }

    #[test]
    fn detail_disposition_matches_the_actual_checkpoint_replacement() {
        let original = vec![
            json!({"role": "system", "content": "rules"}),
            json!({"role": "user", "content": "one"}),
            json!({"role": "assistant", "content": "one"}),
            json!({"role": "user", "content": "two"}),
            json!({"role": "assistant", "content": "two"}),
        ];
        let summary: StructuredSummary = serde_json::from_value(json!({
            "goal": "continue",
            "done": ["first turn"]
        }))
        .unwrap();
        let replacement = vec![original[3].clone(), summary_item(&summary)];
        assert_eq!(
            detail_dispositions(&original, &replacement),
            vec!["kept", "summarized", "summarized", "kept", "summarized"]
        );
    }

    #[test]
    fn duplicate_user_text_preserves_the_latest_matching_item() {
        let original = vec![
            json!({"role": "user", "content": "same"}),
            json!({"role": "assistant", "content": "first"}),
            json!({"role": "user", "content": "same"}),
        ];
        let replacement = vec![json!({"role": "user", "content": "same"})];
        assert_eq!(
            detail_dispositions(&original, &replacement),
            vec!["summarized", "summarized", "kept"]
        );
    }

    // 同一張表存兩種東西：Vellum 自己壓縮的存前後兩半，供應商壓縮的只存
    // 得到的那一半 —— 而且存在 `canonical_items` 欄，因為那是 replay 要
    // 拿回可讀歷史的地方。按前後兩半去讀第二種，畫面就會把壓縮講反。
    // 這幾個測試盯的是「哪一半是哪一半」，不是型別。

    fn provider_history() -> Vec<Value> {
        vec![
            json!({"role": "user", "content": "把 remote manager 的測試補完"}),
            json!({"role": "assistant", "content": "先把 route 投影逐欄位比對"}),
        ]
    }

    #[test]
    fn an_official_journal_is_read_as_input_plus_opaque_result() {
        let journal = crate::history::CompactionJournal {
            kind: crate::history::CompactionJournalKind::Official,
            compaction_id: "cmp_01278bbfabdeae75016a9a33b6e94887".into(),
            source_items: Vec::new(),
            canonical_items: provider_history(),
            ..sample_journal()
        };
        match journal_shape(&journal) {
            JournalShape::ProviderOwned { source } => {
                assert_eq!(
                    source.len(),
                    2,
                    "the stored window is what OpenAI compacted"
                )
            }
            JournalShape::Comparison { .. } => {
                panic!("an Official compaction has no readable replacement to compare against")
            }
        }
    }

    #[test]
    fn a_provider_row_written_before_the_flag_still_reads_forwards() {
        // What is already on disk: the desktop bridge stamped every runtime
        // journal `Local`, so an Official compaction is indistinguishable by
        // kind. It is still distinguishable by shape — no source items, and
        // an id OpenAI issued rather than one Vellum minted.
        let journal = crate::history::CompactionJournal {
            kind: crate::history::CompactionJournalKind::Local,
            compaction_id: "cmp_01278bbfabdeae75016a9a33b6e94887".into(),
            source_items: Vec::new(),
            canonical_items: provider_history(),
            ..sample_journal()
        };
        assert!(matches!(
            journal_shape(&journal),
            JournalShape::ProviderOwned { .. }
        ));
    }

    #[test]
    fn a_vellum_repair_row_is_not_mistaken_for_a_provider_one() {
        // The repair path also leaves `source_items` empty, and its
        // `canonical_items` really is a canonical window. It always mints a
        // `cmp_vellum_` id, which is the whole reason the legacy rule can key
        // on the prefix.
        let journal = crate::history::CompactionJournal {
            kind: crate::history::CompactionJournalKind::Local,
            compaction_id: "cmp_vellum_deadbeef".into(),
            source_items: Vec::new(),
            canonical_items: provider_history(),
            ..sample_journal()
        };
        assert!(matches!(
            journal_shape(&journal),
            JournalShape::Comparison { .. }
        ));
    }

    #[test]
    fn a_vellum_journal_still_compares_both_halves() {
        let journal = crate::history::CompactionJournal {
            kind: crate::history::CompactionJournalKind::Local,
            compaction_id: "cmp_vellum_1234".into(),
            source_items: provider_history(),
            canonical_items: vec![json!({"role": "assistant", "content": "摘要"})],
            ..sample_journal()
        };
        match journal_shape(&journal) {
            JournalShape::Comparison { source, canonical } => {
                assert_eq!(source.len(), 2);
                assert_eq!(canonical.len(), 1);
            }
            JournalShape::ProviderOwned { .. } => panic!("both halves are present"),
        }
    }

    #[test]
    fn the_context_screen_reads_an_official_compaction_forwards() {
        let temp = tempfile::tempdir().unwrap();
        let state = crate::state::AppState::with_data_dir(temp.path().to_path_buf());
        state
            .history_store()
            .save_canonical_compaction(
                "cmp_01278bbfabdeae75016a9a33b6e94887",
                // Exactly what the desktop bridge wrote before this fix.
                crate::history::CompactionJournalKind::Local,
                Vec::new(),
                provider_history(),
                None,
                "runtime",
                None,
                None,
                None,
            )
            .unwrap();

        let preview = preview_for_session(&state, None).unwrap();
        assert_eq!(
            preview.engine, "official_canonical",
            "OpenAI compacted this, not 舊版 Vellum 壓縮紀錄"
        );
        assert!(
            !preview.after_tokens_exact,
            "the replacement is ciphertext; the screen must say Opaque, not a number"
        );
        assert!(
            preview.before_tokens > 0,
            "the pre-compaction history is right there in the journal — reporting \
             0 before and the whole conversation after is the compaction backwards"
        );
    }

    /// A journal with only the two fields that have no serde default, so a
    /// test can state just the columns it is about.
    fn sample_journal() -> crate::history::CompactionJournal {
        serde_json::from_value(json!({
            "compaction_id": "cmp_placeholder",
            "created_at": 0,
        }))
        .expect("journal defaults")
    }

    #[test]
    fn official_canonical_preview_does_not_count_ciphertext_as_tokens() {
        let observable = observable_canonical_items(&[
            json!({
                "type": "compaction",
                "id": "cmp_test",
                "encrypted_content": "x".repeat(100_000)
            }),
            json!({"type": "message", "role": "user", "content": "retained"}),
        ]);
        let encoded = serde_json::to_string(&observable).unwrap();
        assert!(!encoded.contains(&"x".repeat(1_000)));
        assert_eq!(observable[0]["id"], "cmp_test");
        assert_eq!(observable[1]["content"], "retained");
        assert!(estimate_tokens(&observable) < 100);
    }

    #[test]
    fn codex_client_preview_uses_the_same_observed_transition_as_detail() {
        let preview = preview_from_codex_client_event(&crate::codex::CodexCompactionEvent {
            created_at: 1,
            event_id: 2,
            thread_id: None,
            label: Some("task".into()),
            tokens_before: Some(62_000),
            tokens_after: Some(4_000),
            replacement_text: None,
        });
        assert_eq!(preview.engine, "codex_client");
        assert!(preview.after_tokens_exact);
        assert_eq!(
            (preview.before_tokens, preview.after_tokens),
            (62_000, 4_000)
        );
        assert_eq!(preview.segments.len(), 1);
        assert_eq!(preview.segments[0].kind, "observed_total");
        assert_eq!(
            (preview.segments[0].before, preview.segments[0].after),
            (62_000, 4_000)
        );
    }

    #[test]
    fn compaction_handoff_wire_contains_metadata_only() {
        let handoff = handoff_from_codex_client_event(crate::codex::CodexCompactionEvent {
            created_at: 1,
            event_id: 2,
            thread_id: None,
            label: Some("task".into()),
            tokens_before: Some(62_000),
            tokens_after: Some(4_000),
            replacement_text: None,
        });
        let wire = serde_json::to_value(handoff).unwrap();
        assert_eq!(wire["beforeTokens"], 62_000);
        assert_eq!(wire["afterTokens"], 4_000);
        for content_field in ["summary", "summarizedItems", "keptItems", "items"] {
            assert!(wire.get(content_field).is_none(), "{content_field}");
        }
    }

    #[test]
    fn codex_local_transcript_recovers_the_exact_prompt_and_model_result() {
        let result = "Preserve the branch name and rerun the focused test.\nDo not rebuild.";
        let replacement = vellum_proxy_runtime::codex_local_v0_150::build_replacement_items(
            &provider_history(),
            result,
        );
        let latest = LatestCompaction {
            journal: Some(crate::history::CompactionJournal {
                compaction_id: "vcompact.codex0150.test".into(),
                kind: crate::history::CompactionJournalKind::Local,
                engine_id: Some(vellum_proxy_runtime::codex_local_v0_150::ENGINE_ID.into()),
                canonical_items: replacement,
                source_items: provider_history(),
                created_at: 42,
                ..sample_journal()
            }),
            client_event: None,
        };

        let transcript = compaction_transcript_from_latest(&latest);

        assert_eq!(
            transcript.prompt.as_deref(),
            Some(vellum_proxy_runtime::codex_local_v0_150::SUMMARIZATION_PROMPT)
        );
        assert_eq!(transcript.result.as_deref(), Some(result));
        assert_eq!(transcript.unavailable_reason, None);
        assert_eq!(
            transcript.checkpoint_id.as_deref(),
            Some("vcompact.codex0150.test")
        );
    }

    #[test]
    fn official_and_codex_client_compactions_explain_why_text_is_unavailable() {
        let official = LatestCompaction {
            journal: Some(crate::history::CompactionJournal {
                kind: crate::history::CompactionJournalKind::Official,
                compaction_id: "cmp_official".into(),
                canonical_items: provider_history(),
                created_at: 1,
                ..sample_journal()
            }),
            client_event: None,
        };
        let official_transcript = compaction_transcript_from_latest(&official);
        assert!(official_transcript.prompt.is_none());
        assert!(official_transcript.result.is_none());
        assert!(official_transcript
            .unavailable_reason
            .as_deref()
            .is_some_and(|reason| reason == "officialOpaque"));

        let client = LatestCompaction {
            journal: None,
            client_event: Some(crate::codex::CodexCompactionEvent {
                created_at: 2,
                event_id: 9,
                thread_id: None,
                label: None,
                tokens_before: Some(100),
                tokens_after: Some(20),
                replacement_text: None,
            }),
        };
        let client_transcript = compaction_transcript_from_latest(&client);
        assert!(client_transcript
            .unavailable_reason
            .as_deref()
            .is_some_and(|reason| reason == "codexDesktopOpaque"));
    }

    #[test]
    fn codex_client_transcript_exposes_rollout_replacement_text() {
        let latest = LatestCompaction {
            journal: None,
            client_event: Some(crate::codex::CodexCompactionEvent {
                created_at: 2,
                event_id: 9,
                thread_id: None,
                label: None,
                tokens_before: Some(100),
                tokens_after: Some(20),
                replacement_text: Some("persisted compact summary".into()),
            }),
        };
        let transcript = compaction_transcript_from_latest(&latest);
        assert_eq!(transcript.prompt, None);
        assert_eq!(
            transcript.result.as_deref(),
            Some("persisted compact summary")
        );
        assert_eq!(transcript.unavailable_reason, None);
    }

    #[test]
    fn context_snapshot_keeps_both_cards_on_one_codex_event() {
        let temp = tempfile::tempdir().unwrap();
        let state = crate::state::AppState::with_data_dir(temp.path().to_path_buf());
        let latest = LatestCompaction {
            journal: None,
            client_event: Some(crate::codex::CodexCompactionEvent {
                created_at: 42,
                event_id: 7,
                thread_id: None,
                label: Some("same task".into()),
                tokens_before: Some(62_000),
                tokens_after: Some(4_000),
                replacement_text: None,
            }),
        };
        let snapshot = snapshot_from_latest(&state, &latest).unwrap();

        assert_eq!(
            snapshot.preview.before_tokens,
            snapshot.detail.source_tokens
        );
        assert_eq!(
            Some(snapshot.preview.after_tokens),
            snapshot.detail.canonical_tokens
        );
        assert_eq!(snapshot.detail.created_at, Some(42));
        assert_eq!(
            snapshot.detail.checkpoint_id.as_deref(),
            Some("codex-client-7")
        );
        let wire = serde_json::to_value(snapshot).unwrap();
        assert_eq!(wire["preview"]["beforeTokens"], 62_000);
        assert_eq!(wire["detail"]["sourceTokens"], 62_000);
    }

    #[test]
    fn selected_session_ignores_a_newer_event_from_another_task() {
        // Live conversation keys look like this; the rollout file on disk
        // knows only the uuid, so the two are bound by alias, not equality.
        let thread_a = "01a079b3-33f3-7bb3-a4c9-e60261a5267d";
        let thread_b = "01a0756d-5919-77e0-9271-e847988268df";
        let event = |created_at, thread: &str| crate::codex::CodexCompactionEvent {
            created_at,
            event_id: created_at * 1_000,
            thread_id: Some(thread.into()),
            label: Some(thread.into()),
            tokens_before: Some(created_at as u64),
            tokens_after: Some(1),
            replacement_text: None,
        };
        let selected = select_codex_client_event(
            vec![event(20, thread_b), event(10, thread_a)],
            None,
            Some(&format!("codex:{thread_a}:{thread_a}")),
        )
        .expect("selected session event");
        assert_eq!(selected.thread_id.as_deref(), Some(thread_a));
        assert_eq!(selected.created_at, 10);
    }

    #[test]
    fn incomplete_codex_client_transition_never_claims_an_exact_after_size() {
        let preview = preview_from_codex_client_event(&crate::codex::CodexCompactionEvent {
            created_at: 1,
            event_id: 2,
            thread_id: None,
            label: None,
            tokens_before: Some(62_000),
            tokens_after: None,
            replacement_text: None,
        });
        assert!(!preview.after_tokens_exact);
        assert_eq!(preview.after_tokens, 0);
    }
}
