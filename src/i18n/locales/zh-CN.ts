import type { ResourceTree } from "../resources";

const zhCN: ResourceTree = {
  "enhanced": {"heading":"Enhanced 运行状态","lead":"显示每条路由的执行者、兼容性检查结果，以及手机连接状态。","title":"Enhanced Core","subtitle":"执行者、兼容性证据与手机连接","serving":"Enhanced Core 运行中","notServing":"Enhanced Core 未运行","sessions":"已观察到的会话","running":"运行中的回合","freshness":"数据时间","restartRequired":"已准备好新的启动配置。请在运行中的回合结束后再安全重启。","launchCoreDrift":"Codex Desktop 已更新内核。Vellum 会在回合空闲时自动重启 Proxy；完成后请重新启动 Codex。","now": "现在", "runningTurns_one": "{{count}} 个回合执行中", "runningTurns_other": "{{count}} 个回合执行中", "sessionCount_one": "{{count}} 个会话", "sessionCount_other": "{{count}} 个会话", "verdict": {"serving": "Codex Desktop 目前经由 Enhanced Core 运行。", "servingOtherLaunch": "Enhanced 仍在运行，但沿用上一次启动的配置。重启 Codex 后才会切换为这次的配置。", "stoppedAt": "启动流程停在「{{link}}」阶段。", "disabled": "Enhanced Core 已停用，Codex Desktop 正在使用原生内核。"}, "chain": {"title": "启动流程", "artifact": "组件验证", "armed": "路径接管", "adopted": "Bridge 接手", "inPlace": "运行确认"}, "mark": {"blocked": "未通过", "waiting": "等待中", "inPlace": "已生效"}, "whyStopped": "在「{{link}}」阶段停下的原因", "whyNoted": "运行中，但有几点需要注意", "protocol": {"verified": "已验证", "unverified": "未验证", "incompatible": "不兼容"}, "protocolUnrun": "比对尚未执行", "unverifiedNote": "这个 Codex Desktop 版本没有经过验证。路由会用到的方法都还在，所以路由没有问题；下面列出的差异确实存在，但未经测试 —— 可以用，但不保证正确。", "missingHelpers": "Enhanced 内核少了官方安装中有的辅助程序：{{list}}。对话与路由不受影响，只有沙箱命令执行会失败。", "routed": "路由所需", "delta": {"methodUnservable": "此方法无法提供", "methodUnknownToDesktop": "Desktop 未声明此方法", "fieldNewlyRequired": "字段变成必填", "fieldRemoved": "字段已移除"}, "environment": {"leased": "持有中", "released": "已交还", "orphanedBridge": "残留的旧 Bridge", "foreignValue": "由其他工具设置", "unreadable": "无法读取"}, "bridgeProcess": "Bridge 进程", "bridgeState": "Bridge 状态", "children": "子进程", "launch": "启动", "relayNote": "手机要连上，必须通过握手、任务列表、消息加载、实时事件与任务控制五个阶段；哪一关没过，就是连不上的原因。", "staleWarning":"显示的是最后一次观察结果，可能已经过期。","compatibility":"版本与兼容性","activeRuntime":"当前实际使用的 runtime","candidateRuntime":"下次启动使用的 runtime","structure":"Schema 比较","qualification":"最近验证","notObserved":"尚未观察","observed":"已观察","evidenceNote":"Schema 一致只能算是结构上的证据，不能证明 Desktop 或手机的行为已经验证过。","differences":"差异与阻碍","relay":"Relay 状态","clients":"已连接的客户端版本","handshake":"握手","list":"任务列表","history":"消息加载","stream":"实时事件","control":"任务控制","failureStage":"最后一次失败的阶段","plane":"执行者","all":"全部","empty":"这次启动还没有观察到任何会话。过去的绑定不等于当前的连接。","session":"会话","state":"状态","lastActivity":"最后活动时间","parent":"父任务","diagnostics":"诊断","recheck":"重新检查兼容性","export":"导出诊断","exported":"已保存","states":{"starting":"启动中", "ready":"就绪", "degraded":"降级", "stopped":"已停止", "transportReady":"传输就绪", "unknown":"未知","current":"最新","stale":"已过期","offline":"离线","unavailable":"没有数据","running":"运行中","idle":"空闲","approval":"等待批准","unloaded":"已卸载","observed":"已观察","attached":"已连接","failed":"失败"}},
  "common": { "expiresAt": "到期时间：{{month}}月{{day}}日 {{time}}（{{zone}}）", "expiredAlready": "已过期", "expiresInMinutes": "剩 {{value}} 分钟", "expiresInHours": "剩 {{value}} 小时", "expiresInDays": "剩 {{value}} 天","emDash": "—", "none": "—", "listSeparator": "；", "itemSeparator": "、", "loading": "读取中…", "processing": "处理中…", "refreshing": "更新中…", "refresh": "刷新", "reorganize": "重新整理", "save": "保存", "cancel": "取消", "confirm": "确认", "close": "关闭", "remove": "移除", "enabled": "启用", "disabled": "停用", "yes": "是", "no": "否", "on": "开", "off": "关", "unknown": "未知", "provider": "Provider", "model": "模型", "proxy": "Proxy", "codexCatalog": "Codex 模型菜单", "justNow": "刚刚", "minutesAgo": "{{count}} 分钟前", "hoursAgo": "{{count}} 小时前", "daysAgo": "{{count}} 天前", "resetAt": "{{month}}月{{day}}日 {{time}} 重置", "previousPage": "上一页", "nextPage": "下一页", "pageOf": "第 {{page}} / {{total}} 页", "shortcutTitle": "{{label}}（Ctrl+{{n}}）", "statusUpdateFailed": "状态更新失败：{{detail}}", "errorWithDetail": "{{message}}：{{detail}}", "connectionFailed": "连接失败", "operationFailed": "操作失败", "loadFailed": "加载失败", "saveFailed": "保存失败", "visible": "显示", "hidden": "隐藏", "totalItems_one": "共 {{count}} 条", "totalItems_other": "共 {{count}} 条", "itemsNeedAttention_one": "个项目需要处理", "itemsNeedAttention_other": "个项目需要处理", "notice": {"info": "提示", "warn": "注意", "error": "失败"}, "dismiss": "知道了"},
  "navigation": {
    "mainAria": "主导航",
    "today": {
      "label": "现状",
      "title": "现状",
      "blurb": "Proxy、Provider 与额度状态"
    },
    "models": {
      "label": "模型",
      "title": "模型",
      "blurb": "Provider 与 Codex 模型菜单"
    },
    "context": {
      "label": "上下文",
      "title": "上下文",
      "blurb": "上下文用量、压缩预览与原文"
    },
    "enhanced": {
      "label": "增强",
      "title": "增强",
      "blurb": "执行者与兼容性状态"
    },
    "remote": {
      "label": "远程",
      "title": "远程总管",
      "blurb": "Codex App SSH 主机与原生 daemon"
    },
    "log": {
      "label": "记录",
      "title": "记录",
      "blurb": "Token 统计与请求记录"
    },
    "updates": {
      "label": "更新",
      "blurb": "检查、下载并应用全部更新"
    },
    "settings": {
      "label": "设置",
      "title": "设置",
      "blurb": "应用程序与 Codex 集成设置"
    }
  },
  "status": {
    "live": "生效中",
    "pending": "待生效",
    "off": "已停用",
    "reading": "读取中…",
    "enhancedLoaded": "已加载",
    "enhancedNotLoaded": "未加载",
    "enhancedUnverified": "已加载（未验证）",
    "proxyStopped": "Proxy 未启动",
    "codexNotManaged": "Codex 未导向 Vellum",
    "lastSuccessfulRequest": "最近成功请求",
    "defaultRoute": "默认路由",
    "noProviderYet": "还没有 Provider",
    "quotaRemaining": "额度剩 {{percent}}%",
    "restartCodexRequired": "需重启 Codex",
    "updateAvailable": "更新可用",
    "updateWaitingIdle": "已下载，等待空闲",
    "updateWaitingRestart": "已下载，下次启动时应用",
    "updateFailed": "更新失败或已回退",
    "remedy": {
      "startProxy": "启动 Proxy 后生效",
      "restartProxy": "重启 Proxy 后生效",
      "restartCodex": "重启 Codex 后会出现在模型菜单"
    }
  },
  "vocabulary": {
    "quotaUnavailable": "这个端点不提供额度查询",
    "source": {
      "override": "你手动填的值",
      "modelCache": "Provider 的模型缓存",
      "catalog": "Codex 模型菜单",
      "fallback": "保底值"
    }
  },
  "quota": {
    "period": {
      "week": "周额度",
      "month": "月额度",
      "hours_one": "{{count}} 小时额度",
      "hours_other": "{{count}} 小时额度",
      "days_one": "{{count}} 天额度",
      "days_other": "{{count}} 天额度",
      "unspecified": "使用额度"
    },
    "remainingLabel": "{{period}}剩余 {{remaining}}%"
  },
  "review": {
    "policy": {
      "always": {
        "label": "仅使用指定模型",
        "blurb": "一律使用指定模型。该 Provider 额度耗尽时，自动审查就会停止。"
      },
      "failover": {
        "label": "指定模型优先，必要时使用备用",
        "blurb": "指定模型额度耗尽或连接失败时，自动切换至备用模型以继续审查。"
      }
    }
  },
  "heatmap": {
    "tokens": "{{value}} 个 token",
    "gridAria": "token 用量热区图 · {{mode}}",
    "mode": {
      "daily": "每日",
      "weekly": "每周",
      "cumulative": "累计"
    },
    "monthLabel": "{{month}}月",
    "aria": "使用量热区图",
    "tooltip": "{{from}} – {{to}} · {{requests}} 次请求 · {{tokens}} tokens",
    "range": "{{from}} 至 {{to}}",
    "requestsTokens_one": "{{count}} 次请求 · {{tokens}} tokens",
    "requestsTokens_other": "{{count}} 次请求 · {{tokens}} tokens"
  },
  "runtime": {"notice": {"codexConfigStillPointedAtVellum": "Codex 的配置仍指向 Vellum。启动 Proxy 后重新启动 Codex App 即可继续使用。", "codexRunning": "Codex 正在运行；请重新启动以载入 Proxy 与模型菜单", "proxyStoppedCodexRestartRequired": "Proxy 已停止并还原设置；请重新启动 Codex 以离开旧的 Proxy／Bridge 连接", "codexRestartDetected": "已检测到 Codex 重新启动；Proxy 与模型菜单已载入", "routesAndCatalogUpdated": "Provider 设置与模型菜单已更改；重新启动 Vellum Proxy 与 Codex 后生效", "catalogRestored": "已还原模型菜单；Codex 需要重新启动", "catalogUpdated": "模型菜单已更新", "enhancedDesktopRuntimeChanged": "Enhanced Codex Runtime 设置已更改；请重启 Codex 以应用", "enhancedLaunchCoreRepaired": "Codex Desktop 已更新内核，Vellum 已自动重启 Proxy 并准备好新的启动配置；请重新启动 Codex（原启动 {{launchId}}）", "enhancedLaunchCoreRepairFailed": "Codex Desktop 更新后，Vellum 无法自动重启 Proxy：{{reason}}。请手动重启 Proxy，再重新启动 Codex。", "remoteOfficialAccountSwitchNotFollowed": "远程主机 {{host}} 没有跟着切换账号。请打开 Remote Manager 进行配对或同步。原因：{{detail}}", "officialAccountSwitchNativePlane": "Official GPT 仍使用未修改的 Codex 核心；只要 Enhanced bridge 在运行，Vellum 的账号切换就不会取代 Codex Desktop 的登录身份", "enhancedDesktopBridgeReady": "Codex 已重新启动，并改走 Enhanced bridge（启动 {{launchId}}）", "enhancedDesktopBridgeFailed": "Codex 已重新启动，但 Enhanced bridge 失败：{{reason}}", "enhancedDesktopRuntimeNotArmed": "Proxy 已启动，但 Enhanced Codex 没有生效：第三方 Provider 的对话会由原生 Codex 执行。原因：{{detail}}", "enhancedDesktopBridgeNotObserved": "Codex 已重新打开，但没有任何 Enhanced bridge 回报启动 {{launchId}}；Enhanced 并未运行", "routeHotSwapped": "路由已即时切换", "slotSwitched": "{{slot}} 已切换至 {{model}}", "restartSucceeded": "Codex 已安全重新启动；新的运行时设置已加载", "restartNotDetected": "已发出启动 Codex 的请求，但 5 秒内没有检测到 App；请手动打开", "restartProcessStillRunning": "Codex 尚未完全退出（退出代码 {{exitCode}}）；已取消重新启动", "restartBlockedByActiveRequests_one": "仍有 {{count}} 个请求执行中", "restartBlockedByActiveRequests_other": "仍有 {{count}} 个请求执行中", "restartBlockedByCodexTurn_one": "Codex Desktop 有 {{count}} 个对话正在进行；现在重启会丢失它还没写入磁盘的内容", "restartBlockedByCodexTurn_other": "Codex Desktop 有 {{count}} 个对话正在进行；现在重启会丢失它们还没写入磁盘的内容", "restartExecutableMissing": "找不到可验证的 Codex App 执行文件", "restartExecutableInvalid": "Codex App 执行文件不存在或不是绝对路径", "restartUnavailableInPreview": "浏览器预览不会重新启动 Codex", "usageOAuthNotConfigured": "Vellum 尚未配置 OpenAI OAuth；OpenAI 的用量暂以 Proxy 记录推算", "usageProfileUnavailable_one": "{{count}} 个 OpenAI OAuth 账号的 Codex Token 统计暂时无法读取", "usageProfileUnavailable_other": "{{count}} 个 OpenAI OAuth 账号的 Codex Token 统计暂时无法读取", "webSearchDisabledMissingBraveKey": "由于未设置 Brave Search API 密钥，第三方网页搜索已被停用。请在设置中添加密钥以重新启用。"}, "alerts": {"label": "运行时警告"}},
  "settings": {
    "language": {
      "title": "语言",
      "blurb": "选择界面语言。选「跟随系统」会按操作系统语言自动判定。",
      "option": {
        "system": "跟随系统",
        "zh-TW": "繁體中文",
        "zh-CN": "简体中文",
        "en": "English",
        "ja": "日本語"
      },
      "applied": "语言已应用"
    },
    "title": "设置",
    "page": {
      "subagent": { "title": "子代理默认设置", "description": "默认情况下，子代理使用发起它那次对话的模型与推理强度。这里可以改成固定的默认值；任务若明确指定模型或推理强度，仍以任务指定为优先。", "desktopReady": "Codex Desktop 已就绪", "desktopUnavailable": "Codex Desktop 无法应用此设置", "desktopVersion": "Desktop 版本 {{version}} · 原生子代理默认设置可用", "defaultsTitle": "未指定时使用的模型", "mode": { "inherit": "保留 Desktop 现有设置", "custom": "由 Vellum 指定默认" }, "effort": "推理强度", "autoEffort": "自动（模型默认）", "effortNotProbed": "此模型的推理强度尚未验证；请先到「模型」页探测能力。", "modelEmpty": "此 Provider 目前没有可选的模型。", "hint": "这些只是默认值；代理可依任务明确指定其他已启用的模型与推理强度。", "unavailable": "所选的 Provider 或模型目前已不可用。依赖此默认值建立的子代理会明确失败，直到更新设置。", "unsupported": "无法验证 Codex Desktop 的原生子代理设置：{{detail}}" },
      "loading": "正在加载设置…", "heading": "审查、搜索、显示与系统恢复", "unspecified": "未指定", "saved": "设置已保存",
      "review": { "title": "自动审查", "toggle": "启用自动批准审查", "description": "由指定模型评估 Codex 提出的批准请求。本功能不改变沙箱、网络或文件访问权限，仅调整批准决策所使用的审查模型。", "billingHelp": "额度如何计算？", "billingHelpTitle": "自动审查与 ChatGPT 额度", "billingHelpFacts": { "enabled": { "title": "自动审查开启", "body": "Official 审查使用此处指定的计费账号；选择“跟随当前使用的账号”时，才沿用一般 ChatGPT 账号路由。" }, "disabled": { "title": "自动审查关闭", "body": "Vellum 不再指定审查专用账号。Official 请求改走一般 ChatGPT 账号路由，通常计入模型页当前选择的账号。" }, "pool": { "title": "额度池例外", "body": "额度池启用且已有成员时，一般 Official 请求由额度池选择可用账号，因此不一定计入模型页手动选择的账号。" }, "native": { "title": "未由 Vellum 管理账号", "body": "若 Vellum 没有管理任何 ChatGPT 账号，请求会沿用 Codex 传入的原生登录账号。第三方 Provider 则使用该 Provider 自己的额度。" } }, "strategyTitle": "审查模型策略", "currentLabel": "当前生效", "fallbackActive": "使用备用模型", "selectedUnavailable": "指定模型当前不可用", "provider": "指定 Provider", "model": "指定模型", "fallbackModel": "备用模型", "billingAccount": "计费账号", "billingFollowsDefault": "跟随当前使用的账号", "billingAccountMissing": "账号 {{account}} 未在本机登录。自动审查不会改用其他账号计费，而是直接失败；请从列表重选，或重新登录该账号。", "savedRemotePending": "已保存，本机 Proxy 立即生效；{{count}} 台远程主机需要到 Remote Manager 重新应用。", "rulesTitle": "自动审查规则", "willReview": "审查范围", "willReviewValue": "需要离开沙箱、访问受限网络或受保护路径的操作", "willNotReview": "排除范围", "willNotReviewValue": "已由沙箱策略允许的操作", "whenRejected": "审查未通过", "whenRejectedValue": "Codex 将改用风险较低的替代方案，不执行原批准请求" },
      "stats": { "title": "各 Provider 审查次数", "summary": "备用模型 {{fallback}} / {{total}} 次（{{percent}}%）", "primary": "指定", "fallback": "备用模型", "failed": "失败", "lastUsed": "最后使用时间", "neverUsed": "暂无记录", "empty": "自动审查当前暂无执行记录。Codex 下一次提出批准请求后，系统将记录实际使用的 Provider。" },
      "webSearch": { "title": "网页搜索", "toggle": "启用第三方 Provider 网页搜索", "description": "启用后，非官方 Provider 可使用 Codex 网页搜索工具，查询由 Brave Search 处理。停用时，Vellum 会在转发请求前移除搜索工具。OpenAI 官方 Provider 使用原生搜索服务，不受此设置影响。", "braveKey": "Brave Search API 密钥", "braveKeySaved": "已设置；输入新密钥即可更新", "braveKeyPlaceholder": "输入 Brave Search API 密钥", "braveKeyHint": "API 密钥会以加密形式存储在本机，不写入常规设置文件，也不会提供给模型。", "reach": "网页访问范围", "reachOption": { "indexed": "仅使用索引结果", "live": "允许获取公开网页" }, "reachHint": { "indexed": "模型仅接收搜索后端返回的标题与摘要，不获取原始网页内容。", "live": "模型可获取搜索结果中的公开 HTTP(S) 网页；本机、私有网络及云端元数据地址均会被拦截。" }, "probe": "搜索连接测试", "probeAction": "执行测试", "probing": "正在测试…", "probeHint": "发送最小查询以验证 Brave Search 的连接与响应状态。", "probeOk": "Brave Search 已返回 {{count}} 条结果", "probeEmpty": "Brave Search 连接正常，但未返回搜索结果", "probeFailed": "连接测试失败：{{detail}}", "probedAt": "{{when}}完成测试", "needsBraveKey": "网页搜索已启用，但尚未设置 Brave Search API 密钥；添加密钥前查询会一直失败。", "domainTray": "域名访问规则", "domainAllow": "允许列表", "domainBlock": "阻止列表", "domainHint": "每行输入一个域名。两份列表均为空时不限制域名；设置允许列表后，搜索结果仅保留列表内的域名。" },
      "dashboard": { "title": "仪表板 Provider 显示设置", "note": "已选择 {{count}} 家。只影响现状页显示，不会启用或停用任何 Provider。", "disabledVisible": "显示（已停用）" },
      "restore": { "title": "还原 Codex 原始设置", "description": "移除 Vellum 写入 Codex 的 Proxy 网址、boundary key 和模型菜单项目，并还原原本的默认 Provider。为了让你既有的 Vellum 对话仍能在原生 Codex 中打开，会保留一个不经过 Proxy、直接连接 OpenAI 的兼容 Provider。登录凭证、聊天记录、项目分组和工作区数据都保持不变。", "action": "还原 Codex 原始设置", "cleared": "已还原", "result": "结果", "nothingToClear": "Codex 中没有 Vellum 写入的设置，不需要还原", "preserved": "未修改：{{items}}。" },
      "enhancedRuntime": {"title": "Enhanced Codex Runtime", "description": "第三方 Provider 的新对话改由 Enhanced Codex 执行；OpenAI 官方模型保持原生 Codex Runtime。切换由哪一边执行时，必须新建对话。", "desiredState": "要求状态", "desiredEnabled": "要求启用", "desiredDisabled": "要求停用", "activation": {"disabled": "未启用", "artifactBlocked": "可执行文件验证失败", "awaitingDesktopRestart": "等待重新启动 Codex Desktop", "active": "已生效", "disablePendingRestart": "已要求停用，等待重新启动", "environmentDrift": "接管状态不一致", "failed": "Bridge 启动失败"}, "environment": {"leased": "Vellum 已安全接管", "released": "尚未接管", "orphanedBridge": "残留的旧版 Vellum bridge", "foreignValue": "由其他工具设置，Vellum 没有覆盖", "unreadable": "无法读取用户环境变量"}, "environmentValue": "CODEX_CLI_PATH 当前的值", "environmentUnset": "尚未设置", "staleBridge": "这条路径是旧版 Vellum 留下的 bridge，不是这个构建要用的那一个。按「停用并释放接管」清掉它，再重新启用。", "launchDetail": "{{launchId}} · {{state}} · bridge {{bridgePid}} · Official {{officialPid}} · Enhanced {{enhancedPid}}", "officialBinary": "官方 Codex core", "officialBinaryHint": "从已安装的 Codex Desktop 检测。必须与 Desktop 实际执行的那一个相同，因此不开放更改。", "enhancedBinary": "Enhanced Codex core", "bridgeBinary": "App Server bridge", "bridgeBinaryHint": "随这个 Vellum 构建一起提供，哈希写死在程序里，不能换成别的文件。", "protocol": {"label": "协议检查", "unavailable": "无法检查", "details": "差异与诊断（{{count}}）", "routed": "路由", "verdict": {"verified": "与锁定版本一致", "unverified": "可用，但不保证正确性", "incompatible": "不兼容，已回退"}, "mean": {"verified": "这正是这个 Vellum 版本锁定的组合，两边的协议逐字节相同。", "unverified": "这个 Codex Desktop 版本没有人验证过。bridge 分派要用到的方法都还在，所以路由没问题；下面列的差异是真实存在的，只是没被测过。", "incompatible": "Codex Desktop 改动了 bridge 分派时必须用到的方法，没有可以降级的跑法。"}, "delta": {"shapeIncompatible": "{{subject}} 的 wire 形状对方不接受：{{fields}}", "shapeUnverified": "{{subject}} 的 wire 形状无法判定：{{fields}}", "methodUnservable": "Desktop 可能调用 {{subject}}，Enhanced core 没有实现", "methodUnknownToDesktop": "Enhanced core 可能发出 {{subject}}，Desktop 已不再声明", "fieldNewlyRequired": "Desktop 现在要求 {{subject}} 的 {{fields}}，Enhanced core 不会发出", "fieldRemoved": "Desktop 不再发出 {{subject}} 的 {{fields}}，Enhanced core 会读它"}, "fallback": "已回到原生 Codex。Codex 本身照常运行，只是 Enhanced 的功能都不在。"}, "protocolHash": "App Server 协议", "observedBridge": "实际运行中的 bridge", "unverified": "尚未验证", "details": "技术细节与排查信息", "runInstalledGate": "重新执行安装版验证", "installedGateHint": "比重开一次更深入的验证：重新启动 Codex Desktop，运行完整的安装版检查，并留下一份报告。", "lastQualification": "上次验证", "qualificationPassed": "{{mode}} 通过，可以升级", "qualificationComponentOnly": "{{mode}} 组件测试通过，但尚未达到实机升级门槛", "qualificationFailed": "{{mode}} 未通过 —— {{detail}}", "qualificationReport": "报告", "planeTitle": "每条路由现在走哪一边", "contextNote": "压缩由执行对话的 runtime 自己负责：官方路径用 Official Codex 原生压缩，第三方路径用 Enhanced Codex 的本机压缩与上下文恢复。Vellum 不会再套用自己的一套压缩策略。", "planeOfficial": "Official Codex · 原生压缩", "planeEnhanced": "Enhanced Codex · 本机压缩与上下文恢复", "planeUnbound": "尚未绑定 Enhanced Codex", "activationLabel": "启用状态", "environmentLabel": "接管状态", "inject": {"on": "已注入", "off": "未注入", "working": "注入中", "switchLabel": "注入 Enhanced Codex Runtime", "mean": {"staleLaunch": "Codex Desktop 现在正走在 Enhanced 上，但用的是较早那次启动的配置。要让最新的配置生效，重新启动 Codex Desktop。", "on": "Codex Desktop 正跑在 Vellum 的 bridge 上。第三方 Provider 的新对话由 Enhanced Codex 执行，OpenAI 官方模型维持原生 Codex。", "off": "Codex Desktop 跑在原生 Codex 上。CODEX_CLI_PATH 没有被 Vellum 动过。", "wanted": "Vellum 这边设置好了，但 Codex Desktop 还没接上，所以现在跑的还是原生 Codex。", "working": "正在验证可执行文件、接管 CODEX_CLI_PATH，并重新启动 Codex Desktop。", "armed": "重新启动 Codex Desktop 后生效。"}, "blocked": {"proxyStopped": "Proxy 没有启动。Proxy 没在跑的时候 Vellum 不会接管启动路径 —— 不然 Codex Desktop 会启动到一个后面没有数据面的 bridge。先启动 Proxy。", "stuck": "要求过注入，但路径没有被接管。终端会显示卡在哪一步；再拨一次开关即可重试。", "noCore": "这个 Vellum 构建里没有 Enhanced Codex core。这不是你漏掉的设置 —— core 由构建自带，缺了代表安装不完整，请重新安装 Vellum。"}}, "term": {"idle": "还没跑过。拨动上面的开关后，每一步都会显示在这里。", "verify": "核对 Enhanced Codex 可执行文件", "verifyOk": "文件内容与这个版本预期的一致", "verifyFailed": "核对没过，什么都没有动", "restart": "接管 CODEX_CLI_PATH 并重新启动 Codex Desktop", "restartBack": "重新启动 Codex Desktop，回到原生 Runtime", "restartRefused": "没有重新启动", "adopt": "确认 Codex Desktop 是否已接上 bridge", "injected": "已注入 · {{profile}}", "notInjected": "Codex Desktop 没有接上 bridge", "release": "释放 CODEX_CLI_PATH", "releasedOk": "已释放，Codex Desktop 回到原生 Runtime", "envUnset": "（未设置）", "preflight": "检查前置条件", "proxyStopped": "Proxy 没有启动，接管会被释放掉。先启动 Proxy 再注入。", "armed": "启动路径已接管。下次启动 Codex Desktop 就会生效。", "alreadyInjected": "Codex Desktop 已经在这个 bridge 上运行了，不用重新启动 · {{profile}}", "missingHelper": "缺少辅助程序 {{name}}"}, "missingHelpers": "这支 Enhanced Codex 旁边少了官方安装有的辅助程序：{{names}}。Codex 在运行时才会到自己旁边去找，找不到就会变成一个 Windows「找不到文件」对话框。对话与路由不受影响；受影响的只有用到该辅助程序的功能 —— 沙箱 shell 命令需要 codex-windows-sandbox-setup.exe 和 codex-command-runner.exe；code mode 需要 codex-code-mode-host.exe，而 code mode 默认是关的，关着就没有影响。", "enhancedBinaryHint": "由这个 Vellum 构建自带，并用 enhanced-runtime.lock.json 里的哈希逐字节比对。换成别的只会验证失败，所以不开放选择。", "coreMissing": "这个构建中不包含"},
      "updates": { "title": "软件更新", "panelHint": "这是全局更新面板，不会切换当前页面。", "overall": "整体更新", "overallHint": "一次检查并安排 Vellum 本体、Remote 包和 Enhanced core。", "checkAll": "检查全部", "updateAll": "全部更新", "updatingAll": "正在更新全部组件…", "experimentalParts": "实验性：分项更新", "experimentalPartsHint": "仅在需要单独测试、取消或回滚某个组件时使用。", "description": "本体、Remote 包与 Enhanced core 可独立更新。已下载的更新会保持待应用状态，直到满足生效条件。", "liveDisabled": "在配置发布签名之前，自动更新保持停用。", "actionFailed": "更新操作失败：{{error}}", "autoCheck": "自动检查更新", "autoDownload": "自动下载更新", "channel": "通道", "stable": "稳定", "preview": "预览", "current": "当前版本", "available": "可用版本", "none": "无", "progress": "下载进度", "applyWhen": "生效条件", "notes": "发行说明", "failure": "失败原因", "check": "检查更新", "download": "下载", "apply": "应用", "cancel": "取消下载", "rollback": "回退", "hosts": "主机", "idleAuto": "空闲时自动更新", "idleHandoff": "预览：空闲时应用 Enhanced core（默认关闭）", "layerDisabled": "此层更新尚未在当前构建中启用。", "layer": { "desktop": "Vellum 本体", "remote": "Remote 包", "core": "Enhanced Codex core" }, "condition": { "restartVellum": "已就绪。重新启动 Vellum 后应用。", "hostIdle": "已下载。等待主机空闲。", "nextCoreStart": "已下载。下次核心启动时应用。", "download": "下载后暂存。", "failed": "见失败原因。", "idle": "已是最新。", "checking": "检查中…", "available": "有可用更新。", "downloading": "下载中…", "verifying": "验证中…", "staged": "已暂存。", "waitingForIdle": "等待空闲。", "waitingForRestart": "等待重新启动。", "applying": "正在应用…", "validating": "验证中…", "applied": "已应用。", "blocked": "已阻止。", "rolledBack": "已回退。" }, "phase": { "idle": "待命", "checking": "检查中", "available": "可用", "downloading": "下载中", "verifying": "验证中", "staged": "已暂存", "waitingForIdle": "等待空闲", "waitingForRestart": "下次启动时应用", "applying": "正在应用", "validating": "验证中", "applied": "已应用", "blocked": "已阻止", "failed": "失败", "rolledBack": "已回退" } },
      "advanced": { "title": "高级设置", "description": "提供请求接收控制、Codex 安全重启和模型菜单还原功能，仅供故障排除或维护使用。", "activeRequests": "进行中的请求", "drainTitle": "停止接收新请求", "draining": "已停止接收新请求；正在等待已有请求完成", "accepting": "目前正常接收请求", "drainHint": "不会中断进行中的请求；启用后，Vellum 会拒绝新请求，直到手动恢复。", "resume": "恢复接收请求", "stop": "停止接收新请求", "catalogVersion": "当前菜单版本", "notCreated": "尚未建立", "restartTitle": "重启 Codex", "restartHint": "安全结束已有请求后重启 Codex。", "restartAction": "重启 Codex", "restartAnyway": "仍要重启", "guideTitle": "初次设置向导", "guideHint": "重新查看初次设置或连接其他 Provider，不会重置现有设置。", "guideAction": "打开设置向导", "catalogHistory": "模型菜单版本记录", "rollback": "还原", "noVersions": "还没有可还原的版本。每次更新模型菜单都会自动保留一份。", "restartRequired": "重启后生效", "applied": "已生效" },
      "logs": { "title": "诊断日志", "description": "将 Vellum、Proxy 与 Enhanced Runtime 保留的文本日志导出为 ZIP。导出时会再次遮蔽密钥、Token 与用户路径；不包含凭证、设置、聊天记录或数据库。", "export": "导出全部日志 ZIP", "exporting": "正在导出…", "exported": "已导出至 {{path}}", "hint": "ZIP 会保存到下载文件夹，并附带文件清单与截断记录。" },
      "app": { "title": "应用程序控制", "exit": "退出 Vellum", "exitHint": "退出会停止 Proxy 并恢复 Codex 原始连接设置；聊天记录和项目不受影响。" },
      "errors": { "partialRefresh": "部分数据更新失败：{{detail}}", "reviewSave": "无法保存自动审查设置：{{detail}}", "reviewNoModelAvailable": "尚未配置任何已启用的审查模型，请先新增或启用一个 Provider 再切换为固定模型。", "reviewNoFallbackAvailable": "故障转移需要另一个不同 Provider 上已启用的审查模型，请先新增或启用一个。", "restore": "还原失败：{{detail}}", "drain": "无法更新是否接收新请求：{{detail}}", "restart": "无法重启 Codex：{{detail}}", "rollback": "无法还原模型菜单：{{detail}}", "webSearchSave": "无法保存网页搜索设置：{{detail}}", "subagentSave": "无法保存子代理设置：{{detail}}", "logExport": "无法导出诊断日志：{{detail}}" }
    }
  },
  "today": {
    "title": "现状",
    "loading": "正在读取状态…",
    "noProvider": "还没有 Provider",
    "noSuccessfulRequest": "尚无成功请求",
    "addProvider": "加入 Provider",
    "providerTokens": "当前 Provider 累计 Token",
    "requests_one": "{{count}} 次请求",
    "requests_other": "{{count}} 次请求",
    "lastActivity": "最后活动",
    "proxy": { "start": "启动 Proxy", "stopRestore": "停止 Proxy 并还原 Codex" },
    "quota": {
      "tightest": "周额度剩最少的 Provider",
      "remaining": "额度剩余",
      "noData": "目前没有 Provider 回报额度"
    },
    "context": {
      "nearestThreshold": "最接近压缩门槛的会话",
      "usage": "上下文用量",
      "threshold": "压缩门槛 {{percent}}%",
      "compactThreshold": "压缩门槛",
      "averageTurn": "平均每轮",
      "atRate": "按此速度",
      "trend": "上下文用量趋势"
    },
    "findings": {
      "title": "待处理项目",
      "count_one": "{{count}} 项",
      "count_other": "{{count}} 项",
      "adjustContext": "调整上下文窗口"
    },
    "sessions": { "title": "会话", "count": "{{live}} 个活动中 / 共 {{total}} 个", "rest": "其他会话" },
    "providers": {
      "title": "Provider 状态",
      "noModels": "没有可用模型",
      "quotaFailed": "额度查询失败",
      "notQueried": "还没查过",
      "lastChecked": "{{since}}查询",
      "refreshTitle": "重新查询 {{provider}} 的额度",
      "querying": "查询中…",
      "refresh": "重新查询",
      "empty": "没有要显示的 Provider。到「设置」勾选，或到「模型」新增一家。"
    },
    "health": {
      "title": "连接质量",
      "connectionReuse": "连接复用",
      "reusing_one": "复用中 · {{count}} 条连接",
      "reusing_other": "复用中 · {{count}} 条连接",
      "notReusing": "未复用",
      "firstByte": "首字延迟",
      "reasoning": "推理内容",
      "history": "历史保存",
      "historyValue_one": "{{days}} 天 · 已加密",
      "historyValue_other": "{{days}} 天 · 已加密"
    },
    "headroom": {
      "exceeded": "已超过门槛，下个对话回合将触发压缩",
      "noEstimate": "尚无对话回合可供估算",
      "estimate_one": "预计还可进行 {{count}} 个对话回合",
      "estimate_other": "预计还可进行 {{count}} 个对话回合"
    },
    "errors": {
      "partialRefresh": "部分数据更新失败：{{detail}}",
      "quotaRefresh": "额度查询失败：{{detail}}",
      "proxyOperation": "Proxy 操作失败：{{detail}}"
    }
  },
  "models": {
    "title": "模型",
    "ui": {
      "catalogTitle": "设置 Codex 模型菜单", "catalogHint": "先连接账号或 Provider，再挑出要送进 Codex 模型菜单的模型。", "wireUnknown": "无法自动判断，请选择", "customProvider": "自定义 Provider", "errors": { "partialRefresh": "部分数据更新失败：{{detail}}", "noReset": "目前没有可用的 Reset。", "confirmReset": "确定要为 {{account}} 使用一个 Reset？这会消耗一笔 Reset 额度，且无法还原。", "resetSuccess": "OpenAI 额度已重置。", "resetCompleted": "Reset 已完成（{{code}}）。", "resetFailed": "Reset 失败：{{detail}}", "resetLookupFailed": "Reset 查询失败：{{detail}}", "oauthExpired": "授权码已过期，请重新登录。", "oauthLoginFailed": "ChatGPT 登录失败：{{detail}}", "oauthSwitchFailed": "无法切换 ChatGPT 账号：{{detail}}", "oauthRemoveFailed": "无法移除 ChatGPT 账号：{{detail}}", "oauthRefreshFailed": "无法更新 ChatGPT 登录：{{detail}}", "oauthLogoutFailed": "无法登出 ChatGPT：{{detail}}", "probeFailed": "无法探测此端点：{{detail}}", "opencodeApiKeyRequired": "请输入 OpenCode Zen API key。", "opencodeConnectFailed": "无法连接 OpenCode Zen：{{detail}}", "opencodeFreeAttachFailed": "OpenCode Go 已连接，但接上它的免费模型失败：{{detail}}", "modelProbeFailed": "无法验证 {{model}}：{{detail}}", "modelToolProbeFailed": "{{model}} 未返回 Codex 可用的结构化工具调用。", "routeRefreshFailed": "无法更新 Provider 状态：{{detail}}", "reprobeFailed": "重新探测能力失败：{{detail}}", "routeRemoveFailed": "无法移除 Provider：{{detail}}", "providerModelRequired": "每个 Provider 至少要保留一个模型。", "catalogRefreshFailed": "无法更新 Codex 模型菜单：{{detail}}", "wireRequired": "无法自动判断 API 协议，请选择 Responses API 或 Chat Completions。", "modelRequired": "未探测到模型名称，请手动填写。", "routeAddFailed": "无法加入 Provider：{{detail}}", "grokLoginFailed": "Grok 登录失败：{{detail}}", "grokStartFailed": "无法启动 Grok 登录：{{detail}}", "grokCancelFailed": "无法取消 Grok 登录：{{detail}}", "grokSwitchFailed": "无法切换 Grok 账号：{{detail}}", "grokRefreshFailed": "无法更新 Grok 账号：{{detail}}", "grokRemoveFailed": "无法移除 Grok 账号：{{detail}}", "detectAttention": "需补充", "detectFact": "已探测" },
      "confirm": {
        "removeAccount": { "title": "移除 ChatGPT 账号", "confirmLabel": "移除账号", "factAccount": "账号", "factEffect": "影响", "effectValue": "Vellum 会忘记这个账号的登录凭证；要再用它需要重新登录。" },
        "logoutAll": { "title": "登出所有 ChatGPT 账号", "confirmLabel": "全部登出", "factAccount": "账号", "factEffect": "影响", "accountsValue": "所有已连接的 ChatGPT 账号", "effectValue": "每个账号都会从 Vellum 移除，之后都需要重新登录。" },
        "removeGrokAccount": { "title": "移除 Grok 账号", "confirmLabel": "移除账号", "factAccount": "账号", "factEffect": "影响", "effectValue": "Vellum 会忘记这个账号的登录凭证；要再用它需要重新登录。" },
        "removeRoute": { "title": "移除 Provider", "confirmLabel": "移除 Provider", "factProvider": "Provider", "factEffect": "影响", "effectValue": "这个 Provider 与它整份模型菜单都会被移除；要还原就得重新添加一次。" },
        "consumeReset": { "title": "使用一个 Reset", "confirmLabel": "使用 Reset", "factAccount": "账号", "factEffect": "影响", "effectValue": "这会消耗一笔 Reset 额度，且无法恢复。" }
      },
      "chatgpt": { "title": "ChatGPT 账号", "description": "选择官方模型使用的账号。已发送的请求继续使用原账号；切换成功后创建的新请求使用新账号。Token 通常会自动更新，不需要重新登录。" },
      "pool": { "title": "额度池", "onHint": "池里的账号会自动接手；每个账号可以保留一段每周额度。", "offHint": "关闭时维持手动切换，新请求使用你选定的账号。", "rules": "判定规则", "rulesTitle": "额度池判定规则", "rankHint": "左边的编号就是顺序，用 ▲▼ 调整。", "availableThisWeek": "这周还可以自动取用", "availablePercent": "池内这周还可以自动取用 {{value}}%", "currentAndNext": "现在由 {{current}} 取用，接着是 {{next}}。", "stalled": "池内没有可用的账号；不会越过门槛，也不使用池外账号。", "empty": "池是空的。从下面的账号点「加入池」才会开始自动轮替。", "using": "取用中", "next": "下一个", "standby": "待命", "outside": "未加入池", "pause": "暂停", "resume": "恢复", "add": "加入池", "remove": "移出池", "moveUp": "往前一个顺位", "moveDown": "往后一个顺位", "burnable": "可用 {{value}}%", "atGate": "已到门槛", "gateRead": "门槛 {{floor}}%", "gateLabel": "{{account}} 的每周门槛", "gateValue": "门槛 {{floor}}%，这周剩 {{left}}%", "saveFailed": "无法保存额度池：{{detail}}", "reason": { "paused": "已暂停", "weeklyGate": "每周额度到门槛", "fiveHour": "5 小时窗口用完", "missingQuota": "额度数据不完整" }, "rule": { "gate": { "title": "门槛是保留下限", "body": "每周用量到门槛时就停手，门槛以下的额度留给你自己；设成 100% 就完全不交给自动轮替。" }, "fiveHour": { "title": "5 小时窗口只读", "body": "这是上游限流；归零时暂时轮不到，重置后会自动回来。" }, "order": { "title": "顺序由你决定", "body": "编号就是轮替顺序，用 ▲▼ 调整；暂时用不了的账号会被跳过，其余账号的先后不变。" }, "pause": { "title": "暂停与移出", "body": "暂停会保留设置但暂时不用；移出后自动轮替不会再碰它。" }, "reset": { "title": "不自动花掉 Reset", "body": "额度池不会自动消耗 Reset；手动重置后门槛维持原值。" }, "empty": { "title": "没有账号可用", "body": "池内所有账号都受限时会明确停止，不会越过门槛，也不使用池外账号。" } } },
      "oauth": { "code": "授权码", "loginPage": "登录页面", "browserHint": "浏览器已打开，完成授权后这里会自动更新。", "copied": "授权码已复制", "copyFailed": "复制失败，请手动选择授权码", "copy": "复制授权码", "waiting": "等待授权…", "waitingBrowser": "等待浏览器授权", "login": "登录 ChatGPT", "refreshToken": "更新 Token", "logoutAll": "全部登出" },
      "account": { "active": "使用中", "authenticated": "已登录", "current": "当前账号", "useThis": "改用这个" }, "quota": { "failed": "额度查询失败", "loading": "额度查询中", "retry": "重新查询" }, "reset": { "show": "展开每张 Reset 的到期时间", "hide": "收起每张 Reset 的到期时间", "ledgerTitle": "用量上限重设", "noneUsable": "这个账号没有可用的 Reset。", "untitled": "Reset 额度", "spent": "已用掉", "lapsed": "已过期", "noExpiry": "上游没有给到期时间", "failed": "Reset 查询失败", "loading": "Reset 查询中", "use": "使用重置" },
      "fiveHour": { "start": "启动 5h", "starting": "启动中…", "hint": "使用这个账号发送一笔极小的 Luna 请求，以启动 5 小时用量窗口。会消耗少量额度，但不会使用 Reset。", "success": "已触发 5 小时窗口并刷新用量。", "failed": "无法触发 5 小时窗口：{{detail}}", "refreshFailed": "触发请求已完成，但无法刷新用量：{{detail}}", "autoOn": "5h 自动：开", "autoOff": "5h 自动：关", "autoHint": "根据当前额度和 resetAt 自动续接 5 小时窗口。仅在旧窗口已到期且每周额度仍高于门槛时，使用 Luna、最低 effort 发送一笔极小请求；判断会写入可导出的 Vellum 日志。" },
      "grok": { "title": "Grok 账号", "description": "Grok Build 的新请求会使用当前账号。切换立即生效，进行中的请求不会中途更换账号。", "loginStatus": "登录状态", "browserHint": "官方 Grok CLI 已启动登录流程；完成授权后会自动加入账号。", "loginIncomplete": "官方登录流程尚未完成。", "defaultAccount": "Grok CLI 默认账号", "externalCli": "外部 CLI", "managed": "Vellum 管理", "reauthenticate": "重新验证", "refreshModels": "重新探测 Grok 模型", "unlink": "解除关联", "empty": "目前没有可用的 Grok 账号。", "add": "添加 Grok 账号" },
      "opencode": { "title": "OpenCode Zen", "description": "只要填入 API key 就能连接。这里默认与 OpenCode 官方 App 一致，只显示免费模型。付费 Zen 模型需要购买额度，而这条连接无法查证，如果你确实有购买，请改用下方手动加入 Provider 的流程。", "descriptionGo": "只要填入 API key 就能连接。OpenCode Go 是另一个端点上独立的小型模型库，不包含 OpenCode Zen 的 GPT／Claude／Gemini 等高阶模型；如果你的 key 属于 Go 订阅，就选这个。Go 的端点不接受免费模型，所以 Vellum 会另外把 OpenCode Zen 的免费模型接成第二个 Provider。", "freeRouteName": "{{name}}（免费模型）", "catalog": "模型库", "catalogZen": "OpenCode Zen（免费模型）", "catalogGo": "OpenCode Go", "catalogHint": "请选择你实际拥有的方案。Zen 只连接免费模型；Go 连接 Go 方案的模型库，并自动附带同样的免费模型。", "apiKey": "OpenCode Zen API key", "apiKeyPlaceholder": "粘贴 OpenCode Zen API key", "connect": "连接", "connecting": "连接中…", "connected": "已连接" },
      "addProvider": { "title": "加入 Provider", "steps": { "endpoint": "连接端点", "endpointHint": "获取 Provider 提供的模型列表。", "probe": "选择并验证", "probeHint": "先选择要导入的模型，再只针对该模型验证 Codex 协议能力。", "add": "加入", "addHint": "确认结果并加入 Codex 模型菜单。" }, "endpoint": "端点网址", "apiKey": "API key（选填，加密存放）", "apiKeyPlaceholder": "本机或使用账号登录的端点可留空", "startProbe": "获取模型列表", "probing": "正在获取模型列表…", "add": "加入 Provider", "reprobe": "重新开始" },
      "probe": { "reachable": "可以连接", "wire": "API 协议", "modelCount": "探测到 {{count}} 个模型", "modelsMissing": "未探测到，请手动填写", "context": "上下文窗口", "required": "需要填写", "streaming": "流式", "supported": "支持", "unsupported": "不支持", "toolCalling": "Codex 工具协议", "typedToolCalls": "已验证结构化工具调用", "toolCallingUnavailable": "未验证结构化工具调用", "chatOnly": "仅支持对话，无结构化工具调用", "reasoning": "推理内容", "detected": "已探测", "notDetected": "未探测到", "serverResume": "上游记住对话", "remembers": "会记住", "localHistory": "不记住，由 Vellum 补历史", "detectedModels": "探测到的模型", "selectionHint": "只有已勾选并通过验证的模型会加入 Codex 模型菜单。", "verifyToImport": "勾选以验证工具调用", "verificationRequired": "需要进行能力验证", "timeout": "探测超时", "unknown": "未知", "verifyModel": "验证", "verifyingModel": "验证中…",  "quotaWithRetry": "配额限制（HTTP 429 · 请于 {{seconds}} 秒后重试）", "unauthorized": "认证失败（HTTP 401/403）", "protocolError": "协议格式错误", "toolCallMissing": "未生成结构化工具调用", "unsupportedOpenCodeProtocol": "暂不支持 Anthropic／Google 原生协议", "defaultModel": "默认模型", "defaultModelHint": "所有模型都会加入 Codex 菜单；这里只决定 Provider 建立后预先使用哪一个。", "select": "请选择", "wireHint": "这是 Provider 接收请求的 API 协议，不是响应的 JSON 格式。", "manualPlaceholder": "未探测到，请手动填写", "settingsTitle": "端点探测设置", "requestSize": "请求大小", "preferredWire": "优先 API 格式", "preferredWireValue": "先试 Responses，再退回 Chat Completions", "contextSource": "上下文长度来源", "contextSourceValue": "依次使用手动设置、端点回报、模型缓存和 Codex 模型菜单。", "history": "对话历史保存", "historyValue": "上游无法续接会话时，由本机加密保存历史。" },
      "modelCatalog": { "rename": "改名", "renameProvider": "Provider 显示名称", "renameModel": "模型显示名称", "renameHint": "只改显示名称。送给 Provider 的仍是上游 id，Codex 既有的路由也不受影响。", "allowPrivateNetworkHttp": "允许明文 HTTP 连到私有网络", "allowPrivateNetworkHttpHint": "给局域网或 Tailscale 之类的地址用的：默认拒绝明文 HTTP 连到非本机端点，开启后才能连到私有网络（LAN、CGNAT、Tailscale）地址；仍然拒绝连到公网。", "renameModelPlaceholder": "留空就显示上游 id", "renameSave": "保存", "renameCancel": "取消", "free": "免费", "deprecated": "已下架", "vision": "图片", "visionOn": "这个模型可接受图片输入", "visionOff": "这个模型只支持文字；勾选后 Codex 才会允许附图", "visionHint": "图片支持无法可靠探测——纯文字的端点收到图片也会返回 200，然后让模型自己编答案。请按模型实际支持的能力勾选。", "tokenUnit": "token", "effort": "推理强度", "responses": "Responses", "chat": "Chat Completions", "title": "模型菜单", "hint": "已勾选的模型会加入 Codex 模型菜单。", "empty": "还没有 Provider。用上面的流程加入第一家。", "countSelected": "{{selected}} / {{total}} 个", "count": "{{count}} 个", "unknownWindow": "窗口未知", "reasoning": "推理", "noReasoning": "无推理", "auto": "自动", "capabilityMissing": "尚未获取该 Provider 的模型能力数据。", "recent": "最近使用", "reprobe": "重新探测能力", "reprobeInProgress": "正在验证 {{count}} 个已选模型…", "reprobeSummary": "已验证 {{succeeded}}/{{targeted}} 个已选模型", "reprobeSummaryWithFailures": "已验证 {{succeeded}}/{{targeted}} 个已选模型，{{failed}} 个失败", "effortNotProbed": "尚未探测", "effortUnverified": "无法验证", "effortReasonIgnored": "Provider 对刻意发送的无效推理强度也返回成功，因此无法证明 low／medium／high 等层级确实生效。重复探测通常不会改变结果。", "effortReasonQuota": "推理强度探测受到 Provider 配额或账号权限限制{{detail}}", "effortReasonProvider": "推理强度探测期间发生 Provider 错误、超时或响应不完整{{detail}}", "effortReasonUnknown": "这次推理强度探测没有取得足以判断支持层级的证据{{detail}}", "retryEffortProbe": "重试推理强度探测", "footerNote": "启用和停用会立即保存，但需要重启 Proxy 才会生效；模型菜单要等重启 Codex 后才会被重新读取。在两者完成前，状态会显示为“待生效”。", "windowEdit": "设置最大上下文长度", "windowHint": "设置这个模型的最大上下文长度", "windowAuto": "自动", "windowManual": "这是你手动设置的值；清空后会改回探测到的值。", "windowUnavailable": "这个模型还没进 Codex 菜单，没有可设置的地方。", "windowInvalid": "最大上下文长度要填大于 0 的数字，留空则改回自动。", "windowSaveFailed": "无法保存最大上下文长度：{{detail}}" }
    }
  },
  "context": {
    "awaitingCompaction": "这个对话还没有发生过压缩。",
    "transcript": {
      "eyebrow": "压缩记录",
      "title": "压缩指令与替换内容",
      "hint": "显示发送给模型的压缩指令，以及模型返回的替换内容。后续回合将使用该替换内容。",
      "chars": "{{value}} 字",
      "prompt": "送出的指示",
      "promptNote": "压缩提示词，与模型实际收到的内容一致。",
      "result": "写回来的内容",
      "resultNote": "后续对话会一直带着的内容。",
      "noPrompt": "这次压缩的指示没有经过 Vellum。",
      "noResult": "这次压缩的结果在这里读不到。",
      "empty": "还没有东西可以看 —— 要么这个对话还没被压缩过，要么压缩发生在 Vellum 读不到的地方。",
      "unavailable": {
        "codexDesktopOpaque": "Codex Desktop 将这次压缩保存为不透明状态，因此 Vellum 无法读取压缩指令与替换内容。",
        "notCompacted": "这个对话还没有发生过压缩。",
        "officialOpaque": "这次压缩由 OpenAI 官方以不透明状态管理，因此 Vellum 无法读取压缩指令与替换内容。",
        "legacyUnreadable": "这条旧版压缩记录没有可读取的压缩指令与替换摘要。"
      }
    },
    "sessions": {
      "eyebrow": "Enhanced Codex core",
      "title": "Enhanced core 上的会话",
      "hint": "显示在 Enhanced Codex core 上运行的对话。使用 OpenAI 原生核心的对话由 Codex 内部压缩，不会显示在这里。",
      "listTitle": "会话",
      "loading": "正在读取会话…",
      "empty": "现在没有对话跑在 Enhanced Codex core 上。",
      "untitled": "未命名会话",
      "count_one": "{{count}} 个会话",
      "count_other": "{{count}} 个会话",
      "pick": "显示 {{label}} 的压缩情况",
      "headroom": "距离压缩还有 {{percent}}%",
      "imminent": "已经到门槛",
      "search": "按标题搜索",
      "searchClear": "清除筛选",
      "countFiltered": "{{total}} 个中的 {{shown}} 个",
      "noMatch": "没有符合「{{query}}」的会话。"
    },
    "title": "上下文", "compactionEyebrow": "上下文压缩", "compactionTitle": "上下文压缩状态与预览", "compactionHint": "请在 Codex App 输入 /compact 执行压缩；Vellum 显示压缩状态和交接记录。", "before": "压缩前", "after": "压缩后", "opaque": "Opaque", "legendAria": "上下文组成", "previewLoading": "正在计算压缩预览…", "footerNote": "压缩由执行该对话的 Codex runtime 负责；Vellum 只显示观察到的事件，旧版 journal 仅供只读查看。",
 "segmentAria": { "before": "{{label}}，压缩前 {{tokens}} token", "after": "{{label}}，压缩后 {{tokens}} token" },
    "segmentLabel": {
      "canonical": "Canonical 检查点",
      "reasoning": "Readable Reasoning Replay",
      "tool": "工具延续状态",
      "retained": "保留的对话脉络",
      "observedTotal": "Codex 报告的总量"
    },
    "segmentNote": { "canonical": "保存目标、限制、决策、进度、文件和下一步。", "reasoning": "以可读摘要回放必要推理，不发送第三方私有 ciphertext。", "tool": "保留可安全续用的工具结果和调用配对状态。", "retained": "保留近期消息和有效执行上下文；最多优先保留 {{turns}} 轮。", "observedTotal": "这是 Codex 客户端报告的总 token 变化；它没有提供各类内容的拆分。", "default": "按 Canonical 压缩策略处理。" },
    "readout": { "tokenTransition": "{{before}} → {{after}} token", "savedDelta": "省下 {{tokens}} token", "tokenUnit": "token", "total": "总计", "officialStat": "{{before}} → OpenAI opaque canonical", "officialNote": "官方压缩状态为加密数据；Vellum 不根据 ciphertext 大小推算 token。", "saved": "节省 {{saved}} token（{{percent}}%）", "hover": "悬停查看该段明细", "share": "占压缩前的 {{percent}}%" },
    "summary": { "goal": "目标", "acceptanceCriteria": "验收标准", "constraints": "限制", "userPreferences": "用户偏好", "done": "已完成", "inProgress": "进行中", "blocked": "受阻", "decisions": "决策", "changedFiles": "修改的文件", "relevantFiles": "相关文件", "commands": "命令", "tests": "测试", "unresolved": "未解决", "errors": "错误", "criticalContext": "关键背景", "references": "参考", "nextSteps": "下一步" },
    "errors": { "partialRefresh": "部分数据更新失败：{{detail}}" }
  },
  "log": {
    "title": "记录",
    "activityTitle": "Token 活动",
    "loading": "正在读取记录…",
    "heading": "Token 用量与请求记录",
    "days_one": "{{count}} 天",
    "days_other": "{{count}} 天",
    "boot_one": "已启动 {{count}} 次 · PID {{pid}} · 上次启动 {{time}}",
    "boot_other": "已启动 {{count}} 次 · PID {{pid}} · 上次启动 {{time}}",
    "bootFirst": "首次启动 · PID {{pid}}",
    "stats": {
      "total": "累计 Token 数",
      "peak": "Token 峰值",
      "longestRequest": "最长请求时间",
      "currentStreak": "当前连续记录",
      "longestStreak": "最长连续记录"
    },
    "providers": {
      "title": "各 Provider 累计 Token 数",
      "note": "OpenAI 对齐 Codex 个人档案；第三方按上游返回的 input + output 计算",
      "others": "其他 Provider", "fromProfile": "取自 Codex 个人档案", "segmentAria": "{{provider}}：占累计 Token 的 {{percent}}，{{value}}", "accountCount_one": "（{{count}} 个账号）",
      "accountCount_other": "（{{count}} 个账号）"
    },
    "tokens": "{{value}} Token",
    "requests": {
      "title": "请求明细",
      "note": "只显示最近 {{count}} 条",
      "empty": "还没有请求记录。Codex 发送第一个请求后会显示在这里。",
      "filterEmpty": "目前的筛选条件没有符合的请求。",
      "filter": { "statusAll": "全部", "statusFailed": "只看失败", "providerAll": "全部 Provider" },
      "cached": "缓存 {{percent}}%",
      "connection": "conn {{id}}",
      "streamQualityTitle": "流式品质：{{quality}}",
      "accountIdentityTitle": "已哈希的控制账号 A 与执行账号 B",
      "streamQuality": {
        "incremental": "增量",
        "end_flush": "结束时一次性输出",
        "buffered": "缓冲",
        "no_delta": "无增量"
      },
      "subagentChildren_one": "{{count}} 个子代理",
      "subagentChildren_other": "{{count}} 个子代理",
      "subagentChildrenTitle_one": "定位 {{count}} 个已派生的子代理运行",
      "subagentChildrenTitle_other": "定位 {{count}} 个已派生的子代理运行"
    },
    "systemEvents": { "title": "系统事件" },
    "compaction": {
      "title": "压缩",
      "note": "自动压缩判定，与请求行分开显示",
      "empty": "还没有压缩事件。",
      "tokens": "{{before}} → {{after}}",
      "items": "条目 {{before}} → {{after}}",
      "threshold": "阈值 {{percent}}%",
      "window": "{{active}} / {{window}} 上下文",
      "checkpoint": "checkpoint {{id}}（第 {{generation}} 代）",
      "noCheckpoint": "无持久化 checkpoint（stateless）"
    },
    "subagent": {
      "title": "子代理",
      "note": "每行一个子代理运行 —— 展开查看 requested → child → completed 的时间线",
      "empty": "还没有子代理运行记录。",
      "summary": { "label": "子代理运行摘要", "total": "共 {{count}} 个", "completed": "完成 {{count}}", "active": "运行中 {{count}}", "attention": "需注意 {{count}}" },
      "call": "call {{id}}",
      "child": "child {{id}}",
      "parent": "父级 {{id}}",
      "locateParent": "定位父级请求（{{id}}）",
      "locateChild": "定位子级请求（{{id}}）",
      "requestedAt": "请求 {{time}}",
      "completedAt": "结束 {{time}}",
      "linkLabel": "关联：{{value}}",
      "outcome": "结果：{{value}}",
      "error": "错误：{{value}}",
      "unknownModel": "未知模型",
      "state": {
        "requested": "已请求",
        "running": "执行中",
        "completed": "已完成",
        "failed": "失败",
        "cancelled": "已取消",
        "ambiguous": "无法确认",
        "unlinked": "未关联"
      },
      "link": {
        "exact": "精确关联",
        "heuristic": "推测关联",
        "unlinked": "未关联"
      },
      "timeline": {
        "requested": "已请求",
        "child": "子请求",
        "completed": "已完成",
        "pending": "等待中",
        "noChildLink": "找不到对应子请求"
      }
    },
    "invokes": {
      "title": "桌面调用",
      "note": "最近 {{count}} 次 Tauri invoke —— 成功与失败",
      "empty": "此会话尚未记录桌面调用。",
      "ok": "ok",
      "error": "error"
    },
    "errors": {
      "partialRefresh": "部分数据更新失败：{{detail}}"
    }
  },
  "remote": {
    "title": "远程主机",
    "blurb": "设置与管理远程 SSH 主机，使其可直接供 Codex App 使用。聊天、thread 与 session 仍由主机上的 Codex native daemon 负责。",
    "rescan": "重新扫描",
    "rescanning": "扫描中…",
    "discovering": "正在读取 Codex/OpenSSH 连接配置。界面可继续操作，SSH 状态将在后台加载。",
    "featureDisabled": "Native Codex 远程总管在此版本中尚未启用。",
    "legacyBrokerUnsupported": "发现不再支持的旧 Broker 配对配置，已跳过。请改用 SSH 重新发现此主机。Vellum 不会迁移旧的 Broker 配对。",
    "noHosts": "Codex App 与 OpenSSH 配置中均未找到可用的连接。",
    "hostsAria": "远程主机",
    "probing": "探测中…",
    "probeFailed": "探测失败",
    "notProbed": "尚未探测",
    "updating": "更新中…",
    "lastUpdated": "{{when}} 更新",
    "sshInvalid": "SSH 配置有误",
    "otherHostBusy": "{{host}} 上仍有操作正在进行。",
    "goToHost": "前往该主机",
    "blockerUnknown": "遇到此版本无法识别的情况。",
    "releaseBlocked": "内置的部署包尚未通过签名验证，因此安装 Codex 与更新 Agent 已停用。已部署的主机仍可规划与同步。",
    "proxyImageUpdate": "此版本内含较新的 Proxy image，请点击“重新同步”应用到这台主机。",
    "state": {
      "unreachable": "无法连接",
      "unmanaged": "未部署",
      "readyToPlan": "部署未完成",
      "drifted": "配置有偏差",
      "nativeActive": "使用中",
      "detachedReady": "可脱离续跑"
    },
    "verdict": {
      "unreachable": "无法连接此主机上的 Vellum Agent。可能尚未安装，或 SSH 连接中断。",
      "unmanaged": "此主机尚未由 Vellum 管理。部署后 Codex App 即可直接使用。",
      "readyToPlan": "Proxy 已启动，但尚未接管 Codex 的模型菜单。",
      "drifted": "主机上的配置与 Vellum 的记录不一致，需要重新同步。",
      "nativeActive": "主机已就绪。Codex App 目前使用此主机的 native daemon。",
      "detachedReady": "主机已就绪，且关闭 Vellum 后 turn 仍会继续执行。"
    },
    "summary": {
      "threads_one": "{{count}} 个 thread 进行中",
      "threads_other": "{{count}} 个 thread 进行中"
    },
    "act": {
      "bootstrap": "部署此主机",
      "reconverge": "重新同步",
      "plan": "变更模型…",
      "replan": "更新预览",
      "apply": "确认并同步",
      "restartNative": "重启远程 Codex daemon",
      "takeoverNative": "接管远程 Codex daemon",
      "installCodex": "安装 pinned Codex CLI",
      "updateAgent": "更新远程 Agent",
      "syncDesktopCodex": "同步 Desktop Codex runtime",
      "repair": "修复 codex launcher",
      "bundle": "导出诊断包",
      "restore": "解除 Vellum 管理…",
      "retry": "重试",
      "stopAppOwned": "安全停止并重试",
      "grokLogin": "登录 Grok 账号",
      "grokCancel": "取消 Grok 登录",
      "grokRefresh": "更新 Grok 凭证",
      "chatgptPair": "配对桌面端选定的 ChatGPT 账号",
      "chatgptActivate": "启用桌面端选定的 ChatGPT 账号",
      "chatgptPairRow": "配对", "chatgptActivateRow": "启用", "chatgptPairAll": "配对桌面端全部 ChatGPT 账号",
      "chatgptPairSkip": "跳过这个账号",
      "chatgptAuthorize": "授权到远程",
      "chatgptReauthenticate": "重新登录",
      "executionLogin": "添加 Official 执行账号",
      "executionFollowControl": "模型请求跟随 Remote Control 账号",
      "executionSelect": "用于模型请求",
      "executionRemove": "移除执行账号",
      "devicePair": "配对这台手机"
    },
    "trust": {
      "title": "确认此主机的 SSH 密钥",
      "explain": "这是 trust-on-first-use：Vellum 还没见过这台主机的 SSH 密钥。确认前请透过另一个渠道核对下面的指纹。",
      "factHost": "SSH 主机",
      "factFingerprint": "指纹",
      "factCrossCheck": "核对渠道",
      "crossCheckHint": "一个你已经信任的渠道——主机自己的控制台、Tailscale 或 VPN 管理页等。",
      "confirm": "信任并继续",
      "checking": "正在确认此主机是否已被信任…",
      "fetchFailed": "无法读取此主机的 SSH 密钥。",
      "untrustedError": "此主机的 SSH 密钥尚未确认。"
    },
    "chore": {
      "restartNative": "应用新的配置与模型菜单。Proxy 不会停止，但进行中的 turn 可能被中断。",
      "takeoverNative": "Codex App 目前直接占用此主机的 daemon。接管后将改用 Vellum 管理的 native daemon，进行中的工作可能被中断。",
      "installCodex": "在主机上安装内置的 pinned Codex CLI。验证 manifest 与 digest 后才会进行原子替换。",
      "updateAgent": "只更新主机上的 vellum-remote-agent。Broker、Proxy image 与 Codex CLI 不会在这一步更新。",
      "syncDesktopCodex": "安装与此 Desktop core 完全一致的官方 Linux Codex build，完成协议验证后重启远程 daemon。Desktop {{desktop}}；远程 {{remote}}。",
      "repair": "重建 codex 命令的 launcher，使 Codex App 能找到受管理的 CLI。",
      "bundle": "导出 Agent、Docker、Proxy、daemon 与操作日志，敏感值会脱敏。反馈问题时请一并提供。"
    },
    "fact": {
      "changes": "会改",
      "untouched": "不动",
      "turns": "进行中的 turn"
    },
    "confirm": {
      "gate": "请输入主机名称以继续：{{host}}",
      "bootstrap": {
        "changes": "Codex 配置文件、模型菜单、Proxy 与所需凭证。第一次执行会应用所有已验证的模型，之后沿用已保存的选择。",
        "untouched": "主机上的项目、普通 Codex thread，以及用户自行安装的软件。",
        "turns": "会被拒绝，不会强制中断。"
      },
      "restore": {
        "changes": "还原 Codex 原本的配置文件与模型菜单、交还账号 lease、停止 Vellum 管理的 Proxy 与 runtime。",
        "untouched": "你的项目、普通 Codex thread、主机上的 Codex 与 Agent。",
        "turns": "会被拒绝，不会强制中断。"
      },
      "restartNative": {
        "changes": "重新启动主机上的 Codex native daemon，以应用新的配置与模型菜单。",
        "untouched": "Proxy 保持运行，配置文件与模型菜单都不会被改写。",
        "turns": "进行中的 turn 会被中断。"
      },
      "takeoverNative": {
        "changes": "将 Codex App 直接占用的 app-server 更换为 Vellum 管理的 native daemon。",
        "untouched": "配置文件、模型菜单与已登录的账号。",
        "turns": "Codex App 端进行中的工作可能被中断。"
      },
      "installCodex": {
        "changes": "在主机上安装内置的 pinned Codex CLI，验证 manifest 与 digest 后进行原子替换。",
        "untouched": "Codex 的配置文件、模型菜单与已有的 thread。",
        "turns": "不会停止进行中的 turn；新版本会在 daemon 重启后生效。"
      },
      "updateAgent": {
        "changes": "把 vellum-remote-agent 更新到当前 Vellum 内置的版本，并验证 digest。",
        "untouched": "Broker、Proxy image 与 Codex CLI 都不在这一步更新。",
        "turns": "Agent 会自行重启，远程操作会短暂中断。"
      },
      "syncDesktopCodex": {
        "changes": "下载与 Desktop core 完全一致的 OpenAI 官方 Linux artifact、验证发布者 digest、探测 app-server 协议、原子安装并重启远程 daemon。",
        "untouched": "Vellum Proxy image、模型菜单、账号、项目与现有的 task。",
        "turns": "远程 daemon 重启时，进行中的 turn 可能中断。"
      },
      "stopAppOwned": {
        "changes": "安全停止 Codex App 直接占用的 app-server，然后重新执行部署。",
        "untouched": "配置文件、模型菜单与已登录的账号。",
        "turns": "若仍有 turn 正在执行，Vellum 会拒绝停止，不会强制中断。"
      }
    },
    "blocker": {
      "agentUnavailable": "无法连接主机上的 Vellum Agent。",
      "codexRestartRequired": "配置已写入，但需重启后才会生效。",
      "credentialsMissing": "主机缺少本次部署所需的凭证。",
      "desktopOfficialAccountMissing": "桌面端尚未选定 ChatGPT 账号，无法与远程配对。",
      "dockerUnavailable": "主机上没有可用的 Docker，Proxy 无法启动。",
      "intelMacUnsupported": "Intel Mac 尚未支持。第一版 Remote Manager 只支持 Apple Silicon。",
      "guiSessionUnavailable": "没有可用的 macOS 登录会话。Proxy 是登录后常驻，不会改电源或自动登录。",
      "proxyPortConflict": "远程 Proxy 默认端口被占用。请在部署计划指定其他端口，或释放 127.0.0.1:15722。",
      "insufficientDiskSpace": "可用空间不足以容纳下载、解压与回滚保留，替换操作已停止。不会自动删除用户数据。",
      "incompleteObservation": "无法完整观察受管理 runtime 的 turn、工具或批准，已阻止破坏性操作。",
      "hostNotConfigured": "此主机尚未写入 Vellum 的配置。",
      "injectionRequiresReadyProxy": "需待 Proxy 就绪后，才能将模型注入 Codex 的菜单。",
      "invalidCompactionThreshold": "压缩门槛的配置值不在合法范围。",
      "managedRuntimeRecoveryRequired": "受管理的 runtime 处于异常状态，需先修复才能继续。",
      "nativeCodexVersionMismatch": "主机上的 Codex CLI 版本不符合，需要安装 pinned 版本。",
      "nativeDaemonAppOwned": "Codex App 目前直接占用此主机的 app-server，Vellum 无法接管。",
      "noModelsSelected": "尚未选择任何模型，没有可同步的内容。",
      "officialAccountActivationRequired": "远程的 ChatGPT 账号已配对，但尚未启用。",
      "officialAccountReauthenticationRequired": "远程 ChatGPT 凭证已过期，请重新登录。",
      "officialAccountPairingRequired": "远程尚未与桌面端选定的 ChatGPT 账号配对。",
      "proxyConfigurationMissing": "主机上尚无 Proxy 配置。",
      "proxyConfigurationSchemaTooNew": "主机上的 Proxy 配置格式比此 Vellum 版本能理解的更新，为避免覆盖已阻止部署——请先更新 Vellum。",
      "proxyConfigurationUnreadable": "无法读取主机上的 Proxy 配置（权限或 I/O 问题），为避免误判为可安全覆盖已阻止部署。",
      "systemdUserUnavailable": "主机上没有可用的 user systemd，daemon 无法常驻。",
      "versionMismatch": "主机上的 Agent 版本与此 Vellum 不相符。"
    },
    "configuration": {
      "upgradeRequired": "远程 Proxy 配置需要安全升级，重新部署即可自动修复。",
      "repairRequired": "既有 Proxy 配置无效，重新部署将会重建。",
      "incompatible": "主机上的 Proxy 配置格式比此 Vellum 版本能理解的更新，请先更新 Vellum 再部署到这台主机。",
      "unreadable": "无法读取主机上的 Proxy 配置（权限或 I/O 问题），需先在主机上排除才能部署。"
    },
    "phase": {
      "queued": "排入队列",
      "cleanHostPreflight": "检查内置套件与干净主机基准",
      "hostPreflight": "检查 SSH、Docker 与用户服务",
      "resolvingDesktopCodex": "解析并验证与 Desktop 相符的 Codex runtime",
      "installCodex": "安装内置的 pinned Codex CLI",
      "nativeDaemon": "开启可常驻的 Codex 远程控制",
      "deploymentPlan": "盘点合格的模型与凭证",
      "deploymentApply": "安装 Proxy、凭证与模型菜单",
      "applying": "应用部署计划",
      "verification": "验证 Proxy、daemon 与脱离准备状态",
      "verified": "已就绪",
      "restorePreflight": "检查进行中的 turn 与受管理状态",
      "restoreLease": "还原 Codex 配置与模型菜单",
      "restartNative": "用还原后的配置重启 native Codex",
      "stopProxy": "停止 Vellum 管理的 Proxy",
      "restoreVerification": "确认 Vellum 已经退出数据路径",
      "restored": "已解除管理",
      "failed": "失败"
    },
    "operation": {
      "bootstrap": "正在部署",
      "apply": "正在同步",
      "restore": "正在解除管理",
      "desktopCodexSync": "正在同步 Desktop Codex runtime",
      "elapsed": "已经过 {{clock}}"
    },
    "error": {
      "discovery": "读取连接配置失败。",
      "desktopCodexMismatch": "远程 Codex runtime 与当前 Desktop 协议不兼容。请先同步 Desktop Codex runtime，然后重试。",
      "boundaryKeyProvisionFailed": "无法修复远程 Proxy 验证密钥。请重试；若持续失败，请导出诊断包协助排查。",
      "actionFailed": "“{{action}}”执行失败。上方主机状态已重新读取，显示当前状态。",
      "operationFailed": "{{action}} 已停止于“{{phase}}”阶段。"
    },
    "detail": {
      "title": "主机详情",
      "host": "主机",
      "runtime": "运行环境",
      "version": "版本",
      "sshResolved": "已解析",
      "sshUnresolved": "无法解析",
      "agentAbsent": "未安装",
      "cores_one": "{{count}} 核",
      "cores_other": "{{count}} 核",
      "diskFree": "可用 {{free}} / 共 {{total}}",
      "dockerAbsent": "未安装",
      "proxyReady": "就绪 · {{image}}",
      "proxyNotReady": "运行中，尚未就绪",
      "config": "配置",
      "proxyStopped": "未启动",
      "launcherLoginShell": "登录 shell",
      "launcherBroken": "不可用，请执行修复",
      "daemonRunning": "运行中 · PID {{pid}}",
      "daemonStopped": "未启动",
      "daemonOwner": "Daemon 所有者",
      "durable": "可常驻",
      "notDurable": "不可常驻",
      "codexSource": "Codex 来源",
      "releaseTrust": "部署包信任",
      "releaseUnverified": "未通过验证 · {{trust}}",
      "verified": "已验证",
      "pinned": "pinned 版本",
      "inventoryBlockers": "盘点发现的阻碍",
      "platform": "平台",
      "proxyBackend": "Proxy 后端",
      "persistence": "常驻范围",
      "loginResident": "登录后常驻",
      "lingerResident": "systemd linger 常驻",
      "managedHome": "受管理的 CODEX_HOME",
      "isolationLabel": "隔离",
      "isolation": "远程设置与本机 Vellum／Enhanced／~/.codex 隔离。"
    },
    "desktopCodex": {
      "desktop": "Desktop Codex",
      "remote": "远程 Codex",
      "status": "协议兼容状态",
      "states": {
        "current": "已通过此 Desktop 的验证",
        "updateAvailable": "需要更新远程 runtime",
        "qualificationRequired": "需要重新验证协议",
        "agentUpdateRequired": "需先更新远程 Agent",
        "desktopUnavailable": "无法获取 Desktop core",
        "unavailable": "无法检查兼容性"
      }
    },
    "account": {
      "title": "ChatGPT 账号",
      "catalogHint": "Desktop 已知账号与这台主机的授权状态",
      "chatgptUnavailable": "无法读取",
      "chatgpt": {
        "ready": "远程凭证可用",
        "synchronized": "已同步",
        "pairingRequired": "需要配对",
        "pairingPending": "配对中",
        "activationRequired": "需要启用",
        "reauthenticationRequired": "凭证已过期",
        "accountUnavailable": "远程尚未选择账号",
        "desktopAccountUnavailable": "桌面端未选定账号"
      },
      "grokReady": "已登录 {{account}}，refresh timer 运行中",
      "grokPartial": "已安装凭证，但远程 CLI 或 refresh token 尚未符合资格",
      "grokAbsent": "未配置",
      "pairingHint": "请使用桌面端选定的账号登录：",
      "pairingRemaining": "还剩 {{remaining}} 个",
      "pairingActive": "这台主机正在使用",
      "pairingPaired": "已配对，未使用",
      "pairingMissing": "这台主机尚未配对",
      "pairingUnknown": "读不到",
      "desktopDefault": "桌面端预设",
      "desktopCurrent": "当前 Desktop Codex 账号",
      "credentialReady": "远程凭证可用",
      "credentialExpired": "远程凭证已过期",
      "credentialMissing": "尚未授权到远程",
      "grokDeviceLogin": "Grok 设备登录：",
      "grokWaitingUrl": "等待登录网址…",
      "grokStarting": "正在等待 Grok CLI…"
    },
    "control": {
      "title": "Remote 控制",
      "identity": "使用账号",
      "status": "凭证状态",
      "choose": "选择 Remote Control 账号",
      "credentialExpired": "这个账号的远程凭证已过期。账号身份仍相同，但模型请求与 Remote Control 验证可能失败。",
      "sameAccountHint": "Desktop 与手机必须使用相同的 ChatGPT 账号及 workspace；设备配对不会改变远程 daemon 身份。",
      "deviceHint": "短期设备配对信息：",
      "expiresAt": "{{when}} 到期"
    },
    "execution": {
      "title": "官方模型执行账号",
      "identity": "模型请求使用",
      "followControl": "跟随 Remote Control 账号",
      "independentHint": "只决定官方模型请求使用哪个账号与额度，不会改变 Remote Control 身份。",
      "inheritedCredentialExpired": "目前跟随 Remote Control 账号，但该远程凭证已过期。请重新登录控制账号，或选择独立的模型执行账号。",
      "empty": "尚未添加由 Vellum 管理的 Official 执行账号。",
      "selected": "使用中",
      "select": "选择",
      "remove": "移除",
      "removeSelected": "移除当前的独立执行账号",
      "namePlaceholder": "这个 Official 账号的显示名称",
      "loginHint": "完成 Official 设备登录："
    },
    "maintenance": {
      "title": "维护"
    },
    "danger": {
      "body": "撤销 Vellum 对此主机的管理，将 Codex 还原至部署前的状态。此操作并非仅断开连接——再次使用需重新部署。你的项目与普通 Codex thread 不会被删除。"
    },
    "sessions": {
      "cap": "原生 session 观察器",
      "title": "Codex daemon 的 thread",
      "unknown": "尚未获取 native session 状态。",
      "empty": "native daemon 目前没有可见的 thread。",
      "unreadable": "目前无法读取 native app-server 的 thread 列表。",
      "turns_one": "{{count}} 个 turn · 最近一次 {{last}}",
      "turns_other": "{{count}} 个 turn · 最近一次 {{last}}",
      "observability": {
        "nativeAppServer": "可观察",
        "unsupported": "不支持",
        "daemonDown": "daemon 未启动"
      }
    },
    "plan": {
      "cap": "部署计划",
      "title": "模型与 policy 同步",
      "ready": "可同步",
      "blocked": "受阻",
      "configHash": "配置哈希",
      "catalogHash": "菜单哈希",
      "reviewPolicy": "自动审查",
      "reviewPolicyPending": "设置已变更 — 待重新应用",
      "credentials": "凭证",
      "noCredentials": "不需要",
      "revision": "版次",
      "revisionValue": "目标 {{desired}} / 现状 {{observed}}",
      "planHash": "计划哈希",
      "rollback": "回滚方式",
      "managed": "受管理字段",
      "changed": "有差异",
      "same": "相同"
    }
  },
  "onboarding": {
    "title": "开始使用 Vellum",
    "acts": { "what": "认识", "connect": "连接", "features": "功能", "launch": "启动" },
    "folio": { "1": "一", "2": "二", "3": "三", "4": "四" },
    "ui": { "providerSeparator": "、", "back": "回上一幕", "skipHint": "可以先不连接任何 Provider，之后在“模型”页添加。", "skip": "先跳过", "continue": "继续", "enter": "进入 Vellum", "enterWithoutProxy": "暂不启动，直接进入", "start": "开始", "skipSetup": "我已设置过，直接进入", "unwritten": "第 {{step}} 幕，尚未进行", "visited": "（已读过，可返回）", "login": "登录", "collapse": "收起", "fillEndpoint": "填写端点", "connected": "已连接", "unavailable": "不可用", "waitingAuth": "等待授权", "notConnected": "未连接", "waitingAuthEllipsis": "等待授权…", "addAnother": "再连接一个", "enterCode": "在浏览器输入此代码", "providerType": "Provider 类型", "customEndpoint": "自定义 API 端点", "opencodeHint": "OpenCode Zen 使用官方固定端点。只需输入 API key，Vellum 会获取并验证兼容模型。", "opencodeApiKey": "OpenCode Zen API key", "opencodeApiKeyPlaceholder": "粘贴 OpenCode Zen API key", "displayName": "显示名称", "displayNamePlaceholder": "例如 自建 vLLM", "endpoint": "端点", "optionalPlaceholder": "不需要则留空", "endpointHint": "留空表示不验证此端点。", "probing": "探测中…", "probeAndAdd": "探测并添加", "notConnectedYet": "尚未连接" },
    "overture": { "lead": "刮掉，重写。", "history": "中世纪的犊皮纸很昂贵，写满后会刮掉重写，而没有刮干净的旧字迹会从底下透出来。", "mission": "这个程序做的也是同一件事：上下文满了不截断，重写一次，保留重要内容。", "subtitle": "只运行在你电脑上的 Codex 代理 · 接下来的四幕都写在这张纸上" },
    "what": { "axisTargets": "ChatGPT ／ Grok ／ 自定义", "title": "它站在中间", "lead": "Codex 照常访问原本的端点。Vellum 在中间把请求转给你选择的 Provider——换 Provider、换模型、调整上下文，都不必修改 Codex 配置文件。", "axisAria": "Codex CLI 经由本机 Vellum Proxy 连接 ChatGPT、Grok 或自定义端点", "client": "你正在使用", "local": "本机 · 不外送", "answering": "实际回答的", "noConfigTitle": "不修改配置", "noConfigBody": "启动时接管 Codex 端点，停止时原样恢复。不需要手动修改，也不会留下半成品设置。", "noTruncateTitle": "不做硬性截断", "noTruncateBody": "接近门槛时，会先把对话整理成包含目标、已完成事项和下一步的检查点，再继续往下走，而不是删掉前面的历史。", "reviewTitle": "批准可以交给其他模型", "reviewBody": "原本需要你按 y/n 的操作可以交由其他模型评估。沙箱、网络和文件权限保持不变。", "privacy": "全部运行在这台电脑上。Vellum 没有服务器，你的对话不会经过我们；凭证会加密保存在本机。" },
    "connect": { "title": "连接一家 Provider", "lead": "一家就够开始。之后可以再加，也可以同时开着好几家——不同会话走不同 Provider，额度快用完的那家会先被看见。", "chatgptClaim": "使用现有订阅额度", "chatgptDetail": "使用官方设备码登录，不需要 API key。订阅中的 Reset 额度也会带入，之后可在“模型”页使用。", "grokClaim": "使用官方 CLI 登录流程", "grokDetail": "需要在本机安装 Grok CLI。登录后可以连接多个账号并分别统计额度。", "grokUnavailable": "尚未安装 Grok CLI，或目前无法检测。", "customName": "自定义端点", "customClaim": "任何 OpenAI 兼容服务", "customDetail": "填写端点和 API key，Vellum 会自动探测模型和上下文长度。自建、代理、第三方服务都使用此路径。", "codexToolProtocolUnavailable": "端点可以连接，但没有模型返回 Codex 可执行的结构化工具调用。请先在 vLLM、Ollama 或 llama.cpp 启用工具调用协议。", "credentialNote": "凭证使用系统密钥链加密保存在本机，不会写入 Codex 配置或日志。" },
    "features": { "title": "这些功能已经可用", "lead": "无需额外设置即可生效。这一幕只告诉你功能在哪里，以及之后去哪里修改。", "reviewTerm": "自动审查\n可以换模型", "settingsPage": "设置", "reviewBody": "Codex 的批准请求可交给指定 Provider 和模型评估，也可以设置备用模型。主力额度用完时会标示正在使用备用模型。", "resetTerm": "Reset 额度\n直接在这里使用", "modelsPage": "模型", "resetBody": "ChatGPT 账号的 Reset 额度显示在账号行，使用一笔后立即刷新。", "sessionTerm": "每个窗口\n独立计算", "todayPage": "现状", "sessionBody": "每个 Codex 窗口都有自己的对话和上下文窗口。现状页会优先显示最接近压缩门槛的窗口，而不是平均成一个数字。" },
    "launch": { "title": "把 Codex 接过来", "runningTitle": "正在使用 Vellum", "lead": "启动后 Vellum 会接管 Codex 端点设置，停止时原样恢复。此操作可随时撤回。", "runningLead": "端点已接管，请求会转发到已连接的 Provider。停止后恢复安装 Vellum 之前的设置。", "startProxy": "启动 Proxy", "noProviderHint": "还没有连接任何一家——可以先启动，但连接之后才会有模型回答。", "connectedProviders": "已连接的 Provider", "howToStart": "如何开始", "howToStartValue": "打开新的 Codex 窗口。已有窗口需要重启才能使用 Vellum。", "howToStop": "如何停止", "howToStopValue": "在“现状”页点击“停止 Proxy 并恢复 Codex”", "reopenGuide": "重新查看导览", "reopenGuideValue": "“设置”页" }
  }
};

export default zhCN;
