/** 與 Rust 端 serde 結構一一對應。改這裡就要改 src-tauri/src/model.rs。 */

export type WireFormat = "responses" | "chat";
export type ProviderKind = "official" | "openAiCompatible" | "grokCli";
export type AuthKind = "chatGpt" | "bearer" | "grokSession" | "none";
export type AccessMode = "anonymousFree" | "credentialed";

/** 上下文視窗數值的來源。決定 UI 顯示哪一層命中。 */
export type BudgetSource =
  | "override" // 使用者手動填的
  | "modelCache" // 供應商自帶的模型快取
  | "catalog" // Codex 型錄 context_window × percent
  | "fallback"; // 保底常數

export type CatalogScope = "all" | "freeOnly";

export type InsecureHttpPolicy =
  | "deny"
  | "allowPrivateNetwork"
  | "allowPublicWithoutCredentials";

export interface Route {
  id: string;
  name: string;
  baseUrl: string;
  model: string;
  wire: WireFormat;
  isCurrent: boolean;
  /** 上游是否自己記住上一輪（Responses 的 store）。false 代表要本地補歷史。 */
  serverSideResume: boolean;
  streaming: boolean;
  reasoning: boolean;
  providerKind: ProviderKind;
  authKind: AuthKind;
  enabled: boolean;
  models: string[];
  selectedModels: string[] | null;
  contextWindow: number | null;
  modelCapabilities: ModelCapability[];
  /** Third-party plaintext HTTP exemption. Older files omit this and load as deny. */
  insecureHttpPolicy?: InsecureHttpPolicy;
  catalogScope?: CatalogScope;
}

export interface ModelCapability {
  model: string;
  contextWindow: number | null;
  wire: WireFormat | null;
  streaming: boolean | null;
  reasoning: boolean | null;
  /** True only when the provider returned a typed function call during probing. */
  toolCalling?: boolean | null;
  /** Codex-dialect capability probe version; null means a legacy minimal probe. */
  probeVersion: number | null;
  /** Why verification could not complete; distinct from confirmed text-only support. */
  probeIssue?: string | null;
  /**
   * 這個模型在 Codex 選單裡要叫什麼。純顯示——送給供應商的仍是上游 id，而且
   * 上游 id 是 catalog id 的一半，改名絕不能動它。空的就顯示上游 id。
   */
  displayName?: string | null;
  /**
   * 使用者手動宣告這個模型吃不吃圖片，不是探測出來的：純文字端點收到帶圖的
   * 請求照樣回 200 並讓模型自己編，所以「沒被拒絕」不構成證據。
   */
  vision?: boolean | null;
  supportsPersistedReasoning?: boolean | null;
  supportsServerSideCompaction?: boolean | null;
  supportsStandaloneCompaction?: boolean | null;
  compactThresholdTokens?: number | null;
  reasoningEfforts?: string[];
  defaultReasoningEffort?: string | null;
  reasoningEffortTransport?: ReasoningEffortTransport;
  /** `null` only when `effortProbeStatus` is `"indeterminate"` — see there. */
  effortProbeVersion?: number | null;
  effortProbedAt?: number | null;
  /**
   * 這個模型最後一次 Effort 探測的結果狀態，跟 `reasoningEfforts.length === 0`
   * 分開看：那個空陣列可能是「還沒探測過」「探測完了但這個供應商本來就沒有
   * 分級」，也可能是「探測跑過但被 429／5xx／逾時／provider 忽略未知欄位
   * 卡住，判不出來」——三種意思完全不同，UI 不能都畫成「自動」。
   */
  effortProbeStatus?: EffortProbeStatus;
  /** `effortProbeStatus` 是 `"indeterminate"` 時，機器可讀的原因代碼。 */
  effortProbeIssue?: string | null;
  /**
   * 這個模型免不免費，來自供應商自己的型錄（目前只有 OpenCode Zen／Go 有這個
   * 資料）。null／undefined 代表這個供應商沒有免費／付費的區分，不是「已知付費」。
   */
  free?: boolean | null;
  /**
   * 供應商已經把這個模型退役了。跟 `free` 是**兩件事**：
   * `deepseek-v4-flash-free` 到現在價格仍然是 0，但狀態是 deprecated。
   * 免費型錄要同時看這兩個欄位，只看價格就會一直列出供應商已經撤掉的模型。
   * null／undefined 是「不知道」，不是「還在供應」。
   */
  deprecated?: boolean | null;
  /**
   * OpenCode 存取模式。官方目錄確認的免費 Zen 模型是 `anonymousFree`
   *（`Authorization: Bearer public`）；付費、Go 或未知模型是 `credentialed`。
   */
  accessMode?: AccessMode | null;
  /**
   * Chat continuation capabilities. Omitted in older settings and treated as
   * `nativeToolResult` + `none`. Only a probe or live test that observed a
   * provider rejecting a tool-result tail may persist `neutralUserBridge`.
   */
  chatCapabilities?: RuntimeChatCapabilities | null;
  probeAttempts?: ProbeAttempt[];
  lastProbeFailed?: boolean | null;
  lastProbedAt?: number | null;
}

export interface ProbeAttempt {
  stage: string;
  wire?: WireFormat;
  outcome: string;
  status?: number;
  durationMs: number;
  timeout: boolean;
  retryAfter?: number;
  message?: string;
  modelResponseKind?: string;
}

export type ContinuationTail = "nativeToolResult" | "neutralUserBridge";
export type ReasoningReplay = "none" | "toolCallBound";

export interface RuntimeChatCapabilities {
  continuationTail?: ContinuationTail;
  reasoningReplay?: ReasoningReplay;
}

export type ReasoningEffortTransport =
  | "responses_object"
  | "chat_field"
  | "chat_object"
  | "provider_specific"
  | "none";

/**
 * 跟 `reasoningEfforts.length === 0` 分開看的原因：那個空陣列可能代表三種
 * 完全不同的情況（見 `ModelCapability.effortProbeStatus`），UI 不能都當
 * 「自動」畫。
 */
export type EffortProbeStatus =
  | "not_probed"
  | "not_applicable"
  | "supported"
  | "unsupported"
  | "indeterminate";

export interface CodexOAuthAccount {
  accountId: string;
  workspaceId?: string;
  workspaceName?: string | null;
  planType?: string | null;
  email: string | null;
  authenticatedAt: number;
  isDefault: boolean;
}

export interface CodexOAuthStatus {
  authenticated: boolean;
  defaultAccountId: string | null;
  selectionRevision?: number;
  selectionVerified?: boolean;
  selectedAt?: number | null;
  accounts: CodexOAuthAccount[];
}

export interface CodexOAuthDeviceLogin {
  deviceCode: string;
  userCode: string;
  verificationUri: string;
  expiresIn: number;
  interval: number;
}

export type GrokAccountSource = "external" | "managed";

export interface GrokAccount {
  accountId: string;
  email: string | null;
  authenticatedAt: number;
  isDefault: boolean;
  source: GrokAccountSource;
}

export interface GrokAccountStatus {
  authenticated: boolean;
  defaultAccountId: string | null;
  accounts: GrokAccount[];
}

export interface GrokModelCatalog {
  models: string[];
  defaultModel: string | null;
}

export type GrokLoginState = "waiting" | "complete" | "failed" | "cancelled";

export interface GrokLoginStatus {
  loginId: string;
  state: GrokLoginState;
  account: GrokAccount | null;
  error: string | null;
}

export interface CodexResetCredit {
  id: string;
  resetType: string | null;
  status: string;
  expiresAt: string | null;
  title: string | null;
  description: string | null;
}

export interface CodexResetCredits {
  availableCount: number;
  credits: CodexResetCredit[];
}

export interface CodexResetResult {
  code: string;
  windowsReset: number | null;
}

export interface ModelRoute {
  catalogId: string;
  displayName: string;
  routeId: string;
  upstreamModel: string;
  contextWindow: number | null;
  /**
   * 使用者在「模型」頁用鉛筆手動指定的最大上下文長度；null = 交回自動判定。
   *
   * 跟 `contextWindow` 分開存，畫面才說得出「這是你設的」還是「探測到的」。
   * 後端尚未回傳這個欄位之前一律 undefined，鉛筆仍可寫入，只是讀回來會是
   * 探測值。
   */
  contextWindowOverride?: number | null;
  wire: WireFormat;
  reasoning: boolean;
  streaming: boolean;
  reasoningEfforts: string[];
  defaultReasoningEffort: string | null;
  reasoningEffortTransport: ReasoningEffortTransport;
}

export type ProxyPhase =
  | "preparing"
  | "starting"
  | "running"
  | "stopping"
  | "stopped"
  | "failed";

export interface ProxyStatus {
  running: boolean;
  baseUrl: string;
  catalogPath: string | null;
  codexManaged: boolean;
  /** 上游或伺服器任務真的壞掉時的原文。來自外部，翻不了。 */
  lastError: string | null;
  /** 需要知道、但不是故障的狀態。只有代號，句子在前端組。 */
  notice: RuntimeNotice | null;
  /** Lifecycle phase. `running` remains the boolean compatibility field. */
  phase?: ProxyPhase;
  operationId?: string | null;
  generation?: number;
  stage?: string | null;
  stageElapsedMs?: number | null;
}

export interface CatalogStatus {
  proxyRunning: boolean;
  injectedModelIds: string[];
}

export interface RestoreResult {
  status: ProxyStatus;
  changed: boolean;
  cleared: string[];
  preserved: string[];
}

/** 後端要說給人聽的一句話：只有代號與參數，句子在前端組。 */
export interface RuntimeNotice {
  code: string;
  params: Record<string, string>;
}

export interface RuntimeStatus {
  proxyRunning: boolean;
  codexManaged: boolean;
  activeRequests: number;
  draining: boolean;
  restartRequired: boolean;
  restartReasons: RuntimeNotice[];
  liveApplied: RuntimeNotice[];
  activeCatalogVersion: string | null;
}

export interface EnhancedQualificationResult {
  runId: string;
  mode: string;
  startedAt: number;
  finishedAt: number;
  passed: boolean;
  promotionReady: boolean;
  reportPath: string;
  failures: string[];
}

/** One way the installed Codex Desktop's protocol differs from the pinned
 *  Enhanced core's. `routed` is the whole severity model: a difference on a
 *  method the bridge itself demultiplexes leaves no working degraded mode, and
 *  is the only kind that withholds arming. */
export interface EnhancedProtocolDelta {
  kind: "methodUnservable" | "methodUnknownToDesktop" | "fieldNewlyRequired" | "fieldRemoved" | string;
  /** The method or type name the difference is on. */
  subject: string;
  fields: string[];
  routed: boolean;
}

export interface EnhancedProtocolCompatibility {
  /** verified: the pinned pairing. unverified: differs, nothing routed —
   *  Enhanced runs unqualified. incompatible: a routed method broke. */
  verdict: "verified" | "unverified" | "incompatible" | string;
  pinnedSchemaSha256: string;
  desktopSchemaSha256: string;
  deltas: EnhancedProtocolDelta[];
}

export interface EnhancedDesktopRuntimeStatus {
  configured: boolean;
  /** The user asked for Enhanced and the settings verified. */
  enabled: boolean;
  /** The pinned artifact, protocol hash and model map verify on disk. */
  artifactReady: boolean;
  /** Codex Desktop is running through this launch's bridge, both children up. */
  active: boolean;
  /** A live Desktop-owned bridge exists, though it may not be ready. */
  bridgeObserved: boolean;
  /** enabled && artifactReady && active && leased — the only "Enhanced is running". */
  ready: boolean;
  activationState:
    | "disabled"
    | "artifactBlocked"
    | "awaitingDesktopRestart"
    | "active"
    | "disablePendingRestart"
    | "environmentDrift"
    | "failed"
    | string;
  environmentState:
    | "leased"
    | "released"
    | "orphanedBridge"
    | "foreignValue"
    | "unreadable"
    | string;
  /** What CODEX_CLI_PATH names right now. A state word cannot be checked by hand. */
  environmentValue: string | null;
  /** The executable the live bridge actually runs from, which an older build can outlive. */
  observedBridgeExecutable: string | null;
  restartRequired: boolean;
  launchId: string | null;
  bridgeState: string | null;
  bridgePid: number | null;
  officialChildPid: number | null;
  enhancedChildPid: number | null;
  enhancedRuntimeDigest: string | null;
  activeRuntimeDigest: string | null;
  activeFeatureProfile: string | null;
  officialCodexExecutable: string | null;
  enhancedCodexExecutable: string | null;
  bridgeExecutable: string | null;
  /** How the pinned Enhanced core's protocol compares to the installed Codex
   *  Desktop. Null only when the comparison could not run at all — a missing
   *  binary, a failed probe — which is a different thing from a comparison that
   *  ran and found problems. */
  protocol: EnhancedProtocolCompatibility | null;
  /** Codex Desktop is going through the Enhanced core right now, whatever
   *  launch that bridge belongs to. A bridge left from an earlier launch still
   *  serves every turn; only `active`/`ready` additionally require that it be
   *  serving this launch's configuration. This is what the status button
   *  reflects -- "loaded" is about whether Enhanced is answering, not about
   *  whether the config is the newest one. */
  serving: boolean;
  /** Enhanced runs, or would run, against a Codex Desktop this pairing was
   *  never qualified against. This is the state that carries "usable, but
   *  correctness is not guaranteed" — not the clean fallback, where plain
   *  Codex is simply itself and Enhanced features are absent. */
  unverified: boolean;
  lastQualification: EnhancedQualificationResult | null;
  /** Whether this build can find the pinned Enhanced core at all. The core is
   *  not a user-chosen path, so "missing" means a broken install, not an
   *  unfinished setup. */
  coreAvailable: boolean;
  /** Helper executables the official Codex has beside it and the Enhanced core
   *  does not. Chat and routing work without them; sandboxed shell commands
   *  do not, and Codex reports that as a Windows "file not found" dialog. */
  missingHelpers: string[];
  /** Codex Desktop replaced its own core while this launch was running, so the
   *  children are serving a binary the settings no longer name. Vellum repairs
   *  it with a managed restart on its own; this flag is what the screen says
   *  while that is happening. */
  launchCoreDrift: boolean;
  blockers: string[];
}

export interface CatalogVersion {
  id: string;
  createdAt: number;
  path: string;
}

export interface RestartResult {
  restarted: boolean;
  notice: RuntimeNotice;
}

export interface ContextBudget {
  routeId: string;
  catalogId: string;
  model: string;
  /** 命中的來源與數值 */
  source: BudgetSource;
  contextWindow: number;
  effectivePercent: number;
  effectiveWindow: number;
  /** 使用者覆寫；null = 自動 */
  overrideTokens: number | null;
  compactThresholdPercent: number;
}

export type QuotaPeriodUnit = "hour" | "day" | "week" | "month" | "unspecified";

/** 額度視窗長度。後端只給單位與數量，那句話由前端用當前語言組。 */
export interface QuotaPeriod {
  unit: QuotaPeriodUnit;
  /** 命名視窗（週／月）沒有數量 */
  amount: number | null;
}

export interface QuotaSnapshot {
  routeId: string;
  /** 已用百分比 0–100 */
  usedPercent: number;
  period: QuotaPeriod;
  resetAt: string | null;
  tier: string | null;
  /** 取自快取而非即時查詢 */
  stale: boolean;
}

export interface ContextUsage {
  usedTokens: number;
  windowTokens: number;
  turns: number;
  providerTotalTokens: number;
  /** 走勢取樣，0–1 相對值 */
  trend: number[];
  compacted: boolean;
}

export type Severity = "critical" | "warning" | "info";

export interface Finding {
  id: string;
  severity: Severity;
  title: string;
  location: string;
}

/**
 * 自動審查要用誰。
 *
 * 這是策略，不是兩個各自獨立的開關 —— 兩種互斥的意圖：
 *   always    永遠用指定的那個，額度爆了就讓它失敗
 *   failover  指定的優先，那家不能用時換備援
 */
export type ReviewPolicy = "always" | "failover";

export interface ReviewSettings {
  onEdit: boolean;
  beforeSend: boolean;
  beforeCompact: boolean;
  routeId: string;
  model: string;
  /**
   * 以下兩個是**選用的** —— Rust 端還沒送。
   * 一定要 optional：宣告成必填但後端不送，前端會拿到 undefined 當成有效值用，
   * 那正是壓縮色條整條空掉的那個坑。讀取一律走 lib/review.ts 的 policyOf()。
   */
  policy?: ReviewPolicy;
  /**
   * 審查跑在 Official 平面時，由哪個 ChatGPT 帳號計費。
   *
   * `null`／`undefined` 一律解讀成「跟著目前的帳號」——這是加上這個欄位
   * 之前所有存檔的意思，也是非 Official 線路唯一說得通的意思。存的只有
   * 帳號 id，憑證本身還在 OAuth store 裡。
   */
  officialAccountId?: string | null;
  /** failover 時的備援模型（catalogId）。單一備援，不做鏈 —— 見 lib/review.ts。 */
  fallbackCatalogId?: string | null;
}

/**
 * `set_review_settings` 的回傳形狀。
 *
 * `localApplied` 恆為 true —— proxy 每個新請求都會重新讀設定
 * （`ReviewSettingsSource`），不用重開就生效；留著這個欄位是把這個保證
 * 明講出來，不是條件判斷用的。`remoteHostsPendingReapply` 是本機算出來
 * 的、跟遠端無關的數字，不代表存檔當下改動了任何遠端主機 —— 要套用得
 * 去 Remote Manager 按重新套用。
 */
export interface ReviewSettingsUpdate {
  settings: ReviewSettings;
  localApplied: boolean;
  remoteHostsPendingReapply: number;
}

/**
 * Which sub-agent model default Vellum manages in Codex.
 *
 * `inherit` leaves Codex's own sub-agent defaults untouched; `custom` writes
 * the selected Provider/model/effort into the managed Codex config.
 */
export type SubagentMode = "inherit" | "custom";

export interface SubagentSettings {
  mode: SubagentMode;
  /** Provider route the default model belongs to (display grouping only). */
  routeId: string | null;
  /** Stable catalog id written to Codex `agents.default_subagent_model`. */
  catalogId: string | null;
  /**
   * Reasoning effort written to Codex
   * `agents.default_subagent_reasoning_effort`. `null` means automatic: the
   * selected model uses its own default effort.
   */
  reasoningEffort: string | null;
}

export interface SubagentCapability {
  supported: boolean;
  /** User-facing Codex Desktop build, not a standalone CLI version. */
  desktopVersion: string | null;
  /** Bundled Desktop core used internally for capability verification. */
  runtimeVersion: string | null;
  detail: string;
}

/** 某一家供應商在自動審查裡出手幾次。 */
export interface ReviewProviderStat {
  routeId: string;
  provider: string;
  model: string;
  /** 以「指定模型」身分跑的次數 */
  primaryRuns: number;
  /** 以「備援」身分跑的次數 —— 指定的那家不能用時頂上 */
  fallbackRuns: number;
  failedRuns: number;
  lastUsedAt: number | null;
}

/**
 * 自動審查的實際狀況。
 *
 * 重點不是「總共跑了幾次」（那個數字單獨看沒有意義），而是
 * **備援出手的佔比** —— 那才回答「我的主要模型撐不撐得住」。
 */
export interface ReviewStats {
  totalRuns: number;
  fallbackRuns: number;
  providers: ReviewProviderStat[];
  /** 現在實際由誰回答。可能不是指定的那個。 */
  activeRouteId: string | null;
  activeModel: string | null;
  activeIsFallback: boolean;
  /** 為什麼是它（例如「Grok 週額度用盡」）。不是備援時為 null。 */
  activeReason: string | null;
}

/**
 * 網頁搜尋。
 *
 * Rust 端的 `SearchMode` 有四階，但介面只承認其中兩階：
 *   disabled  跟 `enabled: false` 是同一件事的兩種說法 —— UI 只給一顆開關
 *   cached    `allows_search()` 對它是 false，選了它每次搜尋都會拋錯
 * 讀寫一律走 lib/webSearch.ts，不要在畫面裡自己判斷這兩個值。
 */
export type SearchMode = "disabled" | "cached" | "indexed" | "live";

export interface SearchDomainPolicy {
  allow: string[];
  block: string[];
}

/** Brave Search 是唯一的後端；沒有其他家可選，也不會靜默退回別家。 */
export interface WebSearchSettings {
  enabled: boolean;
  mode: SearchMode;
  domainPolicy: SearchDomainPolicy;
  /**
   * 持久化的值沒有人讀 —— 引擎用的是 Codex 每次請求帶進來的
   * `search_context_size`（web_search.rs 的 response_length_default）。
   * 保留欄位只為了往返時不掉值，**不要曝到畫面上**。
   */
  searchContextSize: string;
}

/** 金鑰不隨設定往返，後端只回「有沒有」。 */
export interface WebSearchSettingsView {
  settings: WebSearchSettings;
  hasBraveApiKey: boolean;
  /** 只有剛完成「舊後端設定沒有金鑰所以關掉搜尋」的遷移時才有值，讀一次就沒了。 */
  migrationNotice?: RuntimeNotice | null;
}

export interface WebSearchProbeResult {
  output: string;
  resultCount: number;
}

/**
 * 一個 Codex 工作階段。
 *
 * 身分來自 history.rs 的 `conversation_key`（Codex 送的 conversation_id／thread_id
 * 的 SHA256）。那是雜湊，**對人沒有意義** —— 所以畫面靠「模型 + 用量 + 最後活動」
 * 讓人分辨，短碼只當備用識別。
 *
 * `label` 留給後端有辦法拿到工作目錄／專案名時用；拿不到就是 null，
 * 畫面不會自己編一個名字出來。
 */
export interface SessionStatus {
  /** conversation_key（雜湊）。UI 只顯示前幾碼。 */
  id: string;
  /** 人看得懂的名字（工作目錄／專案）。拿不到就 null。 */
  label: string | null;
  routeId: string;
  provider: string;
  model: string;
  usedTokens: number;
  windowTokens: number;
  /** 壓縮門檻百分比，跟 ContextBudget 同一個來源 */
  compactThresholdPercent: number;
  /** epoch 秒。判斷「還連著嗎」唯一的依據。 */
  lastActivityAt: number;
  /**
   * 這個工作階段跑在哪一顆 core 上。後端尚未填之前一律是 null。
   *
   * 「上下文」頁只講 Enhanced core 的事，判定集中在
   * {@link isEnhancedCoreSession} 一個函式裡，不要散在畫面各處。
   */
  core?: "enhanced" | "official" | null;
}

export interface CompactionPreview {
  beforeTokens: number;
  afterTokens: number;
  /** false 表示壓縮後為 OpenAI opaque canonical state，無法由本機精確估算 token。 */
  afterTokensExact: boolean;
  /**
   * Canonical 檢查點、Readable Reasoning Replay、工具續作狀態與保留的對話脈絡。
   *
   * before/after 是長條的相對長度；beforeTokens/afterTokens 是實際 token 數。
   * 兩者分開是因為「畫多長」與「是多少」不該互相回推 —— 之前用比例回推，
   * 結果「完整保留」的那一段被算成少了 67k，數字自己打自己的臉。
   */
  /**
   * 每段使用穩定的 `kind`，顯示名稱可獨立本地化。
   *
   * `before`/`after` 是**同一把尺上的絕對量**（見 compaction.rs 的 build_preview）：
   * 系統與近期前後不變，只有早期會縮小，所以直接拿來當長度是對的。
   *
   * `beforeTokens`/`afterTokens` 是選用的：Rust 端目前不送，只有 mock 有。
   * **一定要 optional**，否則前端會拿 undefined 去算 flexGrow，
   * 每一段都變成 0，色條就整條空掉 —— 那正是實機上看到的空框。
   */
  segments: {
    /** Stable semantic category; labels may be localized. */
    kind?: "canonical_checkpoint" | "readable_reasoning" | "tool_state" | "retained_context" | "observed_total" | string;
    label: string;
    before: number;
    after: number;
    beforeTokens?: number;
    afterTokens?: number;
    tone: string;
  }[];
  keepRecentTurns: number;
  engine: "official_canonical" | "local_canonical" | "legacy_recovered" | "codex_client" | "pending" | string;
  readableReplayTokens: number;
  crossSessionTokens: number;
  crossSessionAvailable: boolean;
}

/**
 * 早期往返被換成的結構化摘要。
 * 對應 src-tauri/src/compaction.rs 的 StructuredSummary —— 摘要不是一段散文，
 * 是有欄位的，所以畫面也該按欄位呈現，而不是塞成一大段。
 */
export interface StructuredSummary {
  goal: string;
  acceptanceCriteria: string[];
  constraints: string[];
  userPreferences: string[];
  done: string[];
  inProgress: string[];
  blocked: string[];
  decisions: string[];
  changedFiles: string[];
  relevantFiles: string[];
  commands: string[];
  tests: string[];
  unresolved: string[];
  errors: string[];
  criticalContext: string[];
  references: string[];
  nextSteps: string[];
}

/** 逐字保留，還是被摘要掉了 */
export type Disposition = "kept" | "summarized";

export interface CompactionItem {
  id: string;
  role: "system" | "user" | "assistant" | "tool" | "other";
  disposition: Disposition;
  tokens: number;
  /** 原文。上游自己保存且加密時為 null —— 那時候 Vellum 手上沒有。 */
  text: string | null;
}

export interface CompactionDetail {
  /** 本機是否握有原文。false 時只能顯示發生了什麼，不能顯示內容。 */
  originAvailable: boolean;
  /** originAvailable=false 時說明為什麼，以及怎樣才看得到 */
  unavailableReason: string | null;
  checkpointId: string | null;
  createdAt: number | null;
  /** manual = 使用者手動；codex_auto = Codex 自動觸發（包含模型切換前壓縮）。 */
  trigger: "manual" | "codex_auto" | string | null;
  provider: string | null;
  model: string | null;
  /** Codex task label when the event came from a client rollout. */
  sourceLabel: string | null;
  canonicalKind: "Official" | "Local" | "LegacyRecovered" | string | null;
  schemaVersion: number | null;
  sourceTokens: number;
  /** OpenAI opaque canonical state 無法由本機精確估算，因此為 null。 */
  canonicalTokens: number | null;
  canonicalHash: string | null;
  exactRecovery: boolean;
  portableAvailable: boolean;
  encryptedBytes: number;
  officialMode: "standalone" | "server_side" | null;
  canonicalItems: Array<{
    index: number;
    itemType: string;
    id: string | null;
    encryptedBytes: number;
    sha256: string;
  }>;
  items: CompactionItem[];
  summary: StructuredSummary | null;
}

/**
 * 一次壓縮的原文。
 *
 * `prompt` 是送給模型的壓縮指示，`result` 是模型寫回來的東西 —— 也就是這個
 * 對話接下來真正帶著走的內容。
 *
 * 兩個欄位都可能是 null，而且**原因不同**：OpenAI 自己壓的那些，結果是
 * Vellum 讀不到的密文；Codex Desktop 自己壓的，指示根本沒有經過 Proxy。
 * 讀不到的時候 `unavailableReason` 要說出是哪一種，不要留空白讓人以為壞了。
 */
export interface CompactionTranscript {
  /** 是哪一次壓縮 —— 跟上面那張前後對照卡是同一個事件。 */
  checkpointId: string | null;
  createdAt: number | null;
  /** 送出去的壓縮指示，原樣。 */
  prompt: string | null;
  /** 模型寫回來的壓縮結果，原樣。 */
  result: string | null;
  /** 兩者都讀不到時的原因。有內容時為 null。 */
  unavailableReason: string | null;
}

/** Preview and detail derived from one selected compaction event. */
export interface CompactionSnapshot {
  preview: CompactionPreview;
  detail: CompactionDetail;
}

export interface ProbeResult {
  reachable: boolean;
  wire: WireFormat | null;
  models: string[];
  contextWindow: number | null;
  streaming: boolean;
  reasoning: boolean;
  serverSideResume: boolean;
  modelCapabilities: ModelCapability[];
  /** 探不到、需要人填的欄位名 */
  needsInput: string[];
}

export interface ModelProbeError {
  model: string;
  message: string;
  stage?: string;
  outcome?: string;
  status?: number;
  timeout?: boolean;
  retryAfter?: number;
}

/**
 * Provider 級重新探測（`reprobeRouteCapabilities`）的結果。不是成敗二選一：
 * 刷新 `/models` 型錄跟驗證已選模型是兩件事，一個已選模型探測失敗不代表
 * 整輪重新探測「失敗」——其他已選模型可能都成功了。只有型錄刷新本身連不上
 * 或憑證錯誤，指令才會整個回錯。
 */
export interface RouteReprobeReport {
  /** 這次刷新型錄找到的模型數。 */
  discovered: number;
  /** 這次實際嘗試驗證的模型數（= 已選模型 ∩ 型錄裡還在的模型）。 */
  targeted: number;
  succeeded: number;
  failed: number;
  /** 已選但型錄裡已經沒有的模型數——跟 `failed`（探測跑了但沒過）不同。 */
  skipped: number;
  errors: ModelProbeError[];
}

export interface HistoryStorageTelemetry {
  logicalLiveBytes: number;
  mainDbPhysicalBytes: number;
  walBytes: number;
  compactionJournalBytes: number;
  historyTruncatedWithoutCompaction: number;
  historyTruncatedItems: number;
  lastMaintenanceAt: number | null;
  lastVacuumAt: number | null;
  lastJournalRetentionDeleted: number;
}

export interface Health {
  pooled: boolean;
  connections: number;
  firstByteMs: number | null;
  reasoningVisible: boolean;
  historyRetentionDays: number;
  historyStorage?: HistoryStorageTelemetry | null;
}

export interface Overview {
  route: Route | null;
  lastSuccessfulRoute: RouteTelemetry | null;
  quota: QuotaSnapshot | null;
  usage: ContextUsage;
  health: Health;
  findings: Finding[];
}

export interface RouteTelemetry {
  routeId: string;
  provider: string;
  model: string;
  createdAt: number;
}

export interface ProviderModelStatus {
  catalogId: string;
  displayName: string;
  upstreamModel: string;
  contextWindow: number | null;
  effectiveWindow: number;
  reasoning: boolean;
  streaming: boolean;
}

export interface ProviderOverview {
  route: Route;
  appliedToRunningProxy: boolean;
  quota: QuotaSnapshot | null;
  quotaWindows: QuotaSnapshot[];
  quotaError: string | null;
  models: ProviderModelStatus[];
  latestInputTokens: number;
  turns: number;
  firstByteMs: number | null;
}

export interface RequestLogEntry {
  id: number;
  routeId: string;
  /** 目前的顯示名稱，讀取時用 routeId 換算，不是紀錄當下存的那個 */
  provider: string;
  model: string;
  inputTokens: number;
  outputTokens: number;
  cachedInputTokens?: number;
  status: number;
  error: string | null;
  durationMs: number;
  firstByteMs: number | null;
  createdAt: number;
  connectionId?: string | null;
  /** 串流品質分類；與 transport outcome 分開顯示 */
  streamQuality?: string | null;
  firstOutputDeltaMs?: number | null;
  firstReasoningDeltaMs?: number | null;
  outputDeltaCount?: number;
  reasoningDeltaCount?: number;
  /** Stable short SHA-256 identity of the Vellum request id; lets the Log
      screen correlate a parent request row to its spawned sub-agent runs
      without exposing the raw request id. */
  requestIdHash?: string | null;
  controlAccountHash?: string | null;
  executionAccountHash?: string | null;
}

export interface CompactionLogEntry {
  id: number;
  createdAt: number;
  engine: string;
  outcome: string;
  reason?: string | null;
  tokensBefore?: number | null;
  tokensAfter?: number | null;
  sourceModelVisibleTokens?: number | null;
  replacementModelVisibleTokens?: number | null;
  replacementDurableTokens?: number | null;
  qualityOutcome?: string | null;
  candidateGeneration?: number | null;
  fallbackReason?: string | null;
  itemsBefore?: number | null;
  itemsAfter?: number | null;
  window?: number | null;
  activeTokens?: number | null;
  thresholdPercent?: number | null;
  /** Absent for a stateless standalone compact that was never journaled —
      not a loading state, an honest "no durable checkpoint exists". */
  checkpointId?: string | null;
  generation?: number | null;
}

export interface SubagentLogEntry {
  id: number;
  createdAt: number;
  kind: string;
  callId?: string | null;
  parentRequestId?: string | null;
  childRequestId?: string | null;
  childModel?: string | null;
  linkMethod?: string | null;
  linkConfidence?: string | null;
}

/**
 * 一個 subagent run 的真實狀態機。`completed` 只在被 join 的 child 請求
 * 自己的 usage/outcome 說成功時才成立 —— 不是看到 SpawnCompleted 事件
 * 就代表成功；那只證明 parent 收到了「一個」工具結果。
 */
export type SubagentRunState =
  | "requested"
  | "running"
  | "completed"
  | "failed"
  | "cancelled"
  | "ambiguous"
  | "unlinked";

export type SubagentLinkConfidence = "exact" | "heuristic" | "unlinked";

export interface SubagentRun {
  callId?: string | null;
  state: SubagentRunState;
  /** Stable short SHA-256 identities; the raw request ids never reach the UI. */
  parentRequestHash?: string | null;
  childRequestHash?: string | null;
  /** `request_usage.id` navigation anchors: scroll to the parent/child row. */
  parentUsageEntryId?: number | null;
  childUsageEntryId?: number | null;
  routeId?: string | null;
  model?: string | null;
  effort?: string | null;
  linkConfidence: SubagentLinkConfidence;
  requestedAt: number;
  completedAt?: number | null;
  durationMs?: number | null;
  outcome?: string | null;
  errorCategory?: string | null;
}

export interface ProviderUsage {
  provider: string;
  requests: number;
  inputTokens: number;
  outputTokens: number;
  failedRequests: number;
}

export interface RequestLogRouteOption {
  routeId: string;
  provider: string;
}

export interface RequestLog {
  entries: RequestLogEntry[];
  providers: ProviderUsage[];
  /** Matching rows, not the current page length. */
  entryTotal?: number;
  entryOffset?: number;
  compactionTotal?: number;
  subagentTotal?: number;
  entryRoutes?: RequestLogRouteOption[];
  compactionEvents?: CompactionLogEntry[];
  /** 原始逐事件 feed，保留供除錯用；畫面改看 subagentRuns。 */
  subagentEvents?: SubagentLogEntry[];
  subagentRuns?: SubagentRun[];
}

export interface BootTelemetry {
  bootCount: number;
  startedAt: number;
  previousStartedAt: number | null;
  pid: number;
}

export interface UsageActivityDay {
  date: string;
  tokens: number;
  requests: number;
}

export interface UsageProviderTotal {
  routeId: string;
  provider: string;
  tokens: number;
  accountCount: number;
  source: "codex_profile" | "proxy";
}

export interface UsageActivity {
  days: UsageActivityDay[];
  totalTokens: number;
  peakTokens: number;
  longestTaskDurationMs: number;
  currentStreakDays: number;
  longestStreakDays: number;
  providers: UsageProviderTotal[];
  officialSource: "codex_profile" | "proxy_fallback";
  warning: RuntimeNotice | null;
}

/** 熱區圖的一格：某一天的用量。 */
export interface UsageDay {
  /** ISO 日期 YYYY-MM-DD（本地時區） */
  date: string;
  inputTokens: number;
  outputTokens: number;
  requests: number;
  failedRequests: number;
}

/**
 * 每日／每週／累計用量。
 *
 * 熱區圖需要的是「連續的日曆」，不是「有紀錄的那幾天」—— 沒有用量的日子
 * 也要是一格空格，不然看不出斷點。所以 days 必須是補滿的連續區間。
 */
export interface UsageHeatmap {
  days: UsageDay[];
  /** 累計（不受 days 區間限制） */
  totalInputTokens: number;
  totalOutputTokens: number;
  totalRequests: number;
  /** 單日最大值，用來正規化色階；後端算好，前端不要自己掃 */
  maxDayTokens: number;
}

export interface RemoteHostCandidate {
  codexHostId: string | null;
  vellumHostId: string;
  displayName: string;
  sshAlias: string;
  hostname: string | null;
  user: string | null;
  port: number | null;
  source: "codexApp" | "openSsh" | string;
  validated: boolean;
  validationError: string | null;
}

/** Whether the resolved (hostname/IP, port) behind a cached host's SSH
 * destination has already been explicitly confirmed in Vellum's own
 * known_hosts file. Keyed on the resolved target, not the SSH alias. */
export interface SshTrustStatus {
  host: string;
  port: number;
  confirmed: boolean;
}

/** A host key currently offered by a host, fetched via `ssh-keyscan` without
 * connecting or trusting it. `fingerprint` is formatted exactly like
 * `ssh-keygen -lf` (`SHA256:<base64>`) so it can be cross-checked as-is. */
export interface PendingHostFingerprint {
  host: string;
  port: number;
  keyType: string;
  fingerprint: string;
}

export interface NativeCodexRuntimeStatus {
  codexHome: string;
  codexBinary: string | null;
  codexVersion: string | null;
  compatible: boolean;
  compatibilityReason: string | null;
  daemonRunning: boolean;
  daemonPid: number | null;
  daemonVersion: string | null;
  daemonOwner: "absent" | "codexCliDaemon" | "codexAppDirect" | string;
  restartSafe: boolean;
  durable: boolean;
  standaloneInstalled: boolean;
  cliLauncher?: {
    path: string | null;
    target: string | null;
    ready: boolean;
  };
  remoteControlEnabled: boolean | null;
  activeTurn: boolean | null;
  sessionAuthority: "codexNativeDaemon";
  brokerEnabled: false;
}

export interface RemoteHostStatus {
  hostId: string;
  managerState: string;
  cachedHost: { id: string; name: string; sshAlias: string } | null;
  agent: {
    agentVersion: string;
    agentProtocol: number;
    capabilities: {
      os: string;
      arch: string;
      dockerAvailable: boolean;
      dockerMode: string;
      rootlessDocker: boolean;
      userSystemdAvailable: boolean;
      lingerEnabled: boolean;
      codexBinary: string | null;
      codexVersion: string | null;
      platform?: string | null;
      proxyBackend?: string | null;
      serviceManager?: string | null;
      persistenceScope?: string | null;
      managedCodexHome?: string | null;
      guiSessionAvailable?: boolean | null;
    };
    proxy: {
      present: boolean;
      running: boolean;
      ready: boolean;
      image: string | null;
      imageDigest: string | null;
      configHash: string | null;
      lastError: string | null;
    };
    configuration: {
      present: boolean;
      state: "missing" | "current" | "upgradeRequired" | "repairRequired" | "incompatible" | "unreadable";
      schemaVersion: number | null;
      requiresReconfigure: boolean;
      issue: string | null;
      configHash: string | null;
      credentialRefs: string[];
      credentialsReady: boolean;
    };
    nativeCodex: NativeCodexRuntimeStatus | null;
    grok: RemoteGrokStatus | null;
  } | null;
  agentError: string | null;
  availableActions: string[];
  blockedReasons: string[];
  inventory: RemoteHostInventory | null;
  chatgpt?: RemoteChatGptAccountStatus | null;
}

export interface RemoteSystemInventory {
  os: string;
  arch: string;
  hostname: string | null;
  cpuCores: number | null;
  memoryBytes: number | null;
  diskTotalBytes: number | null;
  diskFreeBytes: number | null;
}

export interface RemoteDockerInventory {
  available: boolean;
  mode: string;
  rootless: boolean;
  daemon: string | null;
  serverVersion: string | null;
  clientVersion: string | null;
  context: string | null;
  userInDockerGroup: boolean | null;
}

export interface RemoteCodexInventory {
  binary: string | null;
  version: string | null;
  source: "native" | "path" | "missing";
  codexHome: string | null;
  standaloneInstalled: boolean;
  appCliDiscoverable?: boolean;
  appCliPath?: string | null;
  appServerSupported: boolean;
  compatible: boolean;
  compatibilityReason: string | null;
}

export interface RemoteHostInventory {
  hostId: string;
  agentVersion: string;
  agentProtocol: number;
  system: RemoteSystemInventory;
  docker: RemoteDockerInventory;
  codex: RemoteCodexInventory;
  blockers: { code: string; message: string; repairable: boolean }[];
  availableActions: string[];
  platform?: string | null;
  proxyBackend?: string | null;
  serviceManager?: string | null;
  persistenceScope?: string | null;
  managedCodexHome?: string | null;
}

export interface RemoteGrokStatus {
  configured: boolean;
  credentialId: string;
  account: string | null;
  expiresAt: string | null;
  lastRefreshAt: string | null;
  detachedQualified: boolean;
  refreshTimerActive: boolean;
  loginPending: boolean;
  verificationUri: string | null;
  userCode: string | null;
  loginStartedAt: string | null;
  error: string | null;
}

export interface RemoteChatGptAccountStatus {
  accountId: string | null;
  expectedAccountId: string | null;
  state: "synchronized" | "pairingRequired" | "pairingPending" | "activationRequired" | "desktopAccountUnavailable" | string;
  paired: boolean;
  active: boolean;
  loginPending: boolean;
  loginId: string | null;
}

export interface RemoteChatGptAccountLogin {
  loginId: string;
  verificationUrl: string;
  userCode: string;
  expectedAccountId: string;
}

/**
 * 一台主機對 Desktop 上「每一個」ChatGPT 帳號的配對狀態。
 *
 * `paired` 說的是這台主機自己有沒有那個帳號的 grant —— 不是 Desktop 有沒有。
 * 兩邊各持一份獨立的 grant 是刻意的：OAuth 的 refresh token 會輪替，而且伺服器
 * 端會偵測重用，所以一份 grant 不能給兩個客戶端共用。`detail` 有值代表這一列
 * 自己查不到，其他列仍然有效。
 */
export interface RemoteChatGptAccountPairing {
  accountId: string;
  email: string | null;
  workspaceName?: string | null;
  isDesktopDefault: boolean;
  paired: boolean;
  active: boolean;
  detail: string | null;
}

/** Vellum-managed Official execution identity. Raw account IDs and grants never leave the remote host. */
export interface RemoteOfficialExecutionAccount {
  accountIdHash: string;
  displayName: string;
  authenticatedAt: string;
  selected: boolean;
  selectionRevision?: number;
  selectionVerified?: boolean;
}

export interface RemoteOfficialExecutionAccountLogin {
  loginId: string;
  userCode: string;
  verificationUrl: string;
  expiresIn: number;
  interval: number;
}

export interface RemoteOfficialExecutionAccountPoll {
  state: "pending" | "authenticated" | "expired" | string;
  accountIdHash: string | null;
}

/**
 * Allowlisted short-lived fields from `codex remote-control pair --json`.
 * The remote agent deliberately drops every unknown field so account ids,
 * emails, and tokens cannot cross this RPC boundary.
 */
export interface RemoteControlPairing {
  pairingCode?: string;
  code?: string;
  userCode?: string;
  expiresAt?: string;
  verificationUrl?: string;
  uri?: string;
  pairUri?: string;
  qrUri?: string;
}

export interface RemoteModelSelection {
  catalogIds: string[];
  policy?: {
    compactionThresholdPercent?: number | null;
    autoReviewEnabled?: boolean | null;
    standaloneWebSearch?: boolean | null;
  };
}

export interface RemoteDeploymentPlan {
  planId: string;
  hostId: string;
  state: string;
  desiredRevision: number;
  observedRevision: number;
  selectedCatalogIds: string[];
  selectedModels: Array<{
    catalogId: string;
    displayName: string;
    routeId: string;
    upstreamModel: string;
    selected: boolean;
    mandatory: boolean;
  }>;
  credentialRequirements: Array<{
    credentialId: string;
    kind: string;
    availableOnDesktop: boolean;
    detachedQualified: boolean;
  }>;
  drift: {
    desiredConfigHash: string;
    remoteConfigHash: string | null;
    configChanged: boolean;
    desiredCatalogHash: string;
    remoteCatalogHash: string | null;
    catalogChanged: boolean;
    /** Desktop-side only — never compared against anything the remote host reports. */
    reviewPolicyFingerprint: string;
    /** True when local Auto Review settings changed since this host's own last successful apply. */
    reviewPolicyChanged: boolean;
  };
  publicDiff: Array<{
    path: string;
    oldValue: string | null;
    newValue: string | null;
  }>;
  qualifiedCapabilities: Array<{
    catalogId: string;
    routeId: string;
    upstreamModel: string;
    toolCalling: boolean;
    probeVersion: string | null;
    vision: boolean;
    reasoning: boolean;
  }>;
  rollbackSummary: string;
  planHash: string;
  managedChanges: string[];
  restartRequired: boolean;
  blockedReasons: string[];
  expiresAt: string;
}

export interface RemoteHostDesiredState {
  hostId: string;
  desiredRevision: number;
  observedRevision: number;
  lastPlanId: string | null;
  selectedCatalogIds: string[];
  configHash: string | null;
  catalogHash: string | null;
}

export interface RemoteReleaseStatus {
  ready: boolean;
  trust: "bundled" | string;
  releaseVersion: string | null;
  codexVersion: string | null;
  agentVersion: string | null;
  brokerVersion: string | null;
  proxyImage: string | null;
  proxyDigest: string | null;
  detail: string;
}

export interface DesktopCodexCompatibilityStatus {
  state:
    | "current"
    | "updateAvailable"
    | "qualificationRequired"
    | "agentUpdateRequired"
    | "desktopUnavailable"
    | "unavailable"
    | string;
  desktopVersion: string | null;
  desktopSchemaSha256: string | null;
  remoteVersion: string | null;
  requiredRemoteVersion: string | null;
  remoteArch: string | null;
  agentProtocol: number | null;
  canUpdate: boolean;
  detail: string;
}

export interface RemoteSessionSummary {
  hostId: string;
  managerState: string;
  detachedReady: boolean;
  observability: "nativeAppServer" | "unsupported" | "daemonDown";
  threads: Array<{
    threadId: string;
    status: string;
    activeTurn: boolean;
    activeTurnId: string | null;
    turnCount: number;
    lastTurnStatus: string | null;
  }>;
}

export interface RemoteOperationProgress {
  operationId: string;
  hostId: string;
  kind: string;
  phase: string;
  percent: number;
  state: "running" | "completed" | "failed";
  message: string | null;
  result: unknown | null;
  updatedAt: string;
}

export interface RemoteBootstrapResult {
  operationId: string;
  state: string;
  os: string;
  arch: string;
  agentInstalled: boolean;
  brokerInstalled: boolean;
  dockerAvailable: boolean;
  systemdUserAvailable: boolean;
  lingerEnabled: boolean;
  nativeCodex: NativeCodexRuntimeStatus | null;
  completedSteps: string[];
  blockedReasons: string[];
  repairCommands: string[];
}

export type UpdateComponent = "desktop" | "remote" | "core";
export type UpdateChannel = "stable" | "preview";
export type UpdatePhase =
  | "idle"
  | "checking"
  | "available"
  | "downloading"
  | "verifying"
  | "staged"
  | "waitingForIdle"
  | "waitingForRestart"
  | "applying"
  | "validating"
  | "applied"
  | "blocked"
  | "failed"
  | "rolledBack";
export type UpdateAttention = "none" | "available" | "waitingIdle" | "waitingRestart" | "failed";

export interface HostUpdateStatus {
  hostId: string;
  phase: UpdatePhase;
  currentVersion: string | null;
  stagedVersion: string | null;
  idleAutoUpdate: boolean;
  failureReason: string | null;
}

export interface LayerStatus {
  component: UpdateComponent;
  currentVersion: string;
  availableVersion: string | null;
  stagedVersion: string | null;
  channel: UpdateChannel;
  phase: UpdatePhase;
  applyCondition: string;
  releaseNotes: string | null;
  failureReason: string | null;
  operationId: string | null;
  targetVersion: string | null;
  downloadBytes: number;
  downloadTotal: number;
  liveAutoUpdate: boolean;
  hosts: HostUpdateStatus[];
}

export interface UpdatePreferences {
  channel: UpdateChannel;
  autoCheck: boolean;
  autoDownload: boolean;
  coreIdleHandoff: boolean;
}

export interface UpdateStatusSnapshot {
  desktop: LayerStatus;
  remote: LayerStatus;
  core: LayerStatus;
  preferences: UpdatePreferences;
  liveAutoUpdate: boolean;
  attention: UpdateAttention;
}

export interface UpdateOperation {
  operationId: string;
  component: UpdateComponent;
  phase: UpdatePhase;
  targetVersion: string | null;
}
