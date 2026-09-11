import type {
  CodexOAuthStatus,
  CodexResetCredit,
  CodexResetCredits,
  CompactionDetail,
  CompactionTranscript,
  QuotaSnapshot,
  RequestLog,
  RequestLogEntry,
  UsageActivity,
  ProviderUsage,
  CompactionPreview,
  CompactionSnapshot,
  ContextBudget,
  BootTelemetry,
  Overview,
  ProbeResult,
  ReviewSettings,
  ReviewStats,
  SubagentSettings,
  SessionStatus,
  Route,
  WebSearchSettingsView,
} from "@/types";

export function routes(): Route[] {
  return [
    {
      id: "grok-cli",
      name: "Grok Build",
      baseUrl: "https://cli-chat-proxy.grok.com",
      model: "grok-4.5",
      wire: "responses",
      isCurrent: true,
      serverSideResume: false,
      streaming: true,
      reasoning: true,
      providerKind: "grokCli",
      authKind: "grokSession",
      enabled: true,
      models: ["grok-4.5"],
      selectedModels: ["grok-4.5"],
      contextWindow: 500_000,
      modelCapabilities: [],
    },
    {
      id: "weikuwu",
      name: "weikuwu",
      baseUrl: "https://api.weikuwu.example/v1",
      model: "GLM-5.2",
      wire: "chat",
      isCurrent: false,
      serverSideResume: false,
      streaming: true,
      reasoning: false,
      providerKind: "openAiCompatible",
      authKind: "bearer",
      enabled: true,
      models: ["GLM-5.2", "GLM-5.2p", "DeepSeek-V4-Flash"],
      selectedModels: ["GLM-5.2"],
      contextWindow: null,
      modelCapabilities: [],
    },
    {
      id: "openai",
      name: "OpenAI",
      baseUrl: "https://api.openai.com/v1",
      model: "gpt-5.6-sol",
      wire: "responses",
      isCurrent: false,
      serverSideResume: true,
      streaming: true,
      reasoning: true,
      providerKind: "official",
      authKind: "chatGpt",
      enabled: true,
      models: ["gpt-5.6-sol"],
      selectedModels: null,
      contextWindow: 272_000,
      modelCapabilities: [],
    },
  ];
}

export function overview(): Overview {
  const all = routes();
  return {
    route: all[0] ?? null,
    lastSuccessfulRoute: null,
    quota: {
      routeId: "grok-cli",
      usedPercent: 43,
      period: { unit: "week", amount: null },
      resetAt: "2026-07-28T13:23:39Z",
      tier: "SuperGrok",
      stale: false,
    },
    usage: {
      usedTokens: 190_240,
      windowTokens: 500_000,
      turns: 14,
      providerTotalTokens: 4_090_000_000,
      trend: [0.12, 0.15, 0.21, 0.2, 0.28, 0.31, 0.36, 0.33, 0.41, 0.44, 0.48, 0.52],
      compacted: false,
    },
    health: {
      pooled: true,
      connections: 1,
      firstByteMs: 412,
      reasoningVisible: true,
      historyRetentionDays: 30,
    },
    findings: [
      {
        id: "f1",
        severity: "warning",
        title: "上下文接近壓縮門檻",
        location: "80% 觸發 · 目前 38%，約再 12 次往返",
      },
      {
        id: "f2",
        severity: "info",
        title: "weikuwu 的 GLM-5.2 未設定視窗",
        location: "正使用保底值 121,600",
      },
    ],
  };
}

export function probe(endpoint: string): ProbeResult {
  const isGrok = endpoint.includes("grok");
  return {
    reachable: true,
    wire: isGrok ? "responses" : "chat",
    models: isGrok ? ["grok-4.5"] : ["GLM-5.2", "GLM-5.2p", "DeepSeek-V4-Flash"],
    contextWindow: isGrok ? 500_000 : null,
    streaming: true,
    reasoning: isGrok,
    serverSideResume: false,
    modelCapabilities: (isGrok
      ? ["grok-4.5"]
      : ["GLM-5.2", "GLM-5.2p", "DeepSeek-V4-Flash"]
    ).map((model) => ({
      model,
      contextWindow: isGrok ? 500_000 : null,
      wire: isGrok ? "responses" : "chat",
      streaming: true,
      reasoning: isGrok || model.startsWith("GLM") || model.startsWith("DeepSeek"),
      toolCalling: true,
      probeVersion: 4,
    })),
    needsInput: isGrok ? [] : ["contextWindow"],
  };
}

export function budget(routeId: string, override?: number | null): ContextBudget {
  const hasOverride = override != null && override > 0;
  return {
    routeId,
    catalogId: routeId,
    model: routeId === "weikuwu" ? "GLM-5.2" : "grok-4.5",
    source: hasOverride ? "override" : "modelCache",
    contextWindow: 500_000,
    effectivePercent: 95,
    effectiveWindow: hasOverride ? override : 475_000,
    overrideTokens: hasOverride ? override : null,
    compactThresholdPercent: 80,
  };
}

export function reviewSettings(): ReviewSettings {
  return {
    onEdit: false,
    beforeSend: true,
    beforeCompact: true,
    routeId: "grok-cli",
    model: "grok-4.5",
    policy: "failover",
    fallbackCatalogId: "gpt-5.6-sol",
  };
}

export function subagentSettings(): SubagentSettings {
  return {
    mode: "inherit",
    routeId: null,
    catalogId: null,
    reasoningEffort: null,
  };
}

/**
 * 瀏覽器預覽用的網頁搜尋設定。
 *
 * 刻意給「開著、但沒有金鑰」—— 那正是畫面要考驗的形狀：Brave 是唯一的後端，
 * 沒有金鑰查詢就一律失敗。給預設值看不出這件事。
 */
export function webSearchSettings(): WebSearchSettingsView {
  return {
    settings: {
      enabled: true,
      mode: "live",
      domainPolicy: { allow: [], block: ["example-spam.test"] },
      searchContextSize: "medium",
    },
    hasBraveApiKey: false,
  };
}

/**
 * 一次真的發生過的壓縮，段落切法與 tone 沿用 `compaction.rs::build_preview`。
 *
 * 以前這裡回傳全 0、segments 空陣列，於是預覽台上半頁說「這個對話還沒有發生
 * 過壓縮」，下半頁卻攤著整份壓縮原文 —— 同一畫面自相矛盾，看的人會先懷疑是
 * 畫面壞了。mock 要跟 `compactionTranscript()` 講同一次壓縮。
 */
export function compaction(): CompactionPreview {
  return {
    beforeTokens: 372_000,
    afterTokens: 96_400,
    afterTokensExact: true,
    keepRecentTurns: 6,
    engine: "local_canonical",
    readableReplayTokens: 0,
    crossSessionTokens: 0,
    crossSessionAvailable: false,
    segments: [
      {
        kind: "retained_context",
        label: "系統與指示",
        before: 18_400,
        after: 18_400,
        tone: "var(--haze)",
      },
      {
        kind: "canonical_checkpoint",
        label: "早期往返 → 摘要",
        before: 291_300,
        after: 15_700,
        tone: "var(--coral)",
      },
      {
        kind: "retained_context",
        label: "近期往返",
        before: 62_300,
        after: 62_300,
        tone: "var(--honey)",
      },
    ],
  };
}

export function compactionDetail(): CompactionDetail {
  return {
    originAvailable: false,
    unavailableReason:
      "Codex owns compaction. Vellum can show the observed event, but the runtime did not expose source or replacement items.",
    checkpointId: "codex-client-demo",
    createdAt: Math.floor(Date.now() / 1000),
    trigger: "codex_auto",
    provider: "Codex Desktop",
    model: null,
    sourceLabel: "Example runtime-owned compaction",
    canonicalKind: "CodexClient",
    schemaVersion: null,
    sourceTokens: 38_400,
    canonicalTokens: 2_140,
    canonicalHash: null,
    exactRecovery: false,
    portableAvailable: false,
    encryptedBytes: 0,
    officialMode: null,
    canonicalItems: [],
    summary: null,
    items: [],
  };
}

export function compactionSnapshot(): CompactionSnapshot {
  const detail = compactionDetail();
  return {
    preview: {
      beforeTokens: detail.sourceTokens,
      afterTokens: detail.canonicalTokens ?? 0,
      afterTokensExact: detail.canonicalTokens !== null,
      keepRecentTurns: 0,
      engine: "codex_client",
      readableReplayTokens: 0,
      crossSessionTokens: 0,
      crossSessionAvailable: false,
      segments: [
        {
          kind: "observed_total",
          label: "Codex client observed window",
          before: detail.sourceTokens,
          after: detail.canonicalTokens ?? 0,
          tone: "var(--sage)",
        },
      ],
    },
    detail,
  };
}

const PROVIDERS = ["Grok Build", "weikuwu", "OpenAI"];
const ROUTE_IDS: Record<string, string> = {
  "Grok Build": "grok-cli",
  weikuwu: "weikuwu",
  OpenAI: "openai",
};
const MODELS: Record<string, string> = {
  "Grok Build": "grok-4.5",
  weikuwu: "GLM-5.2",
  "OpenAI": "gpt-5.6-sol",
};
const LONG_ERRORS = [
  "upstream connect error or disconnect/reset before headers. reset reason: connection termination",
  "429 Too Many Requests: rate limit exceeded for model grok-4.5, retry after 37s",
  "context_length_exceeded: requested 512000 tokens but the model supports at most 500000",
];

export function bootTelemetry(): BootTelemetry {
  const now = Math.floor(Date.now() / 1000);
  return {
    bootCount: 1,
    startedAt: now,
    previousStartedAt: null,
    pid: 4242,
  };
}

export function requestLog(keep = 240): RequestLog {
  const now = Math.floor(Date.now() / 1000);
  const entries: RequestLogEntry[] = [];
  for (let i = 0; i < Math.min(keep, 240); i += 1) {
    const provider = PROVIDERS[i % PROVIDERS.length] ?? "Grok Build";
    const failed = i % 11 === 7;
    entries.push({
      id: 10_000 - i,
      routeId: ROUTE_IDS[provider] ?? "grok-cli",
      provider,
      model: MODELS[provider] ?? "unknown",
      inputTokens: 1_200 + ((i * 977) % 46_000),
      outputTokens: failed ? 0 : 180 + ((i * 331) % 6_400),
      status: failed ? [429, 500, 400][i % 3] ?? 500 : 200,
      error: failed ? (LONG_ERRORS[i % LONG_ERRORS.length] ?? null) : null,
      durationMs: 420 + ((i * 613) % 18_000),
      firstByteMs: failed ? null : 180 + ((i * 97) % 900),
      createdAt: now - i * 36_000,
    });
  }
  const providers: ProviderUsage[] = PROVIDERS.map((name) => {
    const mine = entries.filter((e) => e.provider === name);
    return {
      provider: name,
      requests: mine.length,
      inputTokens: mine.reduce((s, e) => s + e.inputTokens, 0),
      outputTokens: mine.reduce((s, e) => s + e.outputTokens, 0),
      failedRequests: mine.filter((e) => e.status >= 400).length,
    };
  });
  return { entries, providers };
}

export function usageActivity(): UsageActivity {
  const log = requestLog();
  const byDate = new Map<string, { tokens: number; requests: number }>();
  for (const entry of log.entries) {
    const date = new Date(entry.createdAt * 1000).toISOString().slice(0, 10);
    const day = byDate.get(date) ?? { tokens: 0, requests: 0 };
    day.tokens += entry.inputTokens + entry.outputTokens;
    day.requests += 1;
    byDate.set(date, day);
  }
  const days = [...byDate]
    .map(([date, value]) => ({ date, ...value }))
    .sort((left, right) => left.date.localeCompare(right.date));
  const providers = log.providers.map((provider, index) => ({
    routeId: provider.provider.toLowerCase().replaceAll(" ", "-"),
    provider: provider.provider,
    tokens: provider.inputTokens + provider.outputTokens,
    accountCount: index === 2 ? 2 : 0,
    source: (index === 2 ? "codex_profile" : "proxy") as
      | "codex_profile"
      | "proxy",
  }));
  return {
    days,
    totalTokens: providers.reduce((sum, provider) => sum + provider.tokens, 0),
    peakTokens: Math.max(0, ...days.map((day) => day.tokens)),
    longestTaskDurationMs: Math.max(
      0,
      ...log.entries.map((entry) => entry.durationMs),
    ),
    currentStreakDays: 1,
    longestStreakDays: 12,
    providers,
    officialSource: "codex_profile",
    warning: null,
  };
}


export function quotaWindowsFor(routeId: string): QuotaSnapshot[] {
  if (routeId === "grok-cli") {
    return [
      {
        routeId,
        usedPercent: 43,
        period: { unit: "week", amount: null },
        resetAt: "2026-07-28T13:23:39Z",
        tier: "SuperGrok",
        stale: false,
      },
    ];
  }
  if (routeId === "openai") {
    return [
      {
        routeId,
        usedPercent: 82,
        period: { unit: "hour", amount: 5 },
        resetAt: "2026-07-27T18:00:00Z",
        tier: "Plus",
        stale: false,
      },
      {
        routeId,
        usedPercent: 31,
        period: { unit: "week", amount: null },
        resetAt: "2026-08-01T00:00:00Z",
        tier: "Plus",
        stale: false,
      },
    ];
  }
  return [];
}

export function activityFor(routeId: string): {
  latestInputTokens: number;
  turns: number;
  firstByteMs: number | null;
} {
  if (routeId === "grok-cli") return { latestInputTokens: 190_240, turns: 14, firstByteMs: 412 };
  if (routeId === "openai") return { latestInputTokens: 84_610, turns: 6, firstByteMs: 733 };
  return { latestInputTokens: 0, turns: 0, firstByteMs: null };
}

export function reviewStats(): ReviewStats {
  const now = Math.floor(Date.now() / 1000);
  return {
    totalRuns: 128,
    fallbackRuns: 23,
    activeRouteId: "openai",
    activeModel: "gpt-5.6-sol",
    activeIsFallback: true,
    activeReason: "Grok Build unavailable (HTTP 429)",
    providers: [
      {
        routeId: "grok-cli",
        provider: "Grok Build",
        model: "grok-4.5",
        primaryRuns: 105,
        fallbackRuns: 0,
        failedRuns: 4,
        lastUsedAt: now - 2_400,
      },
      {
        routeId: "openai",
        provider: "OpenAI",
        model: "gpt-5.6-sol",
        primaryRuns: 0,
        fallbackRuns: 23,
        failedRuns: 0,
        lastUsedAt: now - 180,
      },
    ],
  };
}

/**
 * 工作階段。
 *
 * id 刻意用實機真正會存的那種形狀 —— `codex:<session>:<thread>` ——
 * 而不是好看的短雜湊。實機上每一列都長這樣，所以預覽台上也必須長這樣，
 * 否則「沒有標題時要顯示什麼」這件事在預覽台永遠試不出來。
 *
 * 數量刻意超過清單一次顯示的上限，這樣「最近 5 個」跟「搜尋能翻到更舊的」
 * 兩件事才有東西可以驗。
 */
export function sessions(): SessionStatus[] {
  const now = Math.floor(Date.now() / 1000);
  const rows: Array<
    [string, string | null, string, string, string, number, number, number, string | null]
  > = [
    [
      "01a079b3-33f3-7bb3-a4c9-e60261a5267d",
      "Vellum Proxy 併發重構",
      "grok-cli",
      "Grok Build",
      "grok-4.5",
      372_000,
      500_000,
      90,
      "enhanced",
    ],
    [
      "01a0756d-5919-77e0-9271-e847988268df",
      "幫我仔細調查工作階段監看為甚麼失效",
      "weikuwu",
      "weikuwu",
      "GLM-5.2",
      96_400,
      272_000,
      420,
      "enhanced",
    ],
    [
      "01a076c0-96a0-7883-a260-83a667adac55",
      null,
      "weikuwu",
      "weikuwu",
      "GLM-5.2",
      18_200,
      121_600,
      1_800,
      "enhanced",
    ],
    [
      "01a079ce-40b4-71c1-ae1a-5069994f882c",
      "Enhanced core 交接文案",
      "grok-cli",
      "Grok Build",
      "grok-4.5",
      210_500,
      500_000,
      5_400,
      "enhanced",
    ],
    [
      "01a077a1-438b-7b83-98fb-d545818f6d7c",
      "遠端主機配對重試",
      "weikuwu",
      "weikuwu",
      "GLM-5.2",
      44_900,
      121_600,
      9_000,
      "enhanced",
    ],
    [
      "01a07766-2f59-7f51-aef4-54eb19cfdeb1",
      "四語排版與字級",
      "grok-cli",
      "Grok Build",
      "grok-4.5",
      88_100,
      500_000,
      26_000,
      "enhanced",
    ],
    [
      "01a076e8-ffd2-7c03-a88e-e274c73385e7",
      "整理本機 log",
      "weikuwu",
      "weikuwu",
      "GLM-5.2",
      12_400,
      121_600,
      92_000,
      "enhanced",
    ],
    [
      "01a079cd-4e59-7753-9547-a52f8d31420e",
      "官方帳號切換",
      "openai",
      "OpenAI",
      "gpt-5.6-sol",
      63_000,
      272_000,
      300,
      "official",
    ],
  ];
  return rows.map(
    ([
      thread,
      label,
      routeId,
      provider,
      model,
      usedTokens,
      windowTokens,
      ago,
      core,
    ]) => ({
      id: `codex:${thread}:${thread}`,
      label,
      routeId,
      provider,
      model,
      usedTokens,
      windowTokens,
      compactThresholdPercent: 80,
      lastActivityAt: now - ago,
      core: core as SessionStatus["core"],
    }),
  );
}

/**
 * 一次壓縮的原文。
 *
 * prompt 刻意放真的那種長度與語氣 —— 短短一句假資料看不出「這一塊會不會
 * 把整頁撐爆」，而那正是這張卡唯一的版面風險。
 */
export function compactionTranscript(): CompactionTranscript {
  return {
    checkpointId: "cmp_vellum_9f21",
    createdAt: Math.floor(Date.now() / 1000) - 1_260,
    prompt: [
      "You are compacting a long coding conversation so it can continue in a",
      "smaller context window. Produce a structured handover that the next",
      "turn can act on without re-reading anything that came before.",
      "",
      "Rules:",
      "- Keep every decision the user made, in their own words where possible.",
      "- Keep file paths, commands and test names verbatim. Never paraphrase a path.",
      "- Keep unresolved questions and anything the user explicitly refused.",
      "- Drop tool output that has been superseded by a later result.",
      "- Do not invent progress. If something was attempted and failed, say so.",
      "",
      "Return the handover as JSON with the fields: goal, acceptanceCriteria,",
      "constraints, userPreferences, done, inProgress, blocked, decisions,",
      "changedFiles, relevantFiles, commands, tests, unresolved, errors,",
      "criticalContext, references, nextSteps.",
    ].join("\n"),
    result: [
      "目標：把上下文壓縮頁改成以工作階段為主軸。",
      "",
      "使用者偏好",
      "- 先有概念再有元件；不要角落 logo、置中英雄區、三卡並排。",
      "- 搜尋以標題為主，不要比對 Provider 與模型。",
      "",
      "限制",
      "- 不要動 Codex core 的 build cache。",
      "- 不要消耗 OpenAI 官方模型額度。",
      "",
      "已完成",
      "- 清單改成最近五個，按最近動過排序。",
      "- 模型頁每一列的視窗長度鉛筆（後端 setBudgetOverride 已完成）。",
      "",
      "進行中",
      "- 這張原文卡的後端指令 get_compaction_transcript。",
      "",
      "關鍵背景",
      "- 實機的 conversation key 是 codex:<session>:<thread>，不是 sha256。",
      "  比對身分一律走 conversation_key_matches，不要自己 hash。",
      "",
      "下一步",
      "- 四語文案覆核。",
    ].join("\n"),
    unavailableReason: null,
  };
}

/**
 * 一個 ChatGPT 帳號手上的 Reset 券。
 *
 * 到期時間刻意攤成四種情況，因為到期清單要處理的就是這四種：快到期的、
 * 還很久的、上游沒給到期時間的，以及已經用掉的。全部給同一種「還有 10
 * 天」看起來很整齊，但那樣的畫面沒有被檢查過。
 */
export function resetCredits(accountId: string): CodexResetCredits {
  const inHours = (hours: number) =>
    new Date(Date.now() + hours * 3_600_000).toISOString();
  const credits: CodexResetCredit[] = [
    {
      id: `${accountId}-rc-1`,
      resetType: "weekly",
      status: "available",
      expiresAt: inHours(19),
      title: "每週視窗",
      description: "清掉每週上限一次。",
    },
    {
      id: `${accountId}-rc-2`,
      resetType: "primary",
      status: "available",
      expiresAt: inHours(24 * 9 + 5),
      title: "5 小時視窗",
      description: "清掉 5 小時上限一次。",
    },
    {
      id: `${accountId}-rc-3`,
      resetType: null,
      status: "available",
      expiresAt: null,
      title: null,
      description: null,
    },
    {
      id: `${accountId}-rc-4`,
      resetType: "primary",
      status: "consumed",
      expiresAt: inHours(-52),
      title: "5 小時視窗",
      description: null,
    },
  ];
  return {
    availableCount: credits.filter((credit) => credit.status === "available").length,
    credits,
  };
}

/**
 * 預覽台的 ChatGPT 登入狀態。
 *
 * 預設是「還沒登入」，因為 onboarding 的三條路都要從那裡開始看。想看已登
 * 入之後的樣子（Reset 到期清單就長在那裡），在網址後面加 `?accounts=1`。
 * 用旗標而不是直接把預設改成已登入：改掉預設等於把 onboarding 的預覽台
 * 弄壞，而那個壞法不會有人發現，只會在某天覺得「怎麼看不到登入那一步」。
 */
export function codexOAuthStatus(): CodexOAuthStatus {
  const wantsAccounts =
    typeof window !== "undefined" &&
    new URLSearchParams(window.location.search).get("accounts") === "1";
  if (!wantsAccounts) {
    return {
      authenticated: false,
      defaultAccountId: null,
      selectionRevision: 0,
      selectionVerified: false,
      selectedAt: null,
      accounts: [],
    };
  }
  const now = Math.floor(Date.now() / 1000);
  return {
    authenticated: true,
    defaultAccountId: "acct-preview-1",
    selectionRevision: 3,
    selectionVerified: true,
    selectedAt: now - 900,
    accounts: [
      {
        accountId: "acct-preview-1",
        email: "joe@example.com",
        authenticatedAt: now - 86_400,
        isDefault: true,
      },
      {
        accountId: "acct-preview-2",
        email: "team@example.com",
        authenticatedAt: now - 86_400 * 12,
        isDefault: false,
      },
    ],
  };
}
