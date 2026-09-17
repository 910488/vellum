#![cfg_attr(
    test,
    allow(
        dead_code,
        unused_mut,
        clippy::await_holding_lock,
        clippy::bool_assert_comparison,
        clippy::cloned_ref_to_slice_refs,
        clippy::field_reassign_with_default,
        clippy::len_zero,
        clippy::useless_vec
    )
)]

//! Reusable Vellum proxy runtime.
//!
//! Desktop (Tauri) and the headless daemon both assemble servers through this
//! crate so proxy semantics are not forked into a second implementation.

pub mod adapter;
pub mod auth;
pub mod auto_continue;
pub mod body;
pub mod codex_local_v0_150;
pub mod codex_metadata;
pub mod compaction;
pub mod compaction_engine;
pub mod config;
pub mod context_projection;
pub mod continuation;
pub mod credentials;
pub mod diagnostics;
pub mod environment;
pub mod error;
pub mod evidence_operation;
pub mod exec;
pub mod grok_session;
pub mod harness;
pub mod history;
pub mod identity;
pub mod inbound;
pub mod investigation;
pub mod investigation_diagnostics;
pub mod investigation_linker;
pub mod investigation_reducer;
pub mod investigation_replay;
pub mod investigation_runtime;
pub mod lifecycle;
pub mod live_attribution;
pub mod official_auth;
pub mod official_identity;
pub mod opencode;
pub mod outbound;
pub mod profile_adapter;
pub mod progress_fingerprint;
pub mod recovery_policy;
pub mod recovery_snapshot;
pub mod replay;
pub mod request;
pub mod resource;
pub mod response;
pub mod review;
pub mod route;
pub mod runtime;
pub mod search;
pub mod search_loop;
pub mod server;
pub mod snapshot;
pub mod sse;
pub mod stall_recovery;
pub mod state;
pub mod streaming;
pub mod subagent_graph;
pub mod task_efficiency;
pub mod task_stall;
pub mod tool_output_normalizer;
pub mod tool_result_pruner;
pub mod tool_semantics;
pub mod trajectory;
pub mod transport;
pub mod usage;
pub mod websocket;

pub use adapter::{
    chat_reasoning_text, chat_response_to_responses, chat_response_to_responses_with_context,
    contains_vellum_synthetic_item, content_to_plain_string, content_to_text, custom_tool_input,
    flatten_namespace_name, is_apply_patch, normalize_official_reasoning_summary,
    normalize_patch_delimiters, prepare_openai_official_native, sanitize_for_official,
    strip_cross_realm_fields, strip_cross_realm_fields_for_official,
    strip_opaque_reasoning_keep_summary, validate_patch, ChatSseAdapter, CodexToolContext,
    FileChange, FileChangeKind, NamespaceToolContext, PatchSummary, APPLY_PATCH_TOOL_NAME,
};
pub use auth::ResolvedAuth;
pub use body::{
    decode_request_body, decode_request_body_with_limit, error_envelope, models_list_payload,
    parse_json_request, request_body_error_response, ModelListEntry, RequestBodyError,
    MAX_REQUEST_BODY_BYTES,
};
pub use codex_metadata::{
    extract_codex_turn_identity, parse_canonical_client_metadata, parse_codex_opaque_id,
    parse_compatibility_projection, parse_flat_body_compatibility, parse_flat_header_compatibility,
    parse_turn_metadata_header, resolve_codex_capabilities, schema_fingerprint,
    CodexCapabilityProfile, CodexCompatibilityProjection, CodexIdentitySource, CodexIdentityTrust,
    CodexMetadataError, CodexMetadataHeaderScope, CodexMetadataSources, CodexOpaqueId,
    CodexReleaseChannel, CodexSupportLevel, CodexTurnIdentity, CodexTurnMetadataWire,
    MAX_CODEX_ID_BYTES, MAX_CODEX_TURN_METADATA_BYTES, MAX_UNKNOWN_METADATA_KEYS,
    X_CODEX_PARENT_THREAD_ID, X_CODEX_TURN_METADATA, X_CODEX_WINDOW_ID, X_OPENAI_SUBAGENT,
};
pub use config::{
    build_commit, build_version, current_binary_sha256, ProxyRuntimeConfig, ProxyRuntimeIdentity,
    RuntimeRouteConfig,
};
pub use continuation::{
    ensure_encrypted_reasoning_include, request_disables_server_store, resolve_continuation,
    ContinuationDecision, ContinuationMode,
};
pub use credentials::{CredentialProvider, FileCredentialProvider};
pub use diagnostics::{
    bounded_detail_level, bounded_text, hash_text, input_item_manifest, input_item_text,
    redact_sensitive_json, redact_sensitive_text, spawn_prompt, strip_t3_fields, t3_window_open,
    ChildTurn, CodexMetadataConflictDiagnostic, CompactionDecision, CompactionEngine,
    CompactionOutcome, DetailLevel, DiagnosticEvent, DiagnosticsSink, InputItemManifest,
    LinkConfidence, NoopSink, OfficialAccountSelected, OfficialAuthMode,
    OfficialRequestPrepared, OfficialRequestTransport, RuntimeDiagnostics, SpawnCompleted,
    SpawnRequested, SubagentGraphLinked, SubagentLinkComparison, SubagentLinkMethod,
    T3CaptureSession, WebSocketClosed, WebSocketOpened, WebSocketTransport, T3_MAX_AGE, T3_MAX_BYTES,
};
pub use environment::{
    ExecutionEnvironment, ExecutorCapability, RuntimeAmpersandSemantics, RuntimePathStyle,
    RuntimePlatform, RuntimeShellKind,
};
pub use error::RuntimeError;
pub use exec::{ProxyRuntime, RuntimeResponse, SubagentIdentityMode};
pub use grok_session::{
    conversation_key_from_raw, conversation_key_from_request, grok_request_fingerprint,
    GrokRequestIdentity, GrokSessionRegistry,
};
pub use harness::{
    resolve_with_options, CompletionPolicy, FailurePolicy, HarnessOptions, HarnessProfile,
    HarnessProfileKind, HarnessToolMode, MultiAgentPolicy, ParallelPolicy, PatchContract,
    PromptFamily, SearchPolicy, ShellContract, ToolHistoryContract,
};
pub use history::{
    codex_conversation_key, flatten_chain_items, hydrate_input, resolve_conversation_key,
    ConversationKeyResolution, ConversationKeySource, FileHistoryStore, HistoryEntry, HistoryStore,
    MemoryHistoryStore,
};
pub use identity::{sha256_file_streaming, ArtifactIdentity};
pub use inbound::{
    BoundaryKey, InboundAccessPolicy, BOUNDARY_CREDENTIAL_ID, BOUNDARY_KEY_BYTES,
    BOUNDARY_KEY_ENV_VAR, BOUNDARY_KEY_HEADER,
};
pub use lifecycle::{
    CountingRequestLifecycle, GenerationController, ProxyGeneration, RequestGuard, RequestLifecycle,
};
pub use live_attribution::{
    account_hash, apply_ledger_frame, artifact_to_redacted_json, assert_artifact_is_redacted,
    assert_delta_before_terminal, assert_no_max_output_tokens, assert_no_upgrade_required,
    assert_official_live_body, assert_official_request_parity, assert_opencode_plans_isolated,
    attribute_official, bridge_delay_ms, classify_http_status, classify_official_ab,
    classify_proxy_delay, classify_stream_quality, cold_total_first_frame_ms, evaluate_first_frame,
    extra_delay_budget_ms, identity_fields, is_output_delta, is_reasoning_delta, is_tool_delta,
    is_upgrade_required, ledger_frame_for_request, looks_like_qwen_36, looks_like_qwen_38,
    luna_quota_allows_swe, new_live_artifact, official_catalog_is_luna, official_live_create_body,
    official_live_http_body, official_live_marker_input, official_live_metadata_headers,
    plan_opencode_model, probe_streaming_projection, redact_live_artifact, refuse_qwen_standin,
    resolve_official_live_model, select_qwen_from_models, should_stop_official_luna,
    streaming_qualifies, write_redacted_artifact, AbComparisonInput, AbStage, AbStageTiming,
    AbVerdict, FirstFrameObservation, LedgerFrame, LiveAttributionError, LiveKind, LiveOutcome,
    LivePath, LiveRequestArtifact, OfficialAttribution, OfficialTurnBudget, OpenCodeModelCaps,
    OpenCodeModelPlan, StreamDeltaObserver, StreamQuality, StreamQualityInput, TimingVerdict,
    AB_IDLE_GAP_SECS, LIVE_OFFICIAL_MODEL_ENV, LUNA_SWE_MIN_REMAINING_PERCENT,
    OFFICIAL_FIRST_FRAME_DEADLINE_MS, OFFICIAL_LIVE_TURN_CAP, PINNED_OFFICIAL_MODEL,
};
pub use official_auth::{
    write_grant_atomic, FileManagedOfficialAuthProvider, FileOfficialGrant,
    NativeCodexOfficialAuthProvider, OfficialAuthDecision, OfficialAuthProvider,
    OfficialAuthorization, UnconfiguredOfficialAuthProvider, SELECTED_OFFICIAL_CREDENTIAL_ID,
};
pub use official_identity::{
    chatgpt_credential_id, chatgpt_identity_from_jwt, same_chatgpt_principal, ChatGptIdentity,
    ChatGptWorkspaceKind,
};
pub use opencode::{
    infer_provider_profile, is_opencode_endpoint, OPENCODE_GO_BASE_URL, OPENCODE_PUBLIC_TOKEN,
    OPENCODE_ZEN_BASE_URL,
};
pub use outbound::{
    validate_outbound_url_async, validate_outbound_url_sync, AddressClass, InsecureHttpPolicy,
};
pub use profile_adapter::{
    harness_snapshot_with_environment, normalize_compatible_responses_request,
    prepare_upstream_request_details, prepare_upstream_request_with_environment,
    prepare_upstream_request_with_replay, responses_to_chat, responses_to_chat_with_options,
    responses_to_chat_with_profile, PreparedUpstreamRequest, ProfileAdapterRoute,
};
pub use replay::{
    apply_checkpoint_resume_tail_to_projection, assert_input_identity_len,
    chat_transcript_diagnostics, chat_transcript_diagnostics_with_provenance,
    check_chat_transcript_invariants, check_chat_transcript_invariants_with_provenance,
    checkpoint_item_identity, classify_compaction_item, current_suffix_user_query,
    hydrate_input_with_mode, is_client_local_compaction_marker, journal_item_identity,
    request_item_identity, seed_request_identities, stored_item_identity,
    strip_client_local_compaction, ChatMessageProvenance, ChatProjection, CompactionDisposition,
    HarnessTranscriptDiagnostics, HydrationOutcome, ReplayContext, CHAT_CHECKPOINT_PREFACE,
    CHAT_CHECKPOINT_RESUME_TEXT, NEUTRAL_USER_BRIDGE_TEXT,
};
pub use request::{IncomingAuthContext, RequestMetadata, RuntimeEndpoint, RuntimeRequest};
pub use resource::{
    collect_request_body, CollectBodyError, ResourceGuard, ResourcePolicy,
    DEFAULT_ACTIVE_HTTP_REQUESTS, DEFAULT_BUFFERED_BYTE_BUDGET, DEFAULT_OFFICIAL_WS_SLOTS,
    DEFAULT_THIRD_PARTY_STREAMS,
};
pub use response::normalize_non_streaming_response;
pub use review::{
    build_review_prompt, default_review_settings_source, effective_review_policy, findings_schema,
    guardian_assessment, guardian_assessment_from_chat_body, has_guardian_assessment,
    is_guardian_request, is_guardian_request_with_upstream_model, parse_findings, parse_severity,
    prepare_guardian_request, resolve_guardian_route_plan, severity_rank, sort_and_dedupe_findings,
    Finding, ReviewModelRoute, ReviewPolicy, ReviewRoutePlan, ReviewSettings, ReviewSettingsSource,
    Severity, StaticReviewSettingsSource, AUTO_REVIEW_MODEL,
};
pub use route::{
    grok_responses_readiness, projected_wire, rewrite_grok_chat_routes, ContinuationTail,
    GrokWireRoute, ReasoningReplay, ResolvedRoute, RouteCatalog, RuntimeAccessMode,
    RuntimeAuthKind, RuntimeChatCapabilities, RuntimeCompactionCapabilities, RuntimeModelRoute,
    RuntimeProviderKind, RuntimeProviderProfile, RuntimeReasoningCapabilities,
    RuntimeReasoningEffortTransport, RuntimeToolCapabilities, RuntimeWireFormat,
    CHAT_CAPABILITY_PROBE_VERSION,
};
pub use runtime::{bind_and_serve_static, serve_proxy, serve_proxy_with_router, ProxyServeOptions};
pub use search::{
    parse_brave_web_results, BraveBackend, DisabledSearchEngine, HttpSearchEngine, RawResult,
    RefStore, SearchBackend, SearchEngine, SearchRequest, SearchResponse, SearchResult,
};
pub use server::{
    build_headless_router, build_headless_router_with_resources, health_payload, readyz_payload,
    version_payload,
};
pub use snapshot::{
    CompactionPolicySnapshot, ReviewPolicySnapshot, RuntimePolicySnapshot, RuntimeRouteSnapshot,
    RuntimeSnapshot, ShellContractSnapshot, WebSearchPolicySnapshot,
};
pub use sse::{
    append_utf8_safe, strip_sse_field, take_limited_sse_block, take_sse_block, SseLimitError,
    MAX_PARSER_PENDING_BUFFER_BYTES, MAX_SSE_EVENT_BYTES,
};
pub use state::{ModelRouteView, ProxyRuntimeState, StaticProxyState, StaticRouteCatalog};
pub use streaming::{
    assign_response_message_phases, collapse_adjacent_provider_replay_lines,
    completed_response_from_sse, failed_sse_event, failed_sse_event_with_category,
    normalize_third_party_readable_reasoning, parse_sse_value, sanitize_unparsed_sse_block,
    sse_event_is_response_completed, strip_provider_control_tokens,
    strip_provider_control_tokens_with_options, terminal_error_from_sse, SseCompletionTracker,
    SseProtocolError, ThirdPartySseNormalizer,
};
pub use subagent_graph::{
    CodexSubagentActivity, CodexSubagentActivityStatus, CodexThreadKey, CodexTurnKey,
    GraphObservation, PruneStats, SpawnBindingResult, SpawnBindingSource, SpawnCallBinding,
    SubagentGraphError, SubagentGraphRegistry, SubagentThreadNode, ThreadExecutionBinding,
    TurnObservation, DEFAULT_GRAPH_TTL_MS, DEFAULT_MAX_GRAPH_NODES, MAX_TRAVERSAL_NODES,
};
pub use transport::{
    FixtureTransport, ReqwestTransport, TransportError, UpstreamRequest, UpstreamResponse,
    UpstreamTransport, PRODUCTION_CONNECT_TIMEOUT,
};
pub use usage::{
    build_agent_usage_attribution, usage_from_response, AgentUsageAttribution, FileUsageStore,
    MemoryUsageStore, UsageRecord, UsageStore, UsageSummary,
};

pub const PROXY_RUNTIME_VERSION: &str = env!("CARGO_PKG_VERSION");
