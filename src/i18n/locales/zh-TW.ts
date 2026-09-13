import type { ResourceTree } from "../resources";

const zhTW: ResourceTree = {
  "enhanced": {"heading":"Enhanced 執行狀態","lead":"顯示每條路由的執行歸屬、相容性檢查結果，以及手機連線狀態。","title":"Enhanced Core","subtitle":"執行歸屬、相容性證據與手機連線","serving":"Enhanced Core 正在服務","notServing":"Enhanced Core 尚未服務","sessions":"已觀測工作階段","running":"執行中回合","freshness":"觀測狀態","restartRequired":"已準備新的啟動組合。請於活動回合結束後安全重啟。","launchCoreDrift":"Codex Desktop 已更新核心。Vellum 會在回合閒置時自動重啟 Proxy；完成後請重新啟動 Codex。","now": "現在", "runningTurns_one": "{{count}} 個回合執行中", "runningTurns_other": "{{count}} 個回合執行中", "sessionCount_one": "{{count}} 個工作階段", "sessionCount_other": "{{count}} 個工作階段", "verdict": {"serving": "Codex Desktop 正在透過 Enhanced Core 執行。", "servingOtherLaunch": "Enhanced 正在服務，但使用的是上一次啟動的設定。重新啟動 Codex 才會套用目前的設定。", "stoppedAt": "啟動程序在「{{link}}」階段停止。", "disabled": "Enhanced Core 已停用，Codex Desktop 正使用原生核心。"}, "chain": {"title": "啟動程序", "artifact": "元件驗證", "armed": "路徑接管", "adopted": "Bridge 對接", "inPlace": "執行確認"}, "mark": {"blocked": "未通過", "waiting": "等待中", "inPlace": "已生效"}, "whyStopped": "「{{link}}」階段停止的原因", "whyNoted": "執行中，但存在需注意的項目", "protocol": {"verified": "已驗證", "unverified": "未驗證", "incompatible": "不相容"}, "protocolUnrun": "比對尚未執行", "unverifiedNote": "此 Codex Desktop 版本未通過驗證。路由所需的方法均存在，因此路由可用；列出的差異未經測試，正確性不保證。", "missingHelpers": "Enhanced 核心缺少官方安裝提供的輔助程式：{{list}}。對話與路由不受影響，沙箱命令執行會失敗。", "routed": "路由所需", "delta": {"methodUnservable": "此方法無法提供", "methodUnknownToDesktop": "Desktop 未宣告此方法", "fieldNewlyRequired": "新增必填欄位", "fieldRemoved": "欄位已移除"}, "environment": {"leased": "已接管", "released": "未接管", "orphanedBridge": "殘留舊 bridge", "foreignValue": "由其他工具設定", "unreadable": "無法讀取"}, "bridgeProcess": "Bridge 程序", "bridgeState": "Bridge 狀態", "children": "子程序", "launch": "啟動", "relayNote": "手機連線需通過握手、任務列表、訊息載入、即時事件與任務控制五個階段；未完成的階段即為連線失敗原因。", "staleWarning":"目前顯示最後一次觀測，資料可能已過期。","compatibility":"版本與相容性","activeRuntime":"目前使用的 runtime","candidateRuntime":"下次啟動使用的 runtime","structure":"Schema 比較","qualification":"最近驗證","notObserved":"未觀測","observed":"已觀測","evidenceNote":"Schema 相同僅代表結構證據，不能證明 Desktop 或手機功能已通過實機驗證。","differences":"差異與阻擋原因","relay":"Relay 狀態","clients":"已連線客戶端版本","handshake":"握手","list":"任務列表","history":"訊息載入","stream":"即時事件","control":"任務控制","failureStage":"最近失敗階段","plane":"執行歸屬","all":"全部","empty":"此次啟動尚無工作階段觀測。歷史綁定不等於目前連線。","session":"工作階段","state":"狀態","lastActivity":"最後活動","parent":"父任務","diagnostics":"診斷","recheck":"重新檢查相容性","export":"匯出診斷","exported":"已儲存","states":{"starting":"啟動中", "ready":"就緒", "degraded":"降級", "stopped":"已停止", "transportReady":"傳輸就緒", "unknown":"未知","current":"最新","stale":"已過期","offline":"離線","unavailable":"無觀測資料","running":"執行中","idle":"閒置","approval":"等待批准","unloaded":"已卸載","observed":"已觀測","attached":"已附加","failed":"失敗"}},
  "common": { "expiresAt": "到期時間：{{month}} 月 {{day}} 日 {{time}}（{{zone}}）", "expiredAlready": "已過期", "expiresInMinutes": "剩 {{value}} 分鐘", "expiresInHours": "剩 {{value}} 小時", "expiresInDays": "剩 {{value}} 天","emDash": "—", "none": "—", "listSeparator": "；", "itemSeparator": "、", "loading": "讀取中…", "processing": "處理中…", "refreshing": "更新中…", "refresh": "重新整理", "reorganize": "重新整理", "save": "儲存", "cancel": "取消", "confirm": "確認", "close": "關閉", "remove": "移除", "enabled": "啟用", "disabled": "停用", "yes": "是", "no": "否", "on": "開", "off": "關", "unknown": "未知", "provider": "供應商", "model": "模型", "proxy": "Proxy", "codexCatalog": "Codex 模型選單", "justNow": "剛剛", "minutesAgo": "{{count}} 分鐘前", "hoursAgo": "{{count}} 小時前", "daysAgo": "{{count}} 天前", "resetAt": "{{month}} 月 {{day}} 日 {{time}} 重置", "previousPage": "上一頁", "nextPage": "下一頁", "pageOf": "第 {{page}} / {{total}} 頁", "shortcutTitle": "{{label}}（Ctrl+{{n}}）", "statusUpdateFailed": "狀態更新失敗：{{detail}}", "errorWithDetail": "{{message}}：{{detail}}", "connectionFailed": "連線失敗", "operationFailed": "操作失敗", "loadFailed": "載入失敗", "saveFailed": "儲存失敗", "visible": "顯示", "hidden": "隱藏", "totalItems_one": "共 {{count}} 筆", "totalItems_other": "共 {{count}} 筆", "itemsNeedAttention_one": "個項目需要處理", "itemsNeedAttention_other": "個項目需要處理", "notice": {"info": "說明", "warn": "注意", "error": "失敗"}, "dismiss": "知道了"},
  "navigation": {
    "mainAria": "主導覽",
    "today": {
      "label": "現況",
      "title": "現況",
      "blurb": "Proxy、Provider 與額度狀態"
    },
    "models": {
      "label": "模型",
      "title": "模型",
      "blurb": "Provider 與 Codex 模型選單"
    },
    "context": {
      "label": "上下文",
      "title": "上下文",
      "blurb": "上下文用量、壓縮預覽與原文"
    },
    "enhanced": {
      "label": "增強",
      "title": "增強",
      "blurb": "執行歸屬與相容性狀態"
    },
    "remote": {
      "label": "遠端",
      "title": "遠端總管",
      "blurb": "Codex App SSH 主機與原生 daemon"
    },
    "log": {
      "label": "紀錄",
      "title": "紀錄",
      "blurb": "Token 統計與請求紀錄"
    },
    "settings": {
      "label": "設定",
      "title": "設定",
      "blurb": "應用程式與 Codex 整合設定"
    }
  },
  "status": {
    "live": "生效中",
    "pending": "待生效",
    "off": "已停用",
    "reading": "讀取中…",
    "enhancedLoaded": "已載入",
    "enhancedNotLoaded": "未載入",
    "enhancedUnverified": "已載入（未驗證）",
    "proxyStopped": "Proxy 未啟動",
    "codexNotManaged": "Codex 未導向 Vellum",
    "lastSuccessfulRequest": "最近成功請求",
    "defaultRoute": "預設線路",
    "noProviderYet": "還沒有供應商",
    "quotaRemaining": "額度剩 {{percent}}%",
    "restartCodexRequired": "需重啟 Codex",
    "updateAvailable": "更新可用",
    "updateWaitingIdle": "已下載，等待閒置",
    "updateWaitingRestart": "已下載，下次啟動套用",
    "updateFailed": "更新失敗或已回復",
    "remedy": {
      "startProxy": "啟動 Proxy 後生效",
      "restartProxy": "重啟 Proxy 後生效",
      "restartCodex": "重啟 Codex 後會出現在模型選單"
    }
  },
  "vocabulary": {
    "quotaUnavailable": "這個端點不提供額度查詢",
    "source": {
      "override": "你手動填的值",
      "modelCache": "供應商的模型快取",
      "catalog": "Codex 模型選單",
      "fallback": "保底值"
    }
  },
  "quota": {
    "period": {
      "week": "週額度",
      "month": "月額度",
      "hours_one": "{{count}} 小時額度",
      "hours_other": "{{count}} 小時額度",
      "days_one": "{{count}} 天額度",
      "days_other": "{{count}} 天額度",
      "unspecified": "使用額度"
    },
    "remainingLabel": "{{period}}剩餘 {{remaining}}%"
  },
  "review": {
    "policy": {
      "always": {
        "label": "僅使用指定模型",
        "blurb": "所有審查均使用指定模型。該模型不可用或額度耗盡時，自動審查將停止並回報錯誤。"
      },
      "failover": {
        "label": "指定模型優先，必要時使用備援",
        "blurb": "指定模型額度耗盡或連線失敗時，自動切換至備援模型以繼續審查。"
      }
    }
  },
  "heatmap": {
    "tokens": "{{value}} 個 token",
    "gridAria": "token 用量熱區圖 · {{mode}}",
    "mode": {
      "daily": "每日",
      "weekly": "每週",
      "cumulative": "累計"
    },
    "monthLabel": "{{month}}月",
    "aria": "使用量熱區圖",
    "tooltip": "{{from}} – {{to}} · {{requests}} 次請求 · {{tokens}} tokens",
    "range": "{{from}} 至 {{to}}",
    "requestsTokens_one": "{{count}} 次請求 · {{tokens}} tokens",
    "requestsTokens_other": "{{count}} 次請求 · {{tokens}} tokens"
  },
  "runtime": {"notice": {"codexConfigStillPointedAtVellum": "Codex 的設定仍指向 Vellum。啟動 Proxy 後重新啟動 Codex App 即可接續使用。", "codexRunning": "Codex 正在執行；請重新啟動以載入 Proxy 與模型選單", "proxyStoppedCodexRestartRequired": "Proxy 已停止並還原設定；請重新啟動 Codex 以離開舊的 Proxy／Bridge 連線", "codexRestartDetected": "已偵測到 Codex 重新啟動；Proxy 與模型選單已載入", "routesAndCatalogUpdated": "線路設定與模型選單已更新；重新啟動 Vellum Proxy 與 Codex 後生效", "catalogRestored": "已還原模型選單；Codex 需要重新啟動", "catalogUpdated": "模型選單已更新", "enhancedDesktopRuntimeChanged": "Enhanced Codex Runtime 設定已變更；請重新啟動 Codex 以套用", "enhancedLaunchCoreRepaired": "Codex Desktop 已更新核心，Vellum 已自動重啟 Proxy 並準備新的啟動設定；請重新啟動 Codex（原啟動 {{launchId}}）", "enhancedLaunchCoreRepairFailed": "Codex Desktop 更新後，Vellum 無法自動重啟 Proxy：{{reason}}。請手動重啟 Proxy，再重新啟動 Codex。", "remoteOfficialAccountSwitchNotFollowed": "遠端主機 {{host}} 沒有跟著切換帳號。請到 Remote Manager 配對或對齊。原因：{{detail}}", "officialAccountSwitchNativePlane": "Official GPT 仍走未修改的 Codex 核心；Enhanced bridge 啟用時，Vellum 帳號切換不會取代 Codex Desktop 的登入帳號", "enhancedDesktopBridgeReady": "Codex 已重新啟動，且走的是 Enhanced bridge（啟動 {{launchId}}）", "enhancedDesktopBridgeFailed": "Codex 已重新啟動，但 Enhanced bridge 失敗：{{reason}}", "enhancedDesktopRuntimeNotArmed": "Proxy 已啟動，但 Enhanced Codex 沒有武裝：第三方 Provider 的對話會由原生 Codex 執行。原因：{{detail}}", "enhancedDesktopBridgeNotObserved": "Codex 已重新開啟，但啟動 {{launchId}} 沒有任何 Enhanced bridge 回報；Enhanced 沒有在跑", "routeHotSwapped": "路由已熱切換", "slotSwitched": "{{slot}} 已切換至 {{model}}", "restartSucceeded": "Codex 已安全重新啟動；新的執行期設定已載入", "restartNotDetected": "已送出 Codex 啟動要求，但 5 秒內未偵測到 App；請手動開啟 Codex", "restartProcessStillRunning": "Codex 尚未完全關閉（停止代碼 {{exitCode}}）；已取消重新啟動", "restartBlockedByActiveRequests_one": "仍有 {{count}} 個請求執行中", "restartBlockedByActiveRequests_other": "仍有 {{count}} 個請求執行中", "restartBlockedByCodexTurn_one": "Codex Desktop 有 {{count}} 個對話正在進行；現在重新啟動會丟掉它還沒寫進硬碟的內容", "restartBlockedByCodexTurn_other": "Codex Desktop 有 {{count}} 個對話正在進行；現在重新啟動會丟掉它們還沒寫進硬碟的內容", "restartExecutableMissing": "找不到可驗證的 Codex App 執行檔", "restartExecutableInvalid": "Codex App 執行檔不存在或不是絕對路徑", "restartUnavailableInPreview": "瀏覽器預覽不會重新啟動 Codex", "usageOAuthNotConfigured": "尚未在 Vellum 設定 OpenAI OAuth；OpenAI 暫以 Proxy 紀錄估算", "usageProfileUnavailable_one": "{{count}} 個 OpenAI OAuth 帳號的 Codex Token 統計暫時無法讀取", "usageProfileUnavailable_other": "{{count}} 個 OpenAI OAuth 帳號的 Codex Token 統計暫時無法讀取", "webSearchDisabledMissingBraveKey": "由於尚未設定 Brave Search API 金鑰，第三方網頁搜尋已被停用。請在設定中新增金鑰以重新啟用。"}, "alerts": {"label": "執行階段警告"}},
  "settings": {
    "language": {
      "title": "語言",
      "blurb": "選擇介面語言。選「跟隨系統」會依作業系統語言自動判定。",
      "option": {
        "system": "跟隨系統",
        "zh-TW": "繁體中文",
        "zh-CN": "简体中文",
        "en": "English",
        "ja": "日本語"
      },
      "applied": "語言已套用"
    },
    "title": "設定",
    "page": {
      "subagent": { "title": "子代理預設", "description": "預設情況下，子代理使用發起它那次對話的模型與推理強度。這裡可以改成固定的預設值；任務若明確指定模型或 Effort，仍以任務指定為優先。", "desktopReady": "Codex Desktop 已就緒", "desktopUnavailable": "Codex Desktop 無法套用此設定", "desktopVersion": "Desktop 版本 {{version}} · 原生子代理預設可用", "defaultsTitle": "未指定時使用的模型", "mode": { "inherit": "保留 Desktop 現有設定", "custom": "由 Vellum 指定預設" }, "effort": "推理強度", "autoEffort": "自動（模型預設）", "effortNotProbed": "此模型的推理強度尚未驗證；請先到 Models 頁面探測能力。", "modelEmpty": "此 Provider 目前沒有可選的模型。", "hint": "這些只是預設值；代理可依任務明確指定其他已啟用模型與 Effort。", "unavailable": "所選的 Provider 或模型目前已不可用。依賴此預設建立的子代理會明確失敗，直到更新設定。", "unsupported": "無法驗證 Codex Desktop 的原生子代理設定：{{detail}}" },
      "loading": "正在載入設定…", "heading": "審查、搜尋、顯示與系統復原", "unspecified": "未指定", "saved": "設定已儲存",
      "review": { "title": "自動審查", "toggle": "啟用自動核准審查", "description": "由指定模型評估 Codex 提出的核准請求。此功能不變更沙箱、網路或檔案存取權限，僅調整核准決策所使用的審查模型。", "strategyTitle": "審查模型策略", "currentLabel": "目前生效", "fallbackActive": "使用備援模型", "selectedUnavailable": "指定模型目前不可用", "provider": "指定 Provider", "model": "指定模型", "fallbackModel": "備援模型", "billingAccount": "計費帳號", "billingFollowsDefault": "跟隨目前使用的帳號", "billingAccountMissing": "帳號 {{account}} 未在本機登入。自動審查不會改用其他帳號計費，而是直接失敗；請從清單重選，或重新登入該帳號。", "savedRemotePending": "已儲存，本機 proxy 立即生效；{{count}} 台遠端主機需要到 Remote Manager 重新套用。", "rulesTitle": "自動審查規則", "willReview": "審查範圍", "willReviewValue": "需要離開沙箱、存取受限網路或受保護路徑的操作", "willNotReview": "排除範圍", "willNotReviewValue": "已由沙箱政策允許的操作", "whenRejected": "審查未通過", "whenRejectedValue": "Codex 將改用風險較低的替代方案，不執行原核准請求" },
      "stats": { "title": "各 Provider 審查次數", "summary": "備援 {{fallback}} / {{total}} 次（{{percent}}%）", "primary": "指定", "fallback": "備援", "failed": "失敗", "lastUsed": "最後使用時間", "neverUsed": "尚無紀錄", "empty": "自動審查目前尚無執行紀錄。Codex 下一次提出核准請求後，系統將記錄實際使用的 Provider。" },
      "webSearch": { "title": "網頁搜尋", "toggle": "啟用第三方 Provider 網頁搜尋", "description": "啟用後，非官方 Provider 可使用 Codex 網頁搜尋工具，查詢由 Brave Search 處理。停用時，Vellum 會在轉送請求前移除搜尋工具。OpenAI 官方 Provider 使用原生搜尋服務，不受此設定影響。", "braveKey": "Brave Search API 金鑰", "braveKeySaved": "已設定；輸入新金鑰即可更新", "braveKeyPlaceholder": "輸入 Brave Search API 金鑰", "braveKeyHint": "API 金鑰會以加密形式儲存於本機，不寫入一般設定檔，亦不會提供給模型。", "reach": "網頁存取範圍", "reachOption": { "indexed": "僅使用索引結果", "live": "允許擷取公開網頁" }, "reachHint": { "indexed": "模型僅接收搜尋後端回傳的標題與摘要，不擷取原始網頁內容。", "live": "模型可擷取搜尋結果中的公開 HTTP(S) 網頁；本機、私有網路及雲端中繼資料位址均會封鎖。" }, "probe": "搜尋連線測試", "probeAction": "執行測試", "probing": "正在測試…", "probeHint": "送出最小查詢以驗證 Brave Search 的連線與回應狀態。", "probeOk": "Brave Search 已回傳 {{count}} 筆結果", "probeEmpty": "Brave Search 連線正常，但未回傳搜尋結果", "probeFailed": "連線測試失敗：{{detail}}", "probedAt": "{{when}}完成測試", "needsBraveKey": "網頁搜尋已啟用，但尚未設定 Brave Search API 金鑰；新增金鑰前查詢會一直失敗。", "domainTray": "網域存取規則", "domainAllow": "允許清單", "domainBlock": "封鎖清單", "domainHint": "每行輸入一個網域。兩份清單皆為空時不限制網域；設定允許清單後，搜尋結果僅保留清單內的網域。" },
      "dashboard": { "title": "儀表板 Provider 顯示設定", "note": "已選擇 {{count}} 個 Provider。此設定僅控制現況頁的顯示內容，不會變更 Provider 的啟用狀態。", "disabledVisible": "顯示（已停用）" },
      "restore": { "title": "還原 Codex 原始設定", "description": "移除 Vellum 的 Proxy 位址、boundary key 與模型選單項目，並還原原本的預設 Provider。為了讓既有 Vellum 對話仍可由原生 Codex 開啟，會保留一個不經 Proxy、直接連到 OpenAI 的相容 Provider。登入憑證、聊天紀錄、專案分組及工作區資料均予以保留。", "action": "還原 Codex 原始設定", "cleared": "設定已還原", "result": "執行結果", "nothingToClear": "未偵測到需要移除的 Vellum 設定", "preserved": "保留項目：{{items}}。" },
      "enhancedRuntime": {"title": "Enhanced Codex Runtime", "description": "第三方 Provider 的新對話改由 Enhanced Codex 執行；OpenAI 官方模型維持原生 Codex Runtime。切換執行平面必須開新對話。", "desiredState": "要求狀態", "desiredEnabled": "要求啟用", "desiredDisabled": "要求停用", "activation": {"disabled": "未啟用", "artifactBlocked": "執行檔驗證失敗", "awaitingDesktopRestart": "等待重新啟動 Codex Desktop", "active": "已生效", "disablePendingRestart": "已要求停用，等待重新啟動", "environmentDrift": "接管狀態不一致", "failed": "Bridge 啟動失敗"}, "environment": {"leased": "Vellum 已安全接管", "released": "尚未接管", "orphanedBridge": "殘留舊版 Vellum bridge", "foreignValue": "由其他工具設定，Vellum 沒有覆寫", "unreadable": "無法讀取使用者環境變數"}, "environmentValue": "CODEX_CLI_PATH 目前的值", "environmentUnset": "尚未設定", "staleBridge": "這條路徑是舊版 Vellum 留下的 bridge，不是這個組建要用的那一支。按「停用並釋放接管」清掉它，再重新啟用。", "launchDetail": "{{launchId}} · {{state}} · bridge {{bridgePid}} · Official {{officialPid}} · Enhanced {{enhancedPid}}", "officialBinary": "官方 Codex core", "officialBinaryHint": "從已安裝的 Codex Desktop 偵測。必須與 Desktop 實際執行的那一支相同，所以不開放更改。", "enhancedBinary": "Enhanced Codex core", "bridgeBinary": "App Server bridge", "bridgeBinaryHint": "隨這個 Vellum 組建一起提供，雜湊寫死在程式裡，不能換成別的檔案。", "protocol": {"label": "協定檢測", "unavailable": "無法檢測", "details": "差異與診斷（{{count}}）", "routed": "路由", "verdict": {"verified": "與釘選版本一致", "unverified": "可用，但不保證正確性", "incompatible": "不相容，已回退"}, "mean": {"verified": "這正是這個 Vellum 版本釘選的組合，兩邊的協定逐位元組相同。", "unverified": "這個 Codex Desktop 版本沒有人驗證過。bridge 分派要用到的方法都還在，所以路由沒問題；下面列的差異是真的，只是沒被測過。", "incompatible": "Codex Desktop 動到了 bridge 分派時必須用的方法，沒有可以降級的跑法。"}, "delta": {"shapeIncompatible": "{{subject}} 的 wire 形狀對方不接受：{{fields}}", "shapeUnverified": "{{subject}} 的 wire 形狀無法判定：{{fields}}", "methodUnservable": "Desktop 可能呼叫 {{subject}}，Enhanced core 沒有實作", "methodUnknownToDesktop": "Enhanced core 可能送出 {{subject}}，Desktop 已不再宣告", "fieldNewlyRequired": "Desktop 現在要求 {{subject}} 的 {{fields}}，Enhanced core 不會送出", "fieldRemoved": "Desktop 不再送出 {{subject}} 的 {{fields}}，Enhanced core 會讀它"}, "fallback": "已回到原生 Codex。Codex 本身照常運作，只是 Enhanced 的功能都不在。"}, "protocolHash": "App Server 協定", "observedBridge": "實際執行中的 bridge", "unverified": "尚未驗證", "details": "技術細節與排查資料", "runInstalledGate": "重新執行安裝版驗證", "installedGateHint": "比重開一次更深的驗證：重新啟動 Codex Desktop，跑完整組安裝版 gate 並留下報告。", "lastQualification": "上次驗證", "qualificationPassed": "{{mode}} 通過，可以升級", "qualificationComponentOnly": "{{mode}} 元件測試通過，但尚未達到實機升級門檻", "qualificationFailed": "{{mode}} 未通過 —— {{detail}}", "qualificationReport": "報告", "planeTitle": "每條路由現在走哪一邊", "contextNote": "壓縮由執行對話的 runtime 自己負責：官方路徑用 Official Codex 原生壓縮，第三方路徑用 Enhanced Codex 的本機壓縮與上下文回復。Vellum 不再另外套用壓縮策略。", "planeOfficial": "Official Codex · 原生壓縮", "planeEnhanced": "Enhanced Codex · 本機壓縮與上下文回復", "planeUnbound": "尚未綁定 Enhanced Codex", "activationLabel": "啟用狀態", "environmentLabel": "接管狀態", "inject": {"on": "已注入", "off": "未注入", "working": "注入中", "switchLabel": "注入 Enhanced Codex Runtime", "mean": {"staleLaunch": "Codex Desktop 現在正走在 Enhanced 上，但用的是較早那次啟動的設定。要讓最新的設定生效，重新啟動 Codex Desktop。", "on": "Codex Desktop 正跑在 Vellum 的 bridge 上。第三方 Provider 的新對話由 Enhanced Codex 執行，OpenAI 官方模型維持原生 Codex。", "off": "Codex Desktop 跑在原生 Codex 上。CODEX_CLI_PATH 沒有被 Vellum 動過。", "wanted": "Vellum 這邊設定好了，但 Codex Desktop 還沒接上，所以現在跑的還是原生 Codex。", "working": "正在驗證執行檔、接管 CODEX_CLI_PATH，並重新啟動 Codex Desktop。", "armed": "重新啟動 Codex Desktop 後生效。"}, "blocked": {"proxyStopped": "Proxy 沒有啟動。Proxy 沒在跑的時候 Vellum 不會接管啟動路徑 —— 不然 Codex Desktop 會啟動到一個後面沒有資料面的 bridge。先啟動 Proxy。", "stuck": "要求過注入，但路徑沒有被接管。下面的終端機寫著卡在哪一步；再扳一次開關可以重試。", "noCore": "這個 Vellum 組建裡沒有 Enhanced Codex core。這不是你漏了設定 —— core 由組建自己帶，缺了代表安裝不完整，請重新安裝 Vellum。"}}, "term": {"idle": "還沒跑過。扳動上面的開關，每一步會印在這裡。", "verify": "核對 Enhanced Codex 執行檔", "verifyOk": "檔案內容與這個版本預期的一致", "verifyFailed": "核對沒過，什麼都沒有動", "restart": "接管 CODEX_CLI_PATH 並重新啟動 Codex Desktop", "restartBack": "重新啟動 Codex Desktop，回到原生 Runtime", "restartRefused": "沒有重新啟動", "adopt": "確認 Codex Desktop 接上了", "injected": "已注入 · {{profile}}", "notInjected": "Codex Desktop 沒有接上 bridge", "release": "釋放 CODEX_CLI_PATH", "releasedOk": "已釋放，Codex Desktop 回到原生 Runtime", "envUnset": "（未設定）", "preflight": "檢查前置條件", "proxyStopped": "Proxy 沒有啟動，接管會被釋放掉。先啟動 Proxy 再注入。", "armed": "啟動路徑已接管。下次啟動 Codex Desktop 就會生效。", "alreadyInjected": "Codex Desktop 已經跑在這座 bridge 上了，不用重新啟動 · {{profile}}", "missingHelper": "缺少輔助程式 {{name}}"}, "missingHelpers": "這支 Enhanced Codex 旁邊少了官方安裝有的輔助程式：{{names}}。Codex 在執行期才到自己旁邊找它們，找不到會變成一個 Windows「找不到檔案」對話框。對話與路由不受影響；會壞的只有用到那支程式的功能 —— 沙箱 shell 指令要 codex-windows-sandbox-setup.exe 與 codex-command-runner.exe；code mode 要 codex-code-mode-host.exe，而 code mode 預設是關的，關著就沒有影響。", "enhancedBinaryHint": "由這個 Vellum 組建自己帶，並用 enhanced-runtime.lock.json 裡的雜湊逐位元組比對。換成別的只會驗證失敗，所以不開放選擇。", "coreMissing": "這個組建沒有帶"},
      "updates": { "title": "軟體更新", "description": "本體、Remote 套件與 Enhanced core 可獨立更新。已下載的更新會保持待套用，直到符合套用條件。", "liveDisabled": "在發布簽署設定完成前，自動更新維持停用。", "actionFailed": "更新操作失敗：{{error}}", "autoCheck": "自動檢查更新", "autoDownload": "自動下載更新", "channel": "通道", "stable": "穩定", "preview": "預覽", "current": "目前版本", "available": "可用版本", "none": "無", "progress": "下載進度", "applyWhen": "套用條件", "notes": "發行說明", "failure": "失敗原因", "check": "檢查更新", "download": "下載", "apply": "套用", "cancel": "取消下載", "rollback": "回復", "hosts": "主機", "idleAuto": "閒置時自動更新", "idleHandoff": "預覽：閒置時套用 Enhanced core（預設關閉）", "layerDisabled": "此層更新尚未在目前建置中啟用。", "layer": { "desktop": "Vellum 本體", "remote": "Remote 套件", "core": "Enhanced Codex core" }, "condition": { "restartVellum": "已準備。重新啟動 Vellum 後套用。", "hostIdle": "已下載。等待主機閒置。", "nextCoreStart": "已下載。下次核心啟動時套用。", "download": "下載後暫存。", "failed": "見失敗原因。", "idle": "已是最新。", "checking": "檢查中…", "available": "有可用更新。", "downloading": "下載中…", "verifying": "驗證中…", "staged": "已暫存。", "waitingForIdle": "等待閒置。", "waitingForRestart": "等待重新啟動。", "applying": "套用中…", "validating": "驗證中…", "applied": "已套用。", "blocked": "已阻擋。", "rolledBack": "已回復。" }, "phase": { "idle": "待命", "checking": "檢查中", "available": "可用", "downloading": "下載中", "verifying": "驗證中", "staged": "已暫存", "waitingForIdle": "等待閒置", "waitingForRestart": "下次啟動套用", "applying": "套用中", "validating": "驗證中", "applied": "已套用", "blocked": "已阻擋", "failed": "失敗", "rolledBack": "已回復" } },
      "advanced": { "title": "進階設定", "description": "提供請求接收控制、Codex 安全重新啟動及模型選單還原功能，適用於故障排除與系統維護。", "activeRequests": "執行中的請求", "drainTitle": "暫停接收新請求", "draining": "已暫停接收新請求，正在等待執行中的請求完成", "accepting": "目前正常接收請求", "drainHint": "執行中的請求不會中斷。暫停後，Vellum 將拒絕所有新請求，直到手動恢復接收。", "resume": "恢復接收新請求", "stop": "暫停接收新請求", "catalogVersion": "目前模型選單版本", "notCreated": "尚未建立", "restartTitle": "重新啟動 Codex", "restartHint": "等待執行中的請求安全完成後，重新啟動 Codex。", "restartAction": "重新啟動 Codex", "restartAnyway": "仍要重新啟動", "guideTitle": "初次設定導覽", "guideHint": "重新檢視初次設定或連接其他 Provider，不會重置現有設定。", "guideAction": "開啟設定導覽", "catalogHistory": "模型選單版本紀錄", "rollback": "還原", "noVersions": "目前沒有可供還原的模型選單版本。每次更新模型選單時，系統會自動建立版本紀錄。", "restartRequired": "重新啟動後生效", "applied": "目前已生效" },
      "logs": { "title": "診斷 Log", "description": "將 Vellum、Proxy 與 Enhanced Runtime 保留的文字 log 匯出成 ZIP。匯出時會再次遮蔽金鑰、Token 與使用者路徑；不包含憑證、設定、聊天紀錄或資料庫。", "export": "匯出全部 Log ZIP", "exporting": "正在匯出…", "exported": "已匯出至 {{path}}", "hint": "ZIP 會儲存到下載資料夾，並附上檔案清單與截短紀錄。" },
      "app": { "title": "應用程式控制", "exit": "結束 Vellum", "exitHint": "結束 Vellum 時將停止 Proxy 並還原 Codex 原始連線設定；聊天紀錄與專案資料不受影響。" },
      "errors": { "partialRefresh": "部分資料更新失敗：{{detail}}", "reviewSave": "無法儲存自動審查設定：{{detail}}", "reviewNoModelAvailable": "尚未設定任何已啟用的審查模型，請先新增或啟用一個 Provider 再切換為固定模型。", "reviewNoFallbackAvailable": "備援需要另一個不同 Provider 的已啟用審查模型，請先新增或啟用一個。", "restore": "還原失敗：{{detail}}", "drain": "無法更新新請求接收狀態：{{detail}}", "restart": "無法重新啟動 Codex：{{detail}}", "rollback": "無法回復模型選單：{{detail}}", "webSearchSave": "無法儲存網頁搜尋設定：{{detail}}", "subagentSave": "無法儲存子代理設定：{{detail}}", "logExport": "無法匯出診斷 Log：{{detail}}" }
    }
  },
  "today": {
    "title": "現況",
    "loading": "正在讀取狀態…",
    "noProvider": "還沒有供應商",
    "addProvider": "加入供應商",
    "providerTokens": "當前 Provider 累計 Token",
    "requests_one": "{{count}} 次請求",
    "requests_other": "{{count}} 次請求",
    "lastActivity": "最後活動",
    "proxy": {
      "start": "啟動 Proxy",
      "stopRestore": "停止 Proxy 並還原 Codex"
    },
    "quota": {
      "tightest": "週額度剩餘最少的供應商",
      "remaining": "額度剩餘",
      "noData": "目前沒有供應商回報額度"
    },
    "context": {
      "nearestThreshold": "最接近壓縮門檻的工作階段",
      "usage": "上下文用量",
      "threshold": "壓縮門檻 {{percent}}%",
      "compactThreshold": "壓縮門檻",
      "averageTurn": "平均每輪",
      "atRate": "以這個速度",
      "trend": "上下文用量走勢"
    },
    "findings": {
      "title": "待處理項目",
      "count_one": "{{count}} 件",
      "count_other": "{{count}} 件",
      "adjustContext": "調整上下文視窗"
    },
    "sessions": {
      "title": "工作階段",
      "count": "{{live}} 個活動中 / 共 {{total}} 個",
      "rest": "其餘的工作階段"
    },
    "providers": {
      "title": "Provider 狀態",
      "noModels": "沒有可用模型",
      "quotaFailed": "額度查詢失敗",
      "notQueried": "還沒查過",
      "lastChecked": "{{since}}查詢",
      "refreshTitle": "重新查詢 {{provider}} 的額度",
      "querying": "查詢中…",
      "refresh": "重新查詢",
      "empty": "沒有要顯示的供應商。到「設定」勾選，或到「模型」加一家。"
    },
    "health": {
      "title": "連線品質",
      "connectionReuse": "連線復用",
      "reusing_one": "復用中 · {{count}} 條連線",
      "reusing_other": "復用中 · {{count}} 條連線",
      "notReusing": "未復用",
      "firstByte": "首字延遲",
      "reasoning": "推理內容",
      "history": "歷史保存",
      "historyValue_one": "{{days}} 天 · 已加密",
      "historyValue_other": "{{days}} 天 · 已加密"
    },
    "headroom": {
      "exceeded": "已超過門檻，下個對話回合將觸發壓縮",
      "noEstimate": "尚無對話回合可供估算",
      "estimate_one": "預估還可進行 {{count}} 個對話回合",
      "estimate_other": "預估還可進行 {{count}} 個對話回合"
    },
    "errors": {
      "partialRefresh": "部分資料更新失敗：{{detail}}",
      "quotaRefresh": "額度查詢失敗：{{detail}}",
      "proxyOperation": "Proxy 操作失敗：{{detail}}"
    }
  },
  "models": {
    "title": "模型",
    "ui": {
      "catalogTitle": "設定 Codex 模型選單", "catalogHint": "先連線帳號或供應商，再挑出要送進 Codex 模型選單的模型。", "wireUnknown": "無法自動判定，請選擇", "customProvider": "自訂供應商", "errors": { "partialRefresh": "部分資料更新失敗：{{detail}}", "noReset": "目前沒有可使用的 Reset。", "confirmReset": "確定要為 {{account}} 使用一個 Reset？此操作會消耗一筆 Reset 額度，且無法復原。", "resetSuccess": "OpenAI 額度已重置。", "resetCompleted": "Reset 已完成（{{code}}）。", "resetFailed": "Reset 失敗：{{detail}}", "resetLookupFailed": "Reset 查詢失敗：{{detail}}", "oauthExpired": "授權碼已過期，請重新登入。", "oauthLoginFailed": "ChatGPT 登入失敗：{{detail}}", "oauthSwitchFailed": "無法切換 ChatGPT 帳號：{{detail}}", "oauthRemoveFailed": "無法移除 ChatGPT 帳號：{{detail}}", "oauthRefreshFailed": "無法更新 ChatGPT 登入：{{detail}}", "oauthLogoutFailed": "無法登出 ChatGPT：{{detail}}", "probeFailed": "無法探測這個端點：{{detail}}", "opencodeApiKeyRequired": "請輸入 OpenCode Zen API key。", "opencodeConnectFailed": "無法連線 OpenCode Zen：{{detail}}", "opencodeFreeAttachFailed": "OpenCode Go 已連線，但連線它的免費模型失敗：{{detail}}", "modelProbeFailed": "無法驗證 {{model}}：{{detail}}", "modelToolProbeFailed": "{{model}} 未回傳 Codex 可用的結構化工具呼叫。", "routeRefreshFailed": "無法更新供應商狀態：{{detail}}", "reprobeFailed": "重新探測能力失敗：{{detail}}", "routeRemoveFailed": "無法刪除供應商：{{detail}}", "providerModelRequired": "每個 Provider 至少要保留一個模型。", "catalogRefreshFailed": "無法更新匯入 Codex 的模型：{{detail}}", "wireRequired": "無法自動判定 API 協定，請選擇 Responses API 或 Chat Completions。", "modelRequired": "探測不到模型名稱，請手動填寫。", "routeAddFailed": "無法加入供應商：{{detail}}", "grokLoginFailed": "Grok 登入失敗：{{detail}}", "grokStartFailed": "無法啟動 Grok 登入：{{detail}}", "grokCancelFailed": "無法取消 Grok 登入：{{detail}}", "grokSwitchFailed": "無法切換 Grok 帳號：{{detail}}", "grokRefreshFailed": "無法更新 Grok 帳號：{{detail}}", "grokRemoveFailed": "無法移除 Grok 帳號：{{detail}}", "detectAttention": "需補充", "detectFact": "已探測" },
      "confirm": {
        "removeAccount": { "title": "移除 ChatGPT 帳號", "confirmLabel": "移除帳號", "factAccount": "帳號", "factEffect": "影響", "effectValue": "Vellum 會忘記這個帳號的登入憑證；要再用它得重新登入。" },
        "logoutAll": { "title": "登出所有 ChatGPT 帳號", "confirmLabel": "全部登出", "factAccount": "帳號", "factEffect": "影響", "accountsValue": "所有已連接的 ChatGPT 帳號", "effectValue": "每個帳號都會從 Vellum 移除，之後都要重新登入。" },
        "removeGrokAccount": { "title": "移除 Grok 帳號", "confirmLabel": "移除帳號", "factAccount": "帳號", "factEffect": "影響", "effectValue": "Vellum 會忘記這個帳號的登入憑證；要再用它得重新登入。" },
        "removeRoute": { "title": "移除供應商", "confirmLabel": "移除供應商", "factProvider": "供應商", "factEffect": "影響", "effectValue": "這個供應商與它整份模型型錄都會被移除；要復原得重新加入一次。" },
        "consumeReset": { "title": "使用一個 Reset", "confirmLabel": "使用 Reset", "factAccount": "帳號", "factEffect": "影響", "effectValue": "這會消耗一筆 Reset 額度，且無法復原。" }
      },
      "chatgpt": { "title": "ChatGPT 帳號", "description": "設定官方模型使用的帳號。已送出的請求使用原帳號；切換成功後建立的新請求使用新帳號。Token 通常會自動更新，不需要重新登入。" },
      "oauth": { "code": "授權碼", "loginPage": "登入頁面", "browserHint": "瀏覽器已開啟，完成授權後這裡會自動更新。", "copied": "授權碼已複製", "copyFailed": "複製失敗，請手動選取授權碼", "copy": "複製授權碼", "waiting": "等待授權…", "waitingBrowser": "等待瀏覽器授權", "login": "登入 ChatGPT", "refreshToken": "更新 Token", "logoutAll": "全部登出" },
      "account": { "active": "使用中", "authenticated": "已登入", "current": "目前帳號", "useThis": "改用這個" },
      "quota": { "failed": "額度查詢失敗", "loading": "額度查詢中", "retry": "重新查詢" }, "reset": { "show": "展開每張 Reset 的到期時間", "hide": "收起每張 Reset 的到期時間", "ledgerTitle": "用量上限重設", "noneUsable": "這個帳號沒有可用的 Reset。", "untitled": "Reset 額度", "spent": "已用掉", "lapsed": "已過期", "noExpiry": "上游沒有給到期時間", "failed": "Reset 查詢失敗", "loading": "Reset 查詢中", "use": "使用重置" },
      "grok": { "title": "Grok 帳號", "description": "Grok Build 的新請求會使用目前帳號。切換立即生效，進行中的請求不會中途更換帳號。", "loginStatus": "登入狀態", "browserHint": "官方 Grok CLI 已啟動登入流程；完成授權後會自動加入帳號。", "loginIncomplete": "官方登入流程尚未完成。", "defaultAccount": "Grok CLI 預設帳號", "externalCli": "外部 CLI", "managed": "Vellum 管理", "reauthenticate": "重新驗證", "refreshModels": "重新探測 Grok 模型", "unlink": "解除連結", "empty": "目前沒有可用的 Grok 帳號。", "add": "新增 Grok 帳號" },
      "opencode": { "title": "OpenCode Zen", "description": "只需填入 API key 即可連線。這跟 OpenCode 官方 App 預設顯示的一樣，只有免費模型。付費 Zen 模型需要這個連線沒辦法驗證的購買額度，如果你確實有，請到下方的手動加入供應商流程另外加。", "descriptionGo": "只需填入 API key 即可連線。OpenCode Go 是另一個端點上獨立的較小模型庫，不含 OpenCode Zen 裡的 GPT／Claude／Gemini 等進階模型；如果你的 key 屬於 Go 訂閱，請選這個。Go 自己的端點不接受免費模型，所以 Vellum 會另外把 OpenCode Zen 的免費模型接成第二個供應商。", "freeRouteName": "{{name}}（免費模型）", "catalog": "模型庫", "catalogZen": "OpenCode Zen（免費模型）", "catalogGo": "OpenCode Go", "catalogHint": "請選擇你實際擁有的方案。Zen 只連線免費模型；Go 連線 Go 方案的模型庫，並自動附帶同樣的免費模型。", "apiKey": "OpenCode Zen API key", "apiKeyPlaceholder": "貼上 OpenCode Zen API key", "connect": "連線", "connecting": "連線中…", "connected": "已連線" },
      "addProvider": { "title": "加入供應商", "steps": { "endpoint": "連接端點", "endpointHint": "取得供應商提供的模型清單。", "probe": "選擇並驗證模型", "probeHint": "先選擇要匯入的模型，再針對該模型驗證 Codex 協定能力。", "add": "加入", "addHint": "確認結果並加入 Codex 模型選單。" }, "endpoint": "端點網址", "apiKey": "API key（選填，加密存放）", "apiKeyPlaceholder": "本機或用帳號登入的端點可留空", "startProbe": "取得模型清單", "probing": "正在取得模型清單…", "add": "加入供應商", "reprobe": "重新設定" },
      "probe": { "reachable": "可以連上", "wire": "API 協定", "modelCount": "探測到 {{count}} 個模型", "modelsMissing": "探測不到，請填寫", "context": "上下文視窗", "required": "需要填寫", "streaming": "串流", "supported": "支援", "unsupported": "不支援", "toolCalling": "Codex 工具協定", "typedToolCalls": "已驗證結構化工具呼叫", "toolCallingUnavailable": "未通過驗證", "chatOnly": "僅支援對話，無結構化工具呼叫", "reasoning": "推理內容", "detected": "可辨識", "notDetected": "未偵測到", "serverResume": "上游記住對話", "remembers": "會記住", "localHistory": "不記住，由 Vellum 補歷史", "detectedModels": "探測到的模型", "selectionHint": "只有已勾選且通過驗證的模型會加入 Codex 模型選單。", "verifyToImport": "勾選以驗證工具呼叫", "verificationRequired": "需要進行能力驗證", "timeout": "探測逾時", "unknown": "未知", "verifyModel": "驗證", "verifyingModel": "驗證中…",  "quotaWithRetry": "額度限制（HTTP 429 · 請於 {{seconds}} 秒後重試）", "unauthorized": "認證失敗（HTTP 401/403）", "protocolError": "協定格式錯誤", "toolCallMissing": "未產生結構化工具呼叫", "unsupportedOpenCodeProtocol": "目前尚未支援 Anthropic／Google 原生協定", "defaultModel": "預設模型", "defaultModelHint": "所有模型都會加入 Codex 選單；這裡只決定供應商建立後預先選用哪一個。", "select": "請選擇", "wireHint": "這是供應商接收請求的 API 協定，不是回應的 JSON 格式。", "manualPlaceholder": "探測不到，請手動填寫", "settingsTitle": "端點探測設定", "requestSize": "請求大小", "preferredWire": "優先 API 格式", "preferredWireValue": "先試 Responses，再退回 Chat Completions", "contextSource": "上下文長度來源", "contextSourceValue": "依序採用手動設定、端點回報、模型快取與 Codex 模型選單。", "history": "對話歷史保存", "historyValue": "當上游不提供工作階段續接時，由本機加密保存對話歷史。" },
      "modelCatalog": { "rename": "改名", "renameProvider": "Provider 顯示名稱", "renameModel": "模型顯示名稱", "renameHint": "只改顯示名稱。送給供應商的仍是上游 id，Codex 既有的路由也不受影響。", "allowPrivateNetworkHttp": "允許明文 HTTP 連到私有網路", "allowPrivateNetworkHttpHint": "給區網或 Tailscale 之類的位址用的：預設拒絕明文 HTTP 連到非本機端點，開啟後才能連到私有網路（LAN、CGNAT、Tailscale）位址；仍然拒絕連到公開網際網路。", "renameModelPlaceholder": "留空就顯示上游 id", "renameSave": "儲存", "renameCancel": "取消", "free": "免費", "deprecated": "已下架", "vision": "圖片", "visionOn": "這個模型可接受圖片輸入", "visionOff": "這個模型只吃文字；勾選後 Codex 才會開放附圖", "visionHint": "圖片支援無法自動探測——純文字端點收到圖片照樣回 200 並讓模型自己編。請依模型實際能力勾選。", "tokenUnit": "token", "effort": "Effort", "responses": "Responses", "chat": "Chat Completions", "title": "模型選單", "hint": "已勾選的模型將加入 Codex 模型選單。", "empty": "還沒有供應商。用上面的流程加入第一家。", "countSelected": "{{selected}} / {{total}} 個", "count": "{{count}} 個", "unknownWindow": "視窗未知", "reasoning": "推理", "noReasoning": "無推理", "auto": "自動", "capabilityMissing": "尚未取得此 Provider 的模型能力資料。", "recent": "最近使用", "reprobe": "重新探測能力", "reprobeInProgress": "正在驗證 {{count}} 個已選模型…", "reprobeSummary": "已驗證 {{succeeded}}/{{targeted}} 個已選模型", "reprobeSummaryWithFailures": "已驗證 {{succeeded}}/{{targeted}} 個已選模型，{{failed}} 個失敗", "effortNotProbed": "尚未探測", "effortUnverified": "無法驗證", "effortReasonIgnored": "Provider 對刻意送出的無效 Effort 值也回傳成功，因此無法證明 low／medium／high 等層級真的生效。重複探測通常不會改變結果。", "effortReasonQuota": "Effort 探測遭 Provider 額度或帳號權限阻擋{{detail}}", "effortReasonProvider": "Effort 探測期間發生 Provider 錯誤、逾時或回應不完整{{detail}}", "effortReasonUnknown": "這次 Effort 探測沒有取得足以驗證支援層級的證據{{detail}}", "retryEffortProbe": "重試 Effort 探測", "footerNote": "啟用與停用會立刻存檔，但要重啟 Proxy 才會生效；模型選單則要重啟 Codex 才會重讀。兩者都還沒完成前，狀態會顯示「待生效」。", "windowEdit": "設定最大上下文長度", "windowHint": "設定這個模型的最大上下文長度", "windowAuto": "自動", "windowManual": "這是你手動填的值；清空後就改回探測到的值。", "windowUnavailable": "這個模型還沒進 Codex 選單，沒有地方可以設。", "windowInvalid": "最大上下文長度要填大於 0 的數字，留空則改回自動。", "windowSaveFailed": "最大上下文長度存不進去：{{detail}}" }
    }
  },
  "context": {
    "awaitingCompaction": "這個對話還沒有發生過壓縮。",
    "transcript": {
      "eyebrow": "壓縮紀錄",
      "title": "壓縮指令與替換內容",
      "hint": "顯示送給模型的壓縮指令，以及模型回傳的替換內容。後續回合將使用該替換內容。",
      "chars": "{{value}} 字",
      "prompt": "送出的指示",
      "promptNote": "壓縮 prompt，就是模型收到的樣子。",
      "result": "寫回來的內容",
      "resultNote": "從這裡開始，這個對話帶著走的東西。",
      "noPrompt": "這次壓縮的指示沒有經過 Vellum。",
      "noResult": "這次壓縮的結果在這裡讀不到。",
      "empty": "還沒有東西可以看 —— 不是這個對話還沒被壓縮過，就是壓縮發生在 Vellum 讀不到的地方。",
      "unavailable": {
        "codexDesktopOpaque": "Codex Desktop 將這次壓縮保存為不透明狀態，因此 Vellum 無法讀取壓縮指令與替換內容。",
        "notCompacted": "這個對話還沒有發生過壓縮。",
        "officialOpaque": "這次壓縮由 OpenAI 官方以不透明狀態管理，因此 Vellum 無法讀取壓縮指令與替換內容。",
        "legacyUnreadable": "這筆舊版壓縮紀錄沒有可讀取的壓縮指令與替換摘要。"
      }
    },
    "sessions": {
      "eyebrow": "Enhanced Codex core",
      "title": "Enhanced core 上的工作階段",
      "hint": "顯示在 Enhanced Codex core 上執行的對話。使用 OpenAI 原生核心的對話由 Codex 內部壓縮，不會顯示在這裡。",
      "listTitle": "工作階段",
      "loading": "正在讀取工作階段…",
      "empty": "現在沒有對話跑在 Enhanced Codex core 上。",
      "untitled": "未命名工作階段",
      "count_one": "{{count}} 個工作階段",
      "count_other": "{{count}} 個工作階段",
      "pick": "顯示 {{label}} 的壓縮情形",
      "headroom": "距離壓縮還有 {{percent}}%",
      "imminent": "已經到門檻",
      "search": "用標題搜尋",
      "searchClear": "清除篩選",
      "countFiltered": "{{total}} 個裡的 {{shown}} 個",
      "noMatch": "沒有符合「{{query}}」的工作階段。"
    },
    "title": "上下文",
    "compactionEyebrow": "上下文壓縮",
    "compactionTitle": "上下文壓縮狀態與預覽",
    "compactionHint": "請在 Codex App 輸入 /compact 執行壓縮；Vellum 顯示壓縮狀態與交接紀錄。",
    "before": "壓縮前",
    "after": "壓縮後",
    "opaque": "Opaque",
    "legendAria": "上下文組成",
    "previewLoading": "正在計算壓縮預覽…",
    "footerNote": "壓縮由執行該對話的 Codex runtime 管理；Vellum 只顯示觀測事件，舊版 journal 僅供唯讀查閱。",
    "segmentAria": {
      "before": "{{label}}，壓縮前 {{tokens}} token",
      "after": "{{label}}，壓縮後 {{tokens}} token"
    },
    "segmentLabel": {
      "canonical": "Canonical 檢查點",
      "reasoning": "Readable Reasoning Replay",
      "tool": "工具延續狀態",
      "retained": "保留的對話脈絡",
      "observedTotal": "Codex 回報的總量"
    },
    "segmentNote": {
      "canonical": "保存目標、限制、決策、進度、檔案與下一步。",
      "reasoning": "以可讀摘要回放必要推理，不傳送第三方私有 ciphertext。",
      "tool": "保留可安全續用的工具結果與呼叫配對狀態。",
      "retained": "保留近期訊息與有效執行脈絡；最多優先保留 {{turns}} 輪。",
      "observedTotal": "這是 Codex 客戶端回報的總 token 變化；它沒有提供各類內容的拆分。",
      "default": "依 Canonical 壓縮策略處理。"
    },
    "readout": {
      "tokenTransition": "{{before}} → {{after}} token", "savedDelta": "省下 {{tokens}} token", "tokenUnit": "token",
      "total": "總計",
      "officialStat": "{{before}} → OpenAI opaque canonical",
      "officialNote": "官方壓縮狀態為加密資料；Vellum 不以 ciphertext 大小推算 token。",
      "saved": "省下 {{saved}} token（{{percent}}%）",
      "hover": "滑過任一段看該段明細",
      "share": "佔壓縮前的 {{percent}}%"
    },
    "summary": {
      "goal": "目標",
      "acceptanceCriteria": "驗收標準",
      "constraints": "限制",
      "userPreferences": "使用者偏好",
      "done": "已完成",
      "inProgress": "進行中",
      "blocked": "卡住",
      "decisions": "決策",
      "changedFiles": "改過的檔案",
      "relevantFiles": "相關檔案",
      "commands": "指令",
      "tests": "測試",
      "unresolved": "未解決",
      "errors": "錯誤",
      "criticalContext": "關鍵背景",
      "references": "參考",
      "nextSteps": "下一步"
    },
    "errors": {
      "partialRefresh": "部分資料更新失敗：{{detail}}"
    }
  },
  "log": {
    "title": "紀錄",
    "activityTitle": "Token 活動",
    "loading": "正在讀取紀錄…",
    "heading": "Token 使用量與請求紀錄",
    "days_one": "{{count}} 天",
    "days_other": "{{count}} 天",
    "boot_one": "已啟動 {{count}} 次 · PID {{pid}} · 上次啟動 {{time}}",
    "boot_other": "已啟動 {{count}} 次 · PID {{pid}} · 上次啟動 {{time}}",
    "bootFirst": "首次啟動 · PID {{pid}}",
    "stats": {
      "total": "累計 Token 數",
      "peak": "Token 峰值",
      "longestRequest": "最長請求時間",
      "currentStreak": "目前連續紀錄",
      "longestStreak": "最長連續紀錄"
    },
    "providers": {
      "title": "各 Provider 累計 Token 數",
      "note": "OpenAI 對齊 Codex 個人檔案；第三方依上游回傳的 input + output 計算",
      "others": "其他 Provider", "fromProfile": "取自 Codex 個人檔案", "segmentAria": "{{provider}}：佔累計 Token 的 {{percent}}，{{value}}", "accountCount_one": "（{{count}} 個帳號）",
      "accountCount_other": "（{{count}} 個帳號）"
    },
    "tokens": "{{value}} Token",
    "requests": {
      "title": "請求明細",
      "note": "只讀最近 {{count}} 筆",
      "empty": "還沒有請求紀錄。Codex 送出第一個請求後就會出現在這裡。",
      "filterEmpty": "目前的篩選條件沒有符合的請求。",
      "filter": { "statusAll": "全部", "statusFailed": "只看失敗", "providerAll": "全部 Provider" },
      "cached": "快取 {{percent}}%",
      "connection": "conn {{id}}",
      "streamQualityTitle": "串流品質：{{quality}}",
      "accountIdentityTitle": "已雜湊的控制帳號 A 與執行帳號 B",
      "streamQuality": {
        "incremental": "增量",
        "end_flush": "結束前一次沖出",
        "buffered": "緩衝",
        "no_delta": "無增量"
      },
      "subagentChildren_one": "{{count}} 個子代理",
      "subagentChildren_other": "{{count}} 個子代理",
      "subagentChildrenTitle_one": "定位 {{count}} 個已派生的子代理執行",
      "subagentChildrenTitle_other": "定位 {{count}} 個已派生的子代理執行"
    },
    "systemEvents": { "title": "系統事件" },
    "compaction": {
      "title": "壓縮",
      "note": "自動壓縮判定，與請求列分開顯示",
      "empty": "還沒有壓縮事件。",
      "tokens": "{{before}} → {{after}}",
      "items": "項目 {{before}} → {{after}}",
      "threshold": "門檻 {{percent}}%",
      "window": "{{active}} / {{window}} context",
      "checkpoint": "checkpoint {{id}}（第 {{generation}} 代）",
      "noCheckpoint": "無持久化 checkpoint（stateless）"
    },
    "subagent": {
      "title": "子代理",
      "note": "每列一個子代理執行 —— 展開查看 requested → child → completed 的時間軸",
      "empty": "還沒有子代理執行紀錄。",
      "summary": { "label": "子代理執行摘要", "total": "共 {{count}} 個", "completed": "完成 {{count}}", "active": "執行中 {{count}}", "attention": "需注意 {{count}}" },
      "call": "call {{id}}",
      "child": "child {{id}}",
      "parent": "父層 {{id}}",
      "locateParent": "定位父層請求（{{id}}）",
      "locateChild": "定位子層請求（{{id}}）",
      "requestedAt": "請求 {{time}}",
      "completedAt": "結束 {{time}}",
      "linkLabel": "關聯：{{value}}",
      "outcome": "結果：{{value}}",
      "error": "錯誤：{{value}}",
      "unknownModel": "未知模型",
      "state": {
        "requested": "已請求",
        "running": "執行中",
        "completed": "已完成",
        "failed": "失敗",
        "cancelled": "已取消",
        "ambiguous": "無法確認",
        "unlinked": "未關聯"
      },
      "link": {
        "exact": "精確關聯",
        "heuristic": "推測關聯",
        "unlinked": "未關聯"
      },
      "timeline": {
        "requested": "已請求",
        "child": "子請求",
        "completed": "已完成",
        "pending": "等待中",
        "noChildLink": "找不到對應子請求"
      }
    },
    "invokes": {
      "title": "桌面呼叫",
      "note": "最近 {{count}} 次 Tauri invoke —— 成功與失敗",
      "empty": "此工作階段尚未記錄桌面呼叫。",
      "ok": "ok",
      "error": "error"
    },
    "errors": {
      "partialRefresh": "部分資料更新失敗：{{detail}}"
    }
  },
  "remote": {
    "title": "遠端主機",
    "blurb": "設定與管理遠端 SSH 主機，使其可直接供 Codex App 使用。聊天、thread 與 session 仍由主機上的 Codex native daemon 負責。",
    "rescan": "重新掃描",
    "rescanning": "掃描中…",
    "discovering": "正在讀取 Codex/OpenSSH 連線設定。介面可繼續操作，SSH 狀態將在背景載入。",
    "featureDisabled": "Native Codex 遠端總管在此版本尚未啟用。",
    "legacyBrokerUnsupported": "發現不再支援的舊 Broker 配對設定，已略過。請改以 SSH 重新發現這台主機。Vellum 不會遷移舊的 Broker 配對。",
    "noHosts": "Codex App 與 OpenSSH 設定中皆未找到可用的連線。",
    "hostsAria": "遠端主機",
    "probing": "探測中…",
    "probeFailed": "探測失敗",
    "notProbed": "尚未探測",
    "updating": "更新中…",
    "lastUpdated": "{{when}} 更新",
    "sshInvalid": "SSH 設定有誤",
    "otherHostBusy": "{{host}} 上仍有操作正在進行。",
    "goToHost": "前往該主機",
    "blockerUnknown": "遇到此版本無法辨識的狀況。",
    "releaseBlocked": "內附的部署包尚未通過簽章驗證，因此安裝 Codex 與更新 Agent 已停用。已部署的主機仍可規劃與同步。",
    "proxyImageUpdate": "此版本內含較新的 Proxy image，請按「重新同步」套用到這台主機。",
    "state": {
      "unreachable": "無法連線",
      "unmanaged": "未部署",
      "readyToPlan": "部署未完成",
      "drifted": "設定有落差",
      "nativeActive": "使用中",
      "detachedReady": "可脫離續跑"
    },
    "verdict": {
      "unreachable": "無法連線此主機上的 Vellum Agent。可能尚未安裝，或 SSH 連線中斷。",
      "unmanaged": "此主機尚未由 Vellum 管理。部署後 Codex App 即可直接使用。",
      "readyToPlan": "Proxy 已啟動，但尚未接管 Codex 的模型選單。",
      "drifted": "主機上的設定與 Vellum 的記錄不一致，需要重新同步。",
      "nativeActive": "主機已就緒。Codex App 目前使用此主機的 native daemon。",
      "detachedReady": "主機已就緒，且關閉 Vellum 後 turn 仍會繼續執行。"
    },
    "summary": {
      "threads_one": "{{count}} 個 thread 進行中",
      "threads_other": "{{count}} 個 thread 進行中"
    },
    "act": {
      "bootstrap": "部署此主機",
      "reconverge": "重新同步",
      "plan": "變更模型…",
      "replan": "更新預覽",
      "apply": "確認並同步",
      "restartNative": "重啟遠端 Codex daemon",
      "takeoverNative": "接管遠端 Codex daemon",
      "installCodex": "安裝 pinned Codex CLI",
      "updateAgent": "更新遠端 Agent",
      "syncDesktopCodex": "同步 Desktop Codex runtime",
      "repair": "修復 codex launcher",
      "bundle": "匯出診斷包",
      "restore": "解除 Vellum 管理…",
      "retry": "重試",
      "stopAppOwned": "安全停止並重試",
      "grokLogin": "登入 Grok 帳號",
      "grokCancel": "取消 Grok 登入",
      "grokRefresh": "更新 Grok 憑證",
      "chatgptPair": "配對桌面端選定的 ChatGPT 帳號",
      "chatgptActivate": "啟用桌面端選定的 ChatGPT 帳號",
      "chatgptPairRow": "配對", "chatgptActivateRow": "啟用", "chatgptPairAll": "配對桌面端全部 ChatGPT 帳號",
      "chatgptPairSkip": "跳過這個帳號",
      "executionLogin": "新增 Official 執行帳號",
      "executionSelect": "用於模型請求",
      "executionRemove": "移除執行帳號",
      "devicePair": "配對這支手機"
    },
    "trust": {
      "title": "確認這台主機的 SSH 金鑰",
      "explain": "這是 trust-on-first-use：Vellum 還沒見過這台主機的 SSH 金鑰。確認前請透過另一個管道核對下面的指紋。",
      "factHost": "SSH 主機",
      "factFingerprint": "指紋",
      "factCrossCheck": "核對管道",
      "crossCheckHint": "一個你已經信任的管道——主機自己的主控台、Tailscale 或 VPN 管理頁等。",
      "confirm": "信任並繼續",
      "checking": "正在確認這台主機是否已被信任…",
      "fetchFailed": "無法讀取這台主機的 SSH 金鑰。",
      "untrustedError": "這台主機的 SSH 金鑰尚未確認。"
    },
    "chore": {
      "restartNative": "套用新的設定與模型選單。Proxy 不會停止，但進行中的 turn 可能被中斷。",
      "takeoverNative": "Codex App 目前直接持有此主機的 daemon。接管後將改用 Vellum 管理的 native daemon，進行中的工作可能被中斷。",
      "installCodex": "在主機上安裝內附的 pinned Codex CLI。驗證 manifest 與 digest 後才會進行原子替換。",
      "updateAgent": "只更新主機上的 vellum-remote-agent。Broker、Proxy image 與 Codex CLI 不會在這一步更新。",
      "syncDesktopCodex": "安裝與此 Desktop core 完全一致的官方 Linux Codex build，完成協議驗證後重啟遠端 daemon。Desktop {{desktop}}；遠端 {{remote}}。",
      "repair": "重建 codex 指令的 launcher，使 Codex App 能找到受管理的 CLI。",
      "bundle": "匯出 Agent、Docker、Proxy、daemon 與操作日誌，敏感值會遮蔽。回報問題時請一併提供。"
    },
    "fact": {
      "changes": "會改",
      "untouched": "不動",
      "turns": "進行中的 turn"
    },
    "confirm": {
      "gate": "請輸入主機名稱以繼續：{{host}}",
      "bootstrap": {
        "changes": "Codex 設定檔、模型選單、Proxy 與所需憑證。第一次執行會套用所有已驗證的模型，之後沿用已儲存的選擇。",
        "untouched": "主機上的專案、一般 Codex thread，以及使用者自行安裝的軟體。",
        "turns": "會被拒絕，不會強制中斷。"
      },
      "restore": {
        "changes": "還原 Codex 原本的設定檔與模型選單、交還帳號 lease、停止 Vellum 管理的 Proxy 與 runtime。",
        "untouched": "你的專案、一般 Codex thread、主機上的 Codex 與 Agent。",
        "turns": "會被拒絕，不會強制中斷。"
      },
      "restartNative": {
        "changes": "重新啟動主機上的 Codex native daemon，以套用新的設定與模型選單。",
        "untouched": "Proxy 保持執行，設定檔與模型選單都不會被改寫。",
        "turns": "進行中的 turn 會被中斷。"
      },
      "takeoverNative": {
        "changes": "將 Codex App 直接持有的 app-server 更換為 Vellum 管理的 native daemon。",
        "untouched": "設定檔、模型選單與已登入的帳號。",
        "turns": "Codex App 端進行中的工作可能被中斷。"
      },
      "installCodex": {
        "changes": "在主機上安裝內附的 pinned Codex CLI，驗證 manifest 與 digest 後進行原子替換。",
        "untouched": "Codex 的設定檔、模型選單與既有的 thread。",
        "turns": "不會停止進行中的 turn；新版本會在 daemon 重啟後生效。"
      },
      "updateAgent": {
        "changes": "把 vellum-remote-agent 更新到目前 Vellum 內嵌的版本，並驗證 digest。",
        "untouched": "Broker、Proxy image 與 Codex CLI 都不在這一步更新。",
        "turns": "Agent 自更新需要重啟 agent，遠端操作會短暫中斷。"
      },
      "syncDesktopCodex": {
        "changes": "下載與 Desktop core 完全一致的 OpenAI 官方 Linux artifact、驗證發布者 digest、探測 app-server 協議、原子安裝並重啟遠端 daemon。",
        "untouched": "Vellum Proxy image、模型選單、帳號、專案與既有 task。",
        "turns": "遠端 daemon 重啟時，進行中的 turn 可能中斷。"
      },
      "stopAppOwned": {
        "changes": "安全停止 Codex App 直接持有的 app-server，然後重新執行部署。",
        "untouched": "設定檔、模型選單與已登入的帳號。",
        "turns": "若仍有 turn 正在執行，Vellum 會拒絕停止，不會強制中斷。"
      }
    },
    "blocker": {
      "agentUnavailable": "無法連線主機上的 Vellum Agent。",
      "codexRestartRequired": "設定已寫入，但需重啟後才會生效。",
      "credentialsMissing": "主機缺少本次部署所需的憑證。",
      "desktopOfficialAccountMissing": "桌面端尚未選定 ChatGPT 帳號，無法與遠端配對。",
      "dockerUnavailable": "主機上沒有可用的 Docker，Proxy 無法啟動。",
      "intelMacUnsupported": "Intel Mac 尚未支援。第一版 Remote Manager 只支援 Apple Silicon。",
      "guiSessionUnavailable": "沒有可用的 macOS 登入工作階段。Proxy 是登入後常駐，不會改電源或自動登入。",
      "proxyPortConflict": "遠端 Proxy 預設埠被占用。請在部署計畫指定其他埠，或釋放 127.0.0.1:15722。",
      "insufficientDiskSpace": "可用空間不足以完成下載、解壓與回滾保留，已在替換前停止。不會自動刪除使用者資料。",
      "incompleteObservation": "無法完整觀測受管 runtime 的 turn、工具或核准，已阻擋破壞性操作。",
      "hostNotConfigured": "此主機尚未寫入 Vellum 的設定。",
      "injectionRequiresReadyProxy": "需待 Proxy 就緒後，才能將模型注入 Codex 的選單。",
      "invalidCompactionThreshold": "壓縮門檻的設定值不在合法範圍。",
      "managedRuntimeRecoveryRequired": "受管理的 runtime 處於異常狀態，需先修復才能繼續。",
      "nativeCodexVersionMismatch": "主機上的 Codex CLI 版本不符合，需要安裝 pinned 版本。",
      "nativeDaemonAppOwned": "Codex App 目前直接持有此主機的 app-server，Vellum 無法接管。",
      "noModelsSelected": "尚未選取任何模型，沒有可同步的內容。",
      "officialAccountActivationRequired": "遠端的 ChatGPT 帳號已配對，但尚未啟用。",
      "officialAccountPairingRequired": "遠端尚未與桌面端選定的 ChatGPT 帳號配對。",
      "proxyConfigurationMissing": "主機上尚無 Proxy 設定。",
      "proxyConfigurationSchemaTooNew": "主機上的 Proxy 設定格式比此 Vellum 版本能理解的更新，為避免覆寫已封鎖部署——請先更新 Vellum。",
      "proxyConfigurationUnreadable": "無法讀取主機上的 Proxy 設定（權限或 I/O 問題），為避免誤判為可安全覆寫已封鎖部署。",
      "systemdUserUnavailable": "主機上沒有可用的 user systemd，daemon 無法常駐。",
      "versionMismatch": "主機上的 Agent 版本與此 Vellum 不相符。"
    },
    "configuration": {
      "upgradeRequired": "遠端 Proxy 設定需要安全升級，重新部署即可自動修復。",
      "repairRequired": "既有 Proxy 設定無效，重新部署將會重建。",
      "incompatible": "主機上的 Proxy 設定格式比此 Vellum 版本能理解的更新，請先更新 Vellum 再部署到這台主機。",
      "unreadable": "無法讀取主機上的 Proxy 設定（權限或 I/O 問題），需先在主機上排除才能部署。"
    },
    "phase": {
      "queued": "排入佇列",
      "cleanHostPreflight": "檢查內附套件與乾淨主機基準",
      "hostPreflight": "檢查 SSH、Docker 與使用者服務",
      "resolvingDesktopCodex": "解析並驗證與 Desktop 相符的 Codex runtime",
      "installCodex": "安裝內附的 pinned Codex CLI",
      "nativeDaemon": "開啟可常駐的 Codex 遠端控制",
      "deploymentPlan": "盤點合格的模型與憑證",
      "deploymentApply": "安裝 Proxy、憑證與模型選單",
      "applying": "套用部署計畫",
      "verification": "驗證 Proxy、daemon 與脫離就緒",
      "verified": "已就緒",
      "restorePreflight": "檢查進行中的 turn 與受管理狀態",
      "restoreLease": "還原 Codex 設定與模型選單",
      "restartNative": "用還原後的設定重啟 native Codex",
      "stopProxy": "停止 Vellum 管理的 Proxy",
      "restoreVerification": "確認 Vellum 已經退出資料路徑",
      "restored": "已解除管理",
      "failed": "失敗"
    },
    "operation": {
      "bootstrap": "正在部署",
      "apply": "正在同步",
      "restore": "正在解除管理",
      "desktopCodexSync": "正在同步 Desktop Codex runtime",
      "elapsed": "已經過 {{clock}}"
    },
    "error": {
      "discovery": "讀取連線設定失敗。",
      "desktopCodexMismatch": "遠端 Codex runtime 與目前 Desktop 協議不相容。請先同步 Desktop Codex runtime，再重新操作。",
      "boundaryKeyProvisionFailed": "無法修復遠端 Proxy 驗證金鑰。請重試；若持續失敗，請匯出診斷包協助排查。",
      "actionFailed": "「{{action}}」執行失敗。上方主機狀態已重新讀取，顯示目前狀態。",
      "operationFailed": "{{action}} 已停止於「{{phase}}」階段。"
    },
    "detail": {
      "title": "主機細節",
      "host": "主機",
      "runtime": "執行環境",
      "version": "版本",
      "sshResolved": "已解析",
      "sshUnresolved": "無法解析",
      "agentAbsent": "未安裝",
      "cores_one": "{{count}} 核",
      "cores_other": "{{count}} 核",
      "diskFree": "可用 {{free}} / 共 {{total}}",
      "dockerAbsent": "未安裝",
      "proxyReady": "就緒 · {{image}}",
      "proxyNotReady": "執行中，尚未就緒",
      "proxyStopped": "未啟動",
      "config": "設定",
      "launcherLoginShell": "登入 shell",
      "launcherBroken": "不可用，請執行修復",
      "daemonRunning": "執行中 · PID {{pid}}",
      "daemonStopped": "未啟動",
      "daemonOwner": "Daemon 所有者",
      "durable": "可常駐",
      "notDurable": "不可常駐",
      "codexSource": "Codex 來源",
      "releaseTrust": "部署包信任",
      "releaseUnverified": "未通過驗證 · {{trust}}",
      "verified": "已驗證",
      "pinned": "pinned 版本",
      "inventoryBlockers": "盤點阻礙項",
      "platform": "平台",
      "proxyBackend": "Proxy 後端",
      "persistence": "常駐範圍",
      "loginResident": "登入後常駐",
      "lingerResident": "systemd linger 常駐",
      "managedHome": "受管 CODEX_HOME",
      "isolationLabel": "隔離",
      "isolation": "遠端設定與本機 Vellum／Enhanced／~/.codex 隔離。"
    },
    "desktopCodex": {
      "desktop": "Desktop Codex",
      "remote": "遠端 Codex",
      "status": "協議相容狀態",
      "states": {
        "current": "已通過此 Desktop 的驗證",
        "updateAvailable": "需要更新遠端 runtime",
        "qualificationRequired": "需要重新驗證協議",
        "agentUpdateRequired": "需先更新遠端 Agent",
        "desktopUnavailable": "無法取得 Desktop core",
        "unavailable": "無法檢查相容性"
      }
    },
    "account": {
      "title": "帳號",
      "chatgptUnavailable": "無法讀取",
      "chatgpt": {
        "synchronized": "已同步",
        "pairingRequired": "需要配對",
        "pairingPending": "配對中",
        "activationRequired": "需要啟用",
        "desktopAccountUnavailable": "桌面端未選定帳號"
      },
      "grokReady": "已登入 {{account}}，refresh timer 執行中",
      "grokPartial": "已安裝憑證，但遠端 CLI 或 refresh token 尚未符合資格",
      "grokAbsent": "未設定",
      "pairingHint": "請使用桌面端選定的帳號登入：",
      "pairingRemaining": "還剩 {{remaining}} 個",
      "pairingActive": "這台主機正在使用",
      "pairingPaired": "已配對，未使用",
      "pairingMissing": "這台主機尚未配對",
      "pairingUnknown": "讀不到",
      "desktopDefault": "桌面端預設",
      "grokDeviceLogin": "Grok 裝置登入：",
      "grokWaitingUrl": "等待登入網址…",
      "grokStarting": "正在等待 Grok CLI…"
    },
    "control": {
      "title": "Remote 控制",
      "identity": "控制帳號 A",
      "sameAccountHint": "Desktop 與手機必須使用相同的 ChatGPT 帳號及 workspace；裝置配對不會改變遠端 daemon 身份。",
      "deviceHint": "短效裝置配對資訊：",
      "expiresAt": "{{when}} 到期"
    },
    "execution": {
      "title": "Proxy 執行帳號",
      "independentHint": "Official 執行帳號 B 只用於模型請求；切換它不會改變或中斷 Remote 控制。",
      "empty": "尚未加入由 Vellum 管理的 Official 執行帳號。",
      "selected": "使用中",
      "select": "選用",
      "remove": "移除",
      "namePlaceholder": "這個 Official 帳號的顯示名稱",
      "loginHint": "完成 Official 裝置登入："
    },
    "maintenance": {
      "title": "維護"
    },
    "danger": {
      "body": "撤銷 Vellum 對此主機的管理，將 Codex 還原至部署前的狀態。此操作並非僅中斷連線——再次使用需重新部署。你的專案與一般 Codex thread 不會被刪除。"
    },
    "sessions": {
      "cap": "原生 session 觀測",
      "title": "Codex daemon 的 thread",
      "unknown": "尚未取得 native session 狀態。",
      "empty": "native daemon 目前沒有可見的 thread。",
      "unreadable": "目前無法讀取 native app-server 的 thread 清單。",
      "turns_one": "{{count}} 個 turn · 最後一個 {{last}}",
      "turns_other": "{{count}} 個 turn · 最後一個 {{last}}",
      "observability": {
        "nativeAppServer": "可觀測",
        "unsupported": "不支援",
        "daemonDown": "daemon 未啟動"
      }
    },
    "plan": {
      "cap": "部署計畫",
      "title": "模型與 policy 同步",
      "ready": "可同步",
      "blocked": "受阻",
      "configHash": "設定雜湊",
      "catalogHash": "選單雜湊",
      "reviewPolicy": "自動審查",
      "reviewPolicyPending": "設定已變更 — 待重新套用",
      "credentials": "憑證",
      "noCredentials": "不需要",
      "revision": "版次",
      "revisionValue": "目標 {{desired}} / 現況 {{observed}}",
      "planHash": "計畫雜湊",
      "rollback": "回復方式",
      "managed": "受管理欄位",
      "changed": "有差異",
      "same": "相同"
    }
  },
  "onboarding": {
    "title": "開始使用 Vellum",
    "acts": { "what": "認識", "connect": "連線", "features": "功能", "launch": "啟動" },
    "folio": { "1": "一", "2": "二", "3": "三", "4": "四" },
    "ui": { "providerSeparator": "、", "back": "回上一幕", "skipHint": "一家都沒接也可以先過，之後在「模型」頁加。", "skip": "先跳過", "continue": "繼續", "enter": "進入 Vellum", "enterWithoutProxy": "先不啟動，直接進去", "start": "開始", "skipSetup": "我設定過了，直接進去", "unwritten": "第 {{step}} 幕，尚未進行", "visited": "（已讀過，可回去）", "login": "登入", "collapse": "收起", "fillEndpoint": "填端點", "connected": "已接上", "unavailable": "不可用", "waitingAuth": "等待授權", "notConnected": "未連線", "waitingAuthEllipsis": "等待授權…", "addAnother": "再接一個", "enterCode": "在瀏覽器輸入這組代碼", "providerType": "供應商類型", "customEndpoint": "自訂 API 端點", "opencodeHint": "OpenCode Zen 使用官方固定端點。只需輸入 API key，Vellum 會取得並驗證相容模型。", "opencodeApiKey": "OpenCode Zen API key", "opencodeApiKeyPlaceholder": "貼上 OpenCode Zen API key", "displayName": "顯示名稱", "displayNamePlaceholder": "例如 自架 vLLM", "endpoint": "端點", "optionalPlaceholder": "不需要就留空", "endpointHint": "留空代表這個端點不驗證。", "probing": "探測中…", "probeAndAdd": "探測並加入", "notConnectedYet": "尚未接上" },
    "overture": { "lead": "犢皮紙。", "history": "中世紀的犢皮紙很昂貴，寫滿後會刮掉重寫，而刮不乾淨的舊字跡仍會從底下透出來。", "mission": "這個程式做的是同一件事：上下文滿了不截斷，重寫一次，該留的留著。", "subtitle": "一個只跑在你機器上的 Codex 代理 · 接下來四幕，都寫在這一張紙上" },
    "what": { "axisTargets": "ChatGPT ／ Grok ／ 自訂", "title": "它站在中間", "lead": "Codex 照常打原本的端點。Vellum 在中間把請求轉給你選的供應商——換供應商、換模型、調上下文，都不必修改 Codex 設定檔。", "axisAria": "Codex CLI 經由本機 Vellum Proxy 連到 ChatGPT、Grok 或自訂端點", "client": "你在用的", "local": "本機 · 不外送", "answering": "實際回答的", "noConfigTitle": "不動設定檔", "noConfigBody": "啟動時接管 Codex 端點，停止時原樣還原。不必手改 config，也不會留下改到一半的狀態。", "noTruncateTitle": "不硬截上下文", "noTruncateBody": "接近門檻時先把對話收斂成帶有目標、已完成與下一步的檢查點，再往下接，而不是砍掉前文。", "reviewTitle": "核准可以換人判斷", "reviewBody": "原本要你按 y/n 的操作，可以指定由另一個模型評估。沙箱、網路與檔案權限完全不變。", "privacy": "全部在這台機器上。Vellum 沒有伺服器，你的對話不會經過我們；憑證加密後存在本機。" },
    "connect": { "title": "接上一家", "lead": "一家就夠開始。之後可以再加，也可以同時開著好幾家——不同工作階段走不同供應商，額度快用完的那家會先被看見。", "chatgptClaim": "用你原本的訂閱額度", "chatgptDetail": "走官方裝置碼登入，不需要 API key。訂閱內的 Reset 額度也會帶進來，之後可在「模型」頁使用。", "grokClaim": "走官方 CLI 的登入流程", "grokDetail": "需要這台機器已裝好 Grok CLI。登入後可以掛多個帳號，額度分別計算。", "grokUnavailable": "尚未安裝 Grok CLI，或目前無法偵測。", "customName": "自訂端點", "customClaim": "任何 OpenAI 相容的服務", "customDetail": "填端點與 API key，Vellum 會自動探測可用模型與上下文長度。自架、代理、第三方服務都走這條。", "codexToolProtocolUnavailable": "端點可以連線，但沒有模型回傳 Codex 可執行的結構化工具呼叫。請先在 vLLM、Ollama 或 llama.cpp 啟用工具呼叫協定。", "credentialNote": "憑證用系統金鑰鏈加密後存在本機，不會寫進 Codex 設定檔，也不會出現在紀錄裡。" },
    "features": { "title": "這些已經開著", "lead": "不需要額外設定就會生效。這一幕只讓你知道它們在哪裡，以及之後要去哪一頁修改。", "reviewTerm": "自動審查\n可以換模型", "settingsPage": "設定", "reviewBody": "Codex 要求核准的操作交給指定 Provider 與模型評估，也可以設定備援。主力額度用完時會標示備援中，不會安靜換人。", "resetTerm": "Reset 額度\n直接在這裡用", "modelsPage": "模型", "resetBody": "ChatGPT 帳號的 Reset 額度直接寫在帳號那一列，按下去消耗一筆，用完立刻重新查。", "sessionTerm": "每個視窗\n各算各的", "todayPage": "現況", "sessionBody": "同時開好幾個 Codex 視窗時，每個視窗有自己的對話與上下文視窗。現況頁把最先撞到門檻的那一個放主位，不用平均成一個數字。" },
    "launch": { "title": "把 Codex 接過來", "runningTitle": "已經走 Vellum 了", "lead": "啟動後 Vellum 會接管 Codex 端點設定，停止時原樣還原。這個動作可以隨時撤回。", "runningLead": "端點已經接管，請求會轉給你接上的供應商。停止時會回到安裝 Vellum 之前的設定。", "startProxy": "啟動 Proxy", "noProviderHint": "一家供應商都還沒接上——可以先啟動，但要等接上後才會有模型回答。", "connectedProviders": "接上的供應商", "howToStart": "怎麼開始用", "howToStartValue": "開一個新的 Codex 視窗。已經開著的要重啟才會走 Vellum。", "howToStop": "怎麼停", "howToStopValue": "在「現況」頁按「停止 Proxy 並還原 Codex」", "reopenGuide": "重看這份導覽", "reopenGuideValue": "「設定」頁" }
  }
};

export default zhTW;
