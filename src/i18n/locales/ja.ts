import type { ResourceTree } from "../resources";

const ja: ResourceTree = {
  "enhanced": {"heading":"Enhanced ランタイムの状態","lead":"各ルートの実行先、互換性検査の結果、モバイル接続の状態を表示します。","title":"Enhanced Core","subtitle":"実行先、互換性の証拠、モバイル接続","serving":"Enhanced Core は稼働中です","notServing":"Enhanced Core は稼働していません","sessions":"確認できたセッション","running":"実行中のターン","freshness":"データの時刻","restartRequired":"新しい起動構成が準備できています。実行中のターンが終わってから再起動してください。","launchCoreDrift":"Codex Desktop がコアを更新しました。ターンがアイドルになると Vellum が Proxy を自動再起動します。その後 Codex を再起動してください。","now": "現在", "runningTurns_one": "{{count}} 件のターンが実行中", "runningTurns_other": "{{count}} 件のターンが実行中", "sessionCount_one": "{{count}} 件のセッション", "sessionCount_other": "{{count}} 件のセッション", "verdict": {"serving": "Codex Desktop は Enhanced Core を経由しています。", "servingOtherLaunch": "Enhanced は稼働していますが、前回の起動構成のままです。Codex を再起動すると現在の構成に移ります。", "stoppedAt": "起動シーケンスが「{{link}}」ステージで停止しています。", "disabled": "Enhanced Core は無効です。Codex Desktop は自身のコアで動いています。"}, "chain": {"title": "起動シーケンス", "artifact": "コンポーネント検証", "armed": "パスの引き継ぎ", "adopted": "Bridge の引き受け", "inPlace": "実行確認"}, "mark": {"blocked": "失敗", "waiting": "待機中", "inPlace": "有効"}, "whyStopped": "「{{link}}」ステージで停止した理由", "whyNoted": "実行中、ただし注意点あり", "protocol": {"verified": "検証済み", "unverified": "未検証", "incompatible": "非互換"}, "protocolUnrun": "比較が実行されませんでした", "unverifiedNote": "この Codex Desktop は検証されていません。ルーティング対象のメソッドはすべて通りますが、差分は実在し未検証です —— 使えますが正しさは保証されません。", "missingHelpers": "公式インストールにあって Enhanced コアにないヘルパー：{{list}}。チャットとルーティングは影響を受けませんが、サンドボックス実行は失敗します。", "routed": "ルーティング対象", "delta": {"methodUnservable": "このメソッドは提供できません", "methodUnknownToDesktop": "Desktop がこのメソッドを知りません", "fieldNewlyRequired": "必須になったフィールドがあります", "fieldRemoved": "フィールドが削除されました"}, "environment": {"leased": "保持中", "released": "返却済み", "orphanedBridge": "取り残された bridge", "foreignValue": "他のツールが保持", "unreadable": "読み取れません"}, "bridgeProcess": "Bridge プロセス", "bridgeState": "Bridge の状態", "children": "子プロセス", "launch": "起動", "relayNote": "スマートフォンの接続は、ハンドシェイク・タスク一覧・メッセージ読み込み・ライブイベント・タスク制御の 5 段すべてを通過して初めて成立します。通らなかった段が接続できない理由です。", "staleWarning":"表示しているのは最後に確認した時点の情報で、古くなっている可能性があります。","compatibility":"バージョンと互換性","activeRuntime":"稼働中の runtime","candidateRuntime":"次回起動の runtime","structure":"Schema 比較","qualification":"直近の検証","notObserved":"未確認","observed":"確認済み","evidenceNote":"Schema の一致は構造上の証拠です。Desktop やモバイルの実機検証を意味しません。","differences":"差分と阻害要因","relay":"Relay 状態","clients":"接続中クライアントのバージョン","handshake":"ハンドシェイク","list":"タスク一覧","history":"メッセージ読み込み","stream":"ライブイベント","control":"タスク操作","failureStage":"直近の失敗段階","plane":"実行先","all":"すべて","empty":"今回の起動ではまだセッションを確認できていません。過去の紐付けは現在の接続ではありません。","session":"セッション","state":"状態","lastActivity":"最終アクティビティ","parent":"親タスク","diagnostics":"診断","recheck":"互換性を再確認","export":"診断をエクスポート","exported":"保存しました","states":{"starting":"起動中", "ready":"準備完了", "degraded":"機能低下", "stopped":"停止", "transportReady":"転送準備完了", "unknown":"不明","current":"最新","stale":"古い","offline":"オフライン","unavailable":"取得できません","running":"実行中","idle":"待機中","approval":"承認待ち","unloaded":"アンロード済み","observed":"確認済み","attached":"接続済み","failed":"失敗"}},
  "common": { "expiresAt": "失効：{{month}}月{{day}}日 {{time}}（{{zone}}）", "expiredAlready": "失効済み", "expiresInMinutes": "残り {{value}} 分", "expiresInHours": "残り {{value}} 時間", "expiresInDays": "残り {{value}} 日","emDash": "—", "none": "—", "listSeparator": "、", "itemSeparator": "、", "loading": "読み込み中…", "processing": "処理中…", "refreshing": "更新中…", "refresh": "更新", "reorganize": "整理し直す", "save": "保存", "cancel": "キャンセル", "confirm": "確認", "close": "閉じる", "remove": "削除", "enabled": "有効", "disabled": "無効", "yes": "はい", "no": "いいえ", "on": "オン", "off": "オフ", "unknown": "不明", "provider": "Provider", "model": "モデル", "proxy": "Proxy", "codexCatalog": "Codex モデルカタログ", "justNow": "たった今", "minutesAgo": "{{count}} 分前", "hoursAgo": "{{count}} 時間前", "daysAgo": "{{count}} 日前", "resetAt": "{{month}}月{{day}}日 {{time}} にリセット", "previousPage": "前のページ", "nextPage": "次のページ", "pageOf": "{{page}} / {{total}} ページ", "shortcutTitle": "{{label}}（Ctrl+{{n}}）", "statusUpdateFailed": "状態の更新に失敗しました：{{detail}}", "errorWithDetail": "{{message}}：{{detail}}", "connectionFailed": "接続に失敗しました", "operationFailed": "操作に失敗しました", "loadFailed": "読み込みに失敗しました", "saveFailed": "保存に失敗しました", "visible": "表示", "hidden": "非表示", "totalItems_one": "全 {{count}} 件", "totalItems_other": "全 {{count}} 件", "itemsNeedAttention_one": "件の対応が必要です", "itemsNeedAttention_other": "件の対応が必要です", "notice": {"info": "補足", "warn": "注意", "error": "失敗"}, "dismiss": "閉じる"},
  "navigation": {
    "mainAria": "メインナビゲーション",
    "today": {
      "label": "現況",
      "title": "現況",
      "blurb": "Proxy・Provider・クォータ状態"
    },
    "models": {
      "label": "モデル",
      "title": "モデル",
      "blurb": "Provider と Codex モデルカタログ"
    },
    "context": {
      "label": "コンテキスト",
      "title": "コンテキスト",
      "blurb": "コンテキスト使用量、圧縮プレビュー、原文"
    },
    "enhanced": {
      "label": "Enhanced",
      "title": "Enhanced",
      "blurb": "実行先と互換性の状態"
    },
    "remote": {
      "label": "リモート",
      "title": "リモートマネージャー",
      "blurb": "Codex App SSH ホストとネイティブ daemon"
    },
    "log": {
      "label": "ログ",
      "title": "ログ",
      "blurb": "Token 統計とリクエスト履歴"
    },
    "settings": {
      "label": "設定",
      "title": "設定",
      "blurb": "アプリと Codex 連携設定"
    }
  },
  "status": {
    "live": "有効",
    "pending": "反映待ち",
    "off": "オフ",
    "reading": "読み込み中…",
    "enhancedLoaded": "読み込み済み",
    "enhancedNotLoaded": "未読み込み",
    "enhancedUnverified": "読み込み済み（未検証）",
    "proxyStopped": "Proxy は停止しています",
    "codexNotManaged": "Codex が Vellum を向いていません",
    "lastSuccessfulRequest": "直近の成功リクエスト",
    "defaultRoute": "既定ルート",
    "noProviderYet": "Provider がまだありません",
    "quotaRemaining": "残りクォータ {{percent}}%",
    "restartCodexRequired": "Codex の再起動が必要です",
    "updateAvailable": "更新があります",
    "updateWaitingIdle": "ダウンロード済み、アイドル待ち",
    "updateWaitingRestart": "ダウンロード済み、次回起動時に適用",
    "updateFailed": "更新に失敗、またはロールバック済み",
    "remedy": {
      "startProxy": "Proxy を起動すると反映されます",
      "restartProxy": "Proxy を再起動すると反映されます",
      "restartCodex": "Codex を再起動するとモデルカタログに表示されます"
    }
  },
  "vocabulary": {
    "quotaUnavailable": "このエンドポイントはクォータ照会を提供しません",
    "source": {
      "override": "手動で入力した値",
      "modelCache": "Provider のモデルキャッシュ",
      "catalog": "Codex モデルカタログ",
      "fallback": "フォールバック値"
    }
  },
  "quota": {
    "period": {
      "week": "週次クォータ",
      "month": "月次クォータ",
      "hours_one": "{{count}} 時間クォータ",
      "hours_other": "{{count}} 時間クォータ",
      "days_one": "{{count}} 日間クォータ",
      "days_other": "{{count}} 日間クォータ",
      "unspecified": "使用クォータ"
    },
    "remainingLabel": "{{period}} 残り {{remaining}}%"
  },
  "review": {
    "policy": {
      "always": {
        "label": "指定モデルに固定",
        "blurb": "指定モデルのみ使用します。その Provider のクォータが尽きると自動レビューは停止します。"
      },
      "failover": {
        "label": "指定を優先、失敗時はフォールバック",
        "blurb": "指定モデルを優先し、クォータ切れや接続失敗時はフォールバックモデルへ切り替えてレビューを継続します。"
      }
    }
  },
  "heatmap": {
    "tokens": "{{value}} token",
    "gridAria": "token 使用量ヒートマップ · {{mode}}",
    "mode": {
      "daily": "日次",
      "weekly": "週次",
      "cumulative": "累計"
    },
    "monthLabel": "{{month}}月",
    "aria": "使用量ヒートマップ",
    "tooltip": "{{from}} – {{to}} · {{requests}} リクエスト · {{tokens}} tokens",
    "range": "{{from}} から {{to}}",
    "requestsTokens_one": "{{count}} リクエスト · {{tokens}} tokens",
    "requestsTokens_other": "{{count}} リクエスト · {{tokens}} tokens"
  },
  "runtime": {"notice": {"codexConfigStillPointedAtVellum": "Codex の設定は前回から Vellum を指したままです。Proxy を起動し、Codex App を再起動すると続けて使えます。", "codexRunning": "Codex が実行中です。Proxy とモデルカタログを読み込むには再起動してください", "proxyStoppedCodexRestartRequired": "Proxy を停止し設定を復元しました。古い Proxy／Bridge 接続から離れるには Codex を再起動してください", "codexRestartDetected": "Codex の再起動を検出しました。Proxy とモデルカタログを読み込みました", "routesAndCatalogUpdated": "Provider 設定とモデルカタログを変更しました。Vellum Proxy と Codex の再起動後に反映されます", "catalogRestored": "モデルカタログを復元しました。Codex の再起動が必要です", "catalogUpdated": "モデルカタログを更新しました", "enhancedDesktopRuntimeChanged": "Enhanced Codex Runtime の設定を変更しました。Codex を再起動して適用してください", "enhancedLaunchCoreRepaired": "Codex Desktop がコアを更新したため、Vellum が Proxy を自動再起動して新しい起動設定を準備しました。Codex を再起動してください（以前の起動 {{launchId}}）", "enhancedLaunchCoreRepairFailed": "Codex Desktop の更新後に Vellum が Proxy を自動再起動できませんでした：{{reason}}。Proxy を手動で再起動してから Codex を再起動してください。", "remoteOfficialAccountSwitchNotFollowed": "リモートホスト {{host}} はアカウント切替に追随しませんでした。Remote Manager でペアリングまたは同期してください。理由：{{detail}}", "officialAccountSwitchNativePlane": "Official GPT は未改変の Codex コアで動きます。Enhanced bridge が有効なあいだ、Vellum のアカウント切替は Codex Desktop のログインを置き換えません", "enhancedDesktopBridgeReady": "Codex を再起動し、Enhanced bridge 経由で実行しています（起動 {{launchId}}）", "enhancedDesktopBridgeFailed": "Codex を再起動しましたが、Enhanced bridge が失敗しました：{{reason}}", "enhancedDesktopRuntimeNotArmed": "Proxy は起動しましたが、Enhanced Codex は有効になっていません。サードパーティ Provider のスレッドはネイティブ Codex で実行されます。理由：{{detail}}", "enhancedDesktopBridgeNotObserved": "Codex を開き直しましたが、起動 {{launchId}} に対する Enhanced bridge の報告がありません。Enhanced は実行されていません", "routeHotSwapped": "ルートを即時に切り替えました", "slotSwitched": "{{slot}} を {{model}} に切り替えました", "restartSucceeded": "Codex を安全に再起動しました。新しい runtime 設定を読み込みました", "restartNotDetected": "Codex の起動要求を送信しましたが、5 秒以内に App を検出できませんでした。手動で開いてください", "restartProcessStillRunning": "Codex が完全に終了していません（終了コード {{exitCode}}）。再起動を中止しました", "restartBlockedByActiveRequests_one": "{{count}} 件のリクエストが実行中です", "restartBlockedByActiveRequests_other": "{{count}} 件のリクエストが実行中です", "restartBlockedByCodexTurn_one": "Codex Desktop で {{count}} 件の会話が進行中です。今再起動すると、まだディスクに書き込まれていない内容が失われます", "restartBlockedByCodexTurn_other": "Codex Desktop で {{count}} 件の会話が進行中です。今再起動すると、まだディスクに書き込まれていない内容が失われます", "restartExecutableMissing": "検証可能な Codex App の実行ファイルが見つかりません", "restartExecutableInvalid": "Codex App の実行ファイルが存在しないか、絶対パスではありません", "restartUnavailableInPreview": "ブラウザープレビューでは Codex を再起動しません", "usageOAuthNotConfigured": "Vellum に OpenAI OAuth が設定されていません。OpenAI の集計は Proxy の記録から推定します", "usageProfileUnavailable_one": "{{count}} 件の OpenAI OAuth アカウントの Codex Token 統計を一時的に取得できません", "usageProfileUnavailable_other": "{{count}} 件の OpenAI OAuth アカウントの Codex Token 統計を一時的に取得できません", "webSearchDisabledMissingBraveKey": "Brave Search の API キーが未設定のため、サードパーティのウェブ検索は無効化されました。設定でキーを追加すると再度有効にできます。"}, "alerts": {"label": "ランタイム警告"}},
  "settings": {
    "language": {
      "title": "言語",
      "blurb": "UI 言語を選択します。「システムに従う」は OS 言語を追跡します。",
      "option": {
        "system": "システムに従う",
        "zh-TW": "繁體中文",
        "zh-CN": "简体中文",
        "en": "English",
        "ja": "日本語"
      },
      "applied": "言語を適用しました"
    },
    "title": "設定",
    "page": {
      "subagent": { "title": "サブエージェントの既定値", "description": "既定では、サブエージェントはそれを生成した会話のモデルと推論強度をそのまま使います。ここで固定の既定値に変えられます。タスクでモデルや Effort が明示された場合は、その指定が優先されます。", "desktopReady": "Codex Desktop は準備完了", "desktopUnavailable": "Codex Desktop にこの設定を適用できません", "desktopVersion": "Desktop バージョン {{version}} · ネイティブのサブエージェント既定値を利用可能", "defaultsTitle": "未指定時に使うモデル", "mode": { "inherit": "Desktop の現在の設定を維持", "custom": "Vellum で既定値を指定" }, "effort": "推論強度", "autoEffort": "自動（モデル既定）", "effortNotProbed": "このモデルの推論強度は未検証です。Models ページで先に機能を確認してください。", "modelEmpty": "この Provider には選択できるモデルがありません。", "hint": "これらはデフォルトに過ぎません。エージェントはタスクに応じて他の有効なモデルと Effort を明示的に指定できます。", "unavailable": "選択した Provider またはモデルは現在利用できません。このデフォルトに依存するサブエージェントは、設定が更新されるまで明示的に失敗します。", "unsupported": "Codex Desktop のネイティブサブエージェント設定を確認できません：{{detail}}" },
      "loading": "設定を読み込み中…", "heading": "レビュー、検索、表示、復旧", "unspecified": "未指定", "saved": "保存しました",
      "review": { "title": "自動レビュー", "toggle": "承認が必要な操作をレビュー", "description": "通常 Codex がユーザーに確認する操作を指定モデルでリスク評価します。サンドボックス、ネットワーク、ファイル権限は変更せず、承認判断を行うモデルだけを変更します。", "billingHelp": "クォータはどう消費されますか？", "billingHelpTitle": "自動レビューと ChatGPT クォータ", "billingHelpFacts": { "enabled": { "title": "自動レビューがオン", "body": "Official のレビューには、ここで指定した請求先アカウントが使われます。「使用中のアカウントに従う」を選んだ場合のみ、通常の ChatGPT アカウントルーティングに従います。" }, "disabled": { "title": "自動レビューがオフ", "body": "Vellum はレビュー専用アカウントを指定しません。Official リクエストは通常の ChatGPT アカウントルーティングを使い、通常は Models ページで選択したアカウントに計上されます。" }, "pool": { "title": "クォータプールの例外", "body": "クォータプールが有効でメンバーがいる場合、通常の Official リクエストにはプールが選んだアカウントが使われるため、手動で選択したアカウントに計上されるとは限りません。" }, "native": { "title": "Vellum 管理アカウントなし", "body": "Vellum が ChatGPT アカウントを管理していない場合は、Codex から渡されたネイティブログインを維持します。サードパーティ Provider は各 Provider のクォータを使います。" } }, "strategyTitle": "モデル選択方針", "currentLabel": "現在使用中", "fallbackActive": "フォールバック中", "selectedUnavailable": "指定モデルを利用できません", "provider": "指定 Provider", "model": "指定モデル", "fallbackModel": "フォールバックモデル", "billingAccount": "請求先", "billingFollowsDefault": "使用中のアカウントに従う", "billingAccountMissing": "アカウント {{account}} はこの端末でサインインしていません。自動レビューは別のアカウントに請求せず、失敗として報告します。一覧から選び直すか、そのアカウントで再度サインインしてください。", "savedRemotePending": "保存しました。ローカルの proxy には今すぐ反映されます。リモートホスト {{count}} 台は Remote Manager での再適用が必要です。", "rulesTitle": "自動レビューのルール", "willReview": "レビュー対象", "willReviewValue": "サンドボックス外、制限ネットワーク、保護パスに触れる操作", "willNotReview": "レビュー対象外", "willNotReviewValue": "サンドボックス内ですでに許可されている操作", "whenRejected": "拒否された場合", "whenRejectedValue": "Codex はより安全な方法を選び、直接実行しません" },
      "stats": { "title": "Provider 別レビュー回数", "summary": "フォールバック {{fallback}} / {{total}} 回（{{percent}}%）", "primary": "指定", "fallback": "フォールバック", "failed": "失敗", "lastUsed": "最終使用", "neverUsed": "未使用", "empty": "自動レビューはまだ実行されていません。次の承認要求で回答した Provider が記録されます。" },
      "webSearch": { "title": "ウェブ検索", "toggle": "第三者 Provider のウェブ検索を有効化", "description": "有効にすると、公式以外の Provider が Codex のウェブ検索ツールを使用し、クエリは Brave Search が処理します。無効の場合、Vellum は転送前に検索ツールを削除します。OpenAI 公式 Provider はネイティブ検索を継続して使用し、この設定の影響を受けません。", "braveKey": "Brave Search API キー", "braveKeySaved": "設定済み。新しいキーを入力すると更新します", "braveKeyPlaceholder": "Brave Search API キーを入力", "braveKeyHint": "API キーはこの端末に暗号化して保存され、一般設定ファイルには書き込まれず、モデルにも提供されません。", "reach": "ウェブアクセス範囲", "reachOption": { "indexed": "インデックス結果のみ", "live": "公開ページの取得を許可" }, "reachHint": { "indexed": "モデルは検索バックエンドが返すタイトルと抜粋のみを受け取り、元ページは取得しません。", "live": "モデルは検索結果に含まれる公開 HTTP(S) ページを取得できます。ローカル、プライベートネットワーク、クラウドメタデータのアドレスは遮断されます。" }, "probe": "検索接続テスト", "probeAction": "テストを実行", "probing": "テスト中…", "probeHint": "最小クエリを送信し、Brave Search への接続と応答状態を確認します。", "probeOk": "Brave Search が {{count}} 件の結果を返しました", "probeEmpty": "Brave Search への接続は正常ですが、検索結果は返りませんでした", "probeFailed": "接続テストに失敗しました：{{detail}}", "probedAt": "{{when}}にテスト完了", "needsBraveKey": "ウェブ検索は有効ですが Brave Search API キーが未設定です。キーを追加するまでクエリは失敗します。", "domainTray": "ドメインアクセス規則", "domainAllow": "許可リスト", "domainBlock": "遮断リスト", "domainHint": "1 行に 1 ドメインを入力します。両方のリストが空の場合は制限しません。許可リストを設定すると、一致するドメインのみを検索結果に保持します。" },
      "dashboard": { "title": "ダッシュボードの Provider 表示", "note": "{{count}} 件を選択中。現況ページの表示だけに影響し、Provider の有効化・無効化は行いません。", "disabledVisible": "表示（無効）" },
      "restore": { "title": "Codex の初期設定を復元", "description": "Vellum が Codex に書き込んだ Proxy URL、boundary key、モデルカタログの項目を削除し、元の既定 Provider に戻します。既存の Vellum のチャットをネイティブ Codex で開けるよう、Proxy を介さず OpenAI に直接接続する Provider は残します。ログイン情報、チャット、プロジェクトのグループ、ワークスペースのデータは変更しません。", "action": "Codex の初期設定を復元", "cleared": "復元済み", "result": "結果", "nothingToClear": "Codex に復元対象の Vellum 設定はありません", "preserved": "変更なし：{{items}}。" },
      "enhancedRuntime": {"title": "Enhanced Codex Runtime", "description": "サードパーティ Provider の新しい会話は Enhanced Codex で実行されます。OpenAI 公式モデルはネイティブの Codex Runtime のままです。実行先を切り替えるには新しい会話が必要です。", "desiredState": "要求された状態", "desiredEnabled": "有効化を要求", "desiredDisabled": "無効化を要求", "activation": {"disabled": "無効", "artifactBlocked": "実行ファイルの検証に失敗", "awaitingDesktopRestart": "Codex Desktop の再起動待ち", "active": "有効", "disablePendingRestart": "無効化を要求済み、再起動待ち", "environmentDrift": "引き取り状態が一致しません", "failed": "Bridge の起動に失敗"}, "environment": {"leased": "Vellum がリースして保持中", "released": "引き取っていません", "orphanedBridge": "旧バージョンの Vellum bridge が残っています", "foreignValue": "他のツールが設定した値。Vellum は上書きしていません", "unreadable": "ユーザー環境変数を読み取れません"}, "environmentValue": "現在の CODEX_CLI_PATH", "environmentUnset": "未設定", "staleBridge": "このパスは旧バージョンの Vellum が残した bridge で、このビルドが使うものではありません。「無効化して引き取りを解除」で消してから、もう一度有効化してください。", "launchDetail": "{{launchId}} · {{state}} · bridge {{bridgePid}} · Official {{officialPid}} · Enhanced {{enhancedPid}}", "officialBinary": "公式 Codex core", "officialBinaryHint": "インストール済みの Codex Desktop から検出します。Desktop が実際に実行するものと同一である必要があるため、変更できません。", "enhancedBinary": "Enhanced Codex core", "bridgeBinary": "App Server bridge", "bridgeBinaryHint": "この Vellum ビルドに同梱され、ハッシュが埋め込まれています。差し替えはできません。", "protocol": {"label": "プロトコル検査", "unavailable": "検査できません", "details": "差分と診断（{{count}}）", "routed": "ルーティング", "verdict": {"verified": "固定した版と一致", "unverified": "使用可能だが正確性は保証されない", "incompatible": "非互換、フォールバック済み"}, "mean": {"verified": "この Vellum が固定した組み合わせそのもので、プロトコルはバイト単位で同一です。", "unverified": "この Codex Desktop 版は誰も検証していません。bridge が振り分けに使うメソッドはすべて存在するためルーティングは健全ですが、下記の差分は実在する未テストのものです。", "incompatible": "Codex Desktop が bridge の振り分けに必須のメソッドを変更しました。縮退運用の余地がありません。"}, "delta": {"shapeIncompatible": "{{subject}} の wire 形式を相手側が受け取れません：{{fields}}", "shapeUnverified": "{{subject}} の wire 形式はこの検査では判定できません：{{fields}}", "methodUnservable": "Desktop が {{subject}} を呼ぶ可能性がありますが、Enhanced core は未実装です", "methodUnknownToDesktop": "Enhanced core が {{subject}} を送る可能性がありますが、Desktop はもう宣言していません", "fieldNewlyRequired": "Desktop は {{subject}} の {{fields}} を必須にしましたが、Enhanced core は送出しません", "fieldRemoved": "Desktop は {{subject}} の {{fields}} を送らなくなりましたが、Enhanced core はそれを読みます"}, "fallback": "ネイティブ Codex に戻りました。Codex 自体は正常に動作しますが、Enhanced の機能は一切ありません。"}, "protocolHash": "App Server プロトコル", "observedBridge": "実際に動作中の bridge", "unverified": "未検証", "details": "技術詳細と診断情報", "runInstalledGate": "インストール版の検証をやり直す", "installedGateHint": "1 回の再起動より深い検証です。Codex Desktop を再起動し、インストール版 gate 一式を実行してレポートを残します。", "lastQualification": "前回の検証", "qualificationPassed": "{{mode}} は合格、昇格できます", "qualificationComponentOnly": "{{mode}} はコンポーネント試験に合格しましたが、実機昇格の基準には未達です", "qualificationFailed": "{{mode}} は不合格 —— {{detail}}", "qualificationReport": "レポート", "planeTitle": "各ルートが今どちらで動いているか", "contextNote": "圧縮は会話を実行する runtime 自身の責任です。公式経路は Official Codex のネイティブ圧縮、サードパーティ経路は Enhanced Codex のローカル圧縮とコンテキスト回復を使います。Vellum は独自の圧縮ポリシーを適用しません。", "planeOfficial": "Official Codex · ネイティブ圧縮", "planeEnhanced": "Enhanced Codex · ローカル圧縮とコンテキスト回復", "planeUnbound": "Enhanced Codex に未バインド", "activationLabel": "有効化の状態", "environmentLabel": "引き継ぎの状態", "inject": {"on": "注入済み", "off": "未注入", "working": "注入中", "switchLabel": "Enhanced Codex Runtime を注入する", "mean": {"staleLaunch": "Codex Desktop は現在 Enhanced を経由していますが、以前の起動時の設定で動いています。最新の設定を反映するには Codex Desktop を再起動してください。", "on": "Codex Desktop は Vellum の bridge 上で動いています。サードパーティ Provider の新しいスレッドは Enhanced Codex が実行し、OpenAI 公式モデルはネイティブ Codex のままです。", "off": "Codex Desktop はネイティブ Codex で動いています。CODEX_CLI_PATH に Vellum は触れていません。", "wanted": "Vellum 側の設定は済んでいますが、Codex Desktop がまだ引き受けていないため、動いているのはネイティブ Codex のままです。", "working": "実行ファイルを検証し、CODEX_CLI_PATH を引き継ぎ、Codex Desktop を再起動しています。", "armed": "Codex Desktop を起動し直すと有効になります。"}, "blocked": {"proxyStopped": "Proxy が起動していません。Proxy が動いていない間、Vellum は起動パスを引き継ぎません —— そうしないと Codex Desktop が背後にデータプレーンのない bridge で起動してしまいます。先に Proxy を起動してください。", "stuck": "注入を要求しましたが、起動パスが引き継がれていません。どこで止まったかは下のターミナルにあります。スイッチをもう一度切り替えると再試行します。", "noCore": "この Vellum ビルドに Enhanced Codex core が含まれていません。設定の入れ忘れではなく、core はビルドに同梱されるものなので、無い場合はインストールが不完全です。Vellum を再インストールしてください。"}}, "term": {"idle": "まだ実行していません。上のスイッチを切り替えると、各ステップがここに表示されます。", "verify": "Enhanced Codex 実行ファイルを照合", "verifyOk": "ファイルの中身がこのバージョンの想定どおり", "verifyFailed": "照合に通りませんでした。何も変更していません", "restart": "CODEX_CLI_PATH を引き継ぎ、Codex Desktop を再起動", "restartBack": "Codex Desktop をネイティブ Runtime に戻して再起動", "restartRefused": "再起動しませんでした", "adopt": "Codex Desktop が bridge を引き受けたか確認", "injected": "注入済み · {{profile}}", "notInjected": "Codex Desktop が bridge を引き受けませんでした", "release": "CODEX_CLI_PATH を解放", "releasedOk": "解放しました。Codex Desktop はネイティブ Runtime に戻りました", "envUnset": "（未設定）", "preflight": "前提条件を確認", "proxyStopped": "Proxy が起動していないため、引き継ぎは解放されます。先に Proxy を起動してください。", "armed": "起動パスを引き継ぎました。次に Codex Desktop を起動したときから有効になります。", "alreadyInjected": "Codex Desktop はすでにこの bridge 上で動いています。再起動は不要です · {{profile}}", "missingHelper": "ヘルパー {{name}} がありません"}, "missingHelpers": "公式インストールにあるヘルパー実行ファイルが、この Enhanced core の隣にありません：{{names}}。Codex は実行時に自分の隣を探し、見つからないと Windows の「見つかりません」ダイアログになります。スレッドとルーティングには影響しません。壊れるのはそのヘルパーを使う機能だけです —— サンドボックスの shell コマンドには codex-windows-sandbox-setup.exe と codex-command-runner.exe が、code mode には codex-code-mode-host.exe が必要です。code mode は既定でオフで、オフのままなら影響はありません。", "enhancedBinaryHint": "この Vellum ビルドに同梱され、enhanced-runtime.lock.json のハッシュと 1 バイトずつ照合されます。別のファイルは検証に失敗するだけなので、選択肢としては提供しません。", "coreMissing": "このビルドには含まれていません"},
      "updates": { "title": "ソフトウェア更新", "description": "デスクトップ、Remote パッケージ、Enhanced core は独立して更新できます。ダウンロード済みの更新は、適用条件を満たすまで待機します。", "liveDisabled": "リリース署名が設定されるまで、自動更新は無効です。", "actionFailed": "更新操作に失敗しました：{{error}}", "autoCheck": "更新を自動で確認", "autoDownload": "更新を自動でダウンロード", "channel": "チャンネル", "stable": "安定版", "preview": "プレビュー版", "current": "現在のバージョン", "available": "利用可能なバージョン", "none": "なし", "progress": "ダウンロードの進捗", "applyWhen": "適用条件", "notes": "リリースノート", "failure": "失敗理由", "check": "更新を確認", "download": "ダウンロード", "apply": "適用", "cancel": "ダウンロードを中止", "rollback": "ロールバック", "hosts": "ホスト", "idleAuto": "アイドル時に自動更新", "idleHandoff": "プレビュー：アイドル時に Enhanced core を適用（既定はオフ）", "layerDisabled": "このレイヤーの更新は、現在のビルドでは有効になっていません。", "layer": { "desktop": "Vellum 本体", "remote": "Remote パッケージ", "core": "Enhanced Codex core" }, "condition": { "restartVellum": "準備完了。Vellum を再起動すると適用されます。", "hostIdle": "ダウンロード済み。ホストがアイドルになるのを待っています。", "nextCoreStart": "ダウンロード済み。次回のコア起動時に適用します。", "download": "ダウンロードして待機させます。", "failed": "失敗理由を確認してください。", "idle": "最新です。", "checking": "確認中…", "available": "更新があります。", "downloading": "ダウンロード中…", "verifying": "検証中…", "staged": "ステージ済み。", "waitingForIdle": "アイドル待ち。", "waitingForRestart": "再起動待ち。", "applying": "適用中…", "validating": "検証中…", "applied": "適用済み。", "blocked": "ブロック済み。", "rolledBack": "ロールバック済み。" }, "phase": { "idle": "待機", "checking": "確認中", "available": "利用可能", "downloading": "ダウンロード中", "verifying": "検証中", "staged": "ステージ済み", "waitingForIdle": "アイドル待ち", "waitingForRestart": "次回起動時に適用", "applying": "適用中", "validating": "検証中", "applied": "適用済み", "blocked": "ブロック済み", "failed": "失敗", "rolledBack": "ロールバック済み" } },
      "advanced": { "title": "詳細設定", "description": "リクエストの受付停止、安全な Codex の再起動、モデルカタログのロールバックを、トラブルシューティングと保守向けに提供します。", "activeRequests": "実行中のリクエスト", "drainTitle": "新しいリクエストを受け付けない", "draining": "新しいリクエストを停止し、既存の完了を待機中", "accepting": "通常どおりリクエストを受付中", "drainHint": "実行中のリクエストは中断しません。有効中は手動で再開するまで新規リクエストを拒否します。", "resume": "リクエスト受付を再開", "stop": "新しいリクエストを停止", "catalogVersion": "現在のカタログバージョン", "notCreated": "未作成", "restartTitle": "Codex を再起動", "restartHint": "既存リクエストを安全に完了してから Codex を再起動します。", "restartAction": "Codex を再起動", "restartAnyway": "それでも再起動", "guideTitle": "初期設定ガイド", "guideHint": "現在の設定をリセットせずに、初回ガイドの再確認や Provider の追加を行います。", "guideAction": "設定ガイドを開く", "catalogHistory": "モデルカタログの履歴", "rollback": "復元", "noVersions": "復元可能なバージョンはありません。モデルカタログ更新時に自動保存されます。", "restartRequired": "再起動後に有効", "applied": "適用済み" },
      "logs": { "title": "診断ログ", "description": "Vellum、Proxy、Enhanced Runtime が保持するテキストログを ZIP に書き出します。キー、トークン、ユーザーパスは再度マスクされ、認証情報、設定、チャット履歴、データベースは含まれません。", "export": "全ログを ZIP で書き出す", "exporting": "書き出し中…", "exported": "{{path}} に書き出しました", "hint": "ZIP はダウンロードフォルダーに保存され、ファイル一覧と切り詰め記録が含まれます。" },
      "app": { "title": "アプリケーション操作", "exit": "Vellum を終了", "exitHint": "終了すると Proxy を停止して Codex の元の接続設定を復元します。チャットとプロジェクトには影響しません。" },
      "errors": { "partialRefresh": "一部のデータを更新できませんでした：{{detail}}", "reviewSave": "自動レビュー設定を保存できませんでした：{{detail}}", "reviewNoModelAvailable": "有効なレビューモデルが設定されていません。固定モデルに切り替える前に Provider を追加または有効化してください。", "reviewNoFallbackAvailable": "フォールバックには別の Provider の有効なレビューモデルがもう一つ必要です。先に追加または有効化してください。", "restore": "復元に失敗しました：{{detail}}", "drain": "リクエスト受付状態を更新できませんでした：{{detail}}", "restart": "Codex を再起動できませんでした：{{detail}}", "rollback": "モデルカタログを復元できませんでした：{{detail}}", "webSearchSave": "ウェブ検索設定を保存できませんでした：{{detail}}", "subagentSave": "サブエージェント設定を保存できませんでした：{{detail}}", "logExport": "診断ログを書き出せませんでした：{{detail}}" }
    }
  },
  "today": {
    "title": "現況",
    "loading": "ステータスを読み込み中…",
    "noProvider": "Provider がありません",
    "noSuccessfulRequest": "成功したリクエストはまだありません",
    "addProvider": "Provider を追加",
    "providerTokens": "現在の Provider 累計 Token",
    "requests_one": "{{count}} 件のリクエスト",
    "requests_other": "{{count}} 件のリクエスト",
    "lastActivity": "最終アクティビティ",
    "proxy": { "start": "Proxy を起動", "stopRestore": "Proxy を停止して Codex を復元" },
    "quota": {
      "tightest": "週次残量が最も少ない Provider",
      "remaining": "残量",
      "noData": "残量を報告した Provider はありません"
    },
    "context": {
      "nearestThreshold": "圧縮しきい値に最も近いセッション",
      "usage": "コンテキスト使用量",
      "threshold": "圧縮しきい値 {{percent}}%",
      "compactThreshold": "圧縮しきい値",
      "averageTurn": "1ターンあたりの平均",
      "atRate": "このペースでは",
      "trend": "コンテキスト使用量の推移"
    },
    "findings": {
      "title": "要対応項目",
      "count_one": "{{count}} 件",
      "count_other": "{{count}} 件",
      "adjustContext": "コンテキストウィンドウを調整"
    },
    "sessions": { "title": "セッション", "count": "アクティブ {{live}} / 合計 {{total}}", "rest": "それ以前のセッション" },
    "providers": {
      "title": "Provider ステータス",
      "noModels": "利用可能なモデルがありません",
      "quotaFailed": "残量の取得に失敗しました",
      "notQueried": "未確認",
      "lastChecked": "{{since}}に確認",
      "refreshTitle": "{{provider}} の残量を更新",
      "querying": "確認中…",
      "refresh": "更新",
      "empty": "表示する Provider がありません。設定で選択するか、モデル画面で追加してください。"
    },
    "health": {
      "title": "接続品質",
      "connectionReuse": "接続の再利用",
      "reusing_one": "再利用中 · {{count}} 接続",
      "reusing_other": "再利用中 · {{count}} 接続",
      "notReusing": "再利用なし",
      "firstByte": "最初のトークンまでの時間",
      "reasoning": "推論",
      "history": "履歴保存",
      "historyValue_one": "{{days}} 日 · 暗号化済み",
      "historyValue_other": "{{days}} 日 · 暗号化済み"
    },
    "headroom": {
      "exceeded": "しきい値を超えています。次のターンで圧縮します",
      "noEstimate": "推定に必要なターンがありません",
      "estimate_one": "残り約 {{count}} ターン",
      "estimate_other": "残り約 {{count}} ターン"
    },
    "errors": {
      "partialRefresh": "一部のデータを更新できませんでした：{{detail}}",
      "quotaRefresh": "残量の取得に失敗しました：{{detail}}",
      "proxyOperation": "Proxy 操作に失敗しました：{{detail}}"
    }
  },
  "models": {
    "title": "モデル",
    "ui": {
      "catalogTitle": "Codex モデルカタログを設定", "catalogHint": "アカウントまたは Provider を接続し、そのどのモデルを Codex に出すかを選びます。", "wireUnknown": "自動判定できません。選択してください", "customProvider": "カスタム Provider", "errors": { "partialRefresh": "一部のデータを更新できませんでした：{{detail}}", "noReset": "使用できる Reset クレジットがありません。", "confirmReset": "{{account}} で Reset クレジットを 1 つ使用しますか？この操作は取り消せません。", "resetSuccess": "OpenAI クレジットをリセットしました。", "resetCompleted": "Reset が完了しました（{{code}}）。", "resetFailed": "Reset に失敗しました：{{detail}}", "resetLookupFailed": "Reset の照会に失敗しました：{{detail}}", "oauthExpired": "認証コードの有効期限が切れています。もう一度サインインしてください。", "oauthLoginFailed": "ChatGPT のサインインに失敗しました：{{detail}}", "oauthSwitchFailed": "ChatGPT アカウントを切り替えられませんでした：{{detail}}", "oauthRemoveFailed": "ChatGPT アカウントを削除できませんでした：{{detail}}", "oauthRefreshFailed": "ChatGPT のサインインを更新できませんでした：{{detail}}", "oauthLogoutFailed": "ChatGPT からサインアウトできませんでした：{{detail}}", "probeFailed": "このエンドポイントを検出できませんでした：{{detail}}", "opencodeApiKeyRequired": "OpenCode Zen API key を入力してください。", "opencodeConnectFailed": "OpenCode Zen に接続できませんでした：{{detail}}", "opencodeFreeAttachFailed": "OpenCode Go には接続しましたが、無料モデルの接続に失敗しました：{{detail}}", "modelProbeFailed": "{{model}} を検証できませんでした：{{detail}}", "modelToolProbeFailed": "{{model}} は Codex 用の構造化ツール呼び出しを返しませんでした。", "routeRefreshFailed": "Provider の状態を更新できませんでした：{{detail}}", "reprobeFailed": "能力の再検出に失敗しました：{{detail}}", "routeRemoveFailed": "Provider を削除できませんでした：{{detail}}", "providerModelRequired": "各 Provider には少なくとも 1 つのモデルが必要です。", "catalogRefreshFailed": "Codex モデルカタログを更新できませんでした：{{detail}}", "wireRequired": "API プロトコルを自動判定できません。Responses API または Chat Completions を選択してください。", "modelRequired": "モデル名を検出できませんでした。手動で入力してください。", "routeAddFailed": "Provider を追加できませんでした：{{detail}}", "grokLoginFailed": "Grok のサインインに失敗しました：{{detail}}", "grokStartFailed": "Grok のサインインを開始できませんでした：{{detail}}", "grokCancelFailed": "Grok のサインインをキャンセルできませんでした：{{detail}}", "grokSwitchFailed": "Grok アカウントを切り替えられませんでした：{{detail}}", "grokRefreshFailed": "Grok アカウントを更新できませんでした：{{detail}}", "grokRemoveFailed": "Grok アカウントを削除できませんでした：{{detail}}", "detectAttention": "入力が必要", "detectFact": "検出済み" },
      "confirm": {
        "removeAccount": { "title": "ChatGPT アカウントを削除", "confirmLabel": "アカウントを削除", "factAccount": "アカウント", "factEffect": "影響", "effectValue": "Vellum はこのアカウントのトークンを破棄します。再度使うにはサインインし直してください。" },
        "logoutAll": { "title": "すべての ChatGPT アカウントをサインアウト", "confirmLabel": "すべてサインアウト", "factAccount": "アカウント", "factEffect": "影響", "accountsValue": "接続中のすべての ChatGPT アカウント", "effectValue": "すべてのアカウントが Vellum から削除されます。再度サインインが必要です。" },
        "removeGrokAccount": { "title": "Grok アカウントを削除", "confirmLabel": "アカウントを削除", "factAccount": "アカウント", "factEffect": "影響", "effectValue": "Vellum はこのアカウントのトークンを破棄します。再度使うにはサインインし直してください。" },
        "removeRoute": { "title": "Provider を削除", "confirmLabel": "Provider を削除", "factProvider": "Provider", "factEffect": "影響", "effectValue": "この Provider とモデルカタログ全体が削除されます。復元するには最初から追加し直してください。" },
        "consumeReset": { "title": "Reset を 1 回使用", "confirmLabel": "Reset を使用", "factAccount": "アカウント", "factEffect": "影響", "effectValue": "Reset クレジットを 1 回消費し、元に戻せません。" }
      },
      "chatgpt": { "title": "ChatGPT アカウント", "description": "公式モデルで使用するアカウントを選択します。送信済みのリクエストは元のアカウントを使い、切り替え成功後の新しいリクエストは新しいアカウントを使います。Token は通常自動更新され、再ログインは不要です。" },
      "pool": { "title": "クォータプール", "onHint": "プール内のアカウントが自動で引き受けます。各アカウントの週次枠の一部を予約として残せます。", "offHint": "オフの間は手動選択が続き、新しいリクエストは選択中のアカウントを使います。", "rules": "判定ルール", "rulesTitle": "クォータプールの判定ルール", "rankHint": "数字が順番です。▲▼ で入れ替えられます。", "availableThisWeek": "今週プールに提供できる残量", "availablePercent": "今週は {{value}}% まで利用可能", "currentAndNext": "現在は {{current}} を使用中。次は {{next}} です。", "stalled": "プール内に使えるアカウントがありません。ゲートを越えたり、プール外のアカウントを使ったりはしません。", "empty": "プールは空です。下のアカウントで「プールに追加」を押すとローテーションが始まります。", "using": "使用中", "next": "次", "standby": "待機", "outside": "プール外", "pause": "一時停止", "resume": "再開", "add": "プールに追加", "remove": "プールから削除", "moveUp": "順番を上げる", "moveDown": "順番を下げる", "burnable": "{{value}}% まで使用可", "atGate": "ゲートに到達", "gateRead": "ゲート {{floor}}%", "gateLabel": "{{account}} の週次ゲート", "gateValue": "ゲート {{floor}}%、今週の残り {{left}}%", "saveFailed": "クォータプールを保存できませんでした：{{detail}}", "reason": { "paused": "一時停止中", "weeklyGate": "週次ゲートに到達", "fiveHour": "5 時間枠を使い切り", "missingQuota": "クォータ情報が不完全" }, "rule": { "gate": { "title": "ゲートは下限", "body": "週次ゲートに達した時点で自動利用を止め、ゲートより下の分は予約として残します。100% にするとそのアカウントには一切回しません。" }, "fiveHour": { "title": "5 時間枠は読み取り専用", "body": "これは上流側の制限です。使い切るとそのアカウントは待機し、リセット後に戻ります。" }, "order": { "title": "順番は自分で決める", "body": "数字がローテーションの順番です。▲▼ で調整します。今使えないアカウントは飛ばされ、ほかの順番は変わりません。" }, "pause": { "title": "一時停止と削除", "body": "一時停止は設定を残したまま一時的に外し、削除するとローテーションで使われなくなります。" }, "reset": { "title": "Reset は手動のみ", "body": "プールが Reset を自動で消費することはありません。手動でリセットしてもゲートの値はそのままです。" }, "empty": { "title": "使用できるアカウントなし", "body": "プール内の全アカウントが使えないときは、ゲートを越えたりプール外のアカウントを使ったりせず、明示的に停止します。" } } },
      "oauth": { "code": "認証コード", "loginPage": "ログインページ", "browserHint": "ブラウザを開きました。認証後にこのページが更新されます。", "copied": "認証コードをコピーしました", "copyFailed": "コピーできませんでした。コードを手動で選択してください", "copy": "認証コードをコピー", "waiting": "認証を待機中…", "waitingBrowser": "ブラウザ認証を待機中", "login": "ChatGPT にログイン", "refreshToken": "Token を更新", "logoutAll": "すべてログアウト" },
      "account": { "active": "使用中", "authenticated": "ログイン済み", "current": "現在のアカウント", "useThis": "これを使用" }, "quota": { "failed": "クォータを取得できません", "loading": "クォータを確認中", "retry": "再確認" }, "reset": { "show": "失効日を表示", "hide": "失効日を隠す", "ledgerTitle": "利用上限のリセット", "noneUsable": "このアカウントに使える Reset はありません。", "untitled": "Reset 枠", "spent": "使用済み", "lapsed": "失効", "noExpiry": "失効日は返ってきていません", "failed": "Reset を確認できません", "loading": "Reset を確認中", "use": "リセットを使う" },
      "fiveHour": { "start": "5h を開始", "starting": "開始中…", "hint": "このアカウントでごく小さな Luna リクエストを 1 件送信し、5 時間の利用枠を開始します。少量のクォータを使用しますが、Reset 枠は消費しません。", "success": "5 時間の利用枠を開始し、使用量を更新しました。", "failed": "5 時間の利用枠を開始できませんでした：{{detail}}", "refreshFailed": "開始リクエストは完了しましたが、使用量を更新できませんでした：{{detail}}", "autoOn": "5h 自動：オン", "autoOff": "5h 自動：オフ", "autoHint": "現在のクォータと resetAt に基づいて 5 時間枠を継続します。旧枠が期限切れで週次残量がゲートを上回る場合のみ、Luna と最小 effort で極小リクエストを送信し、判断はエクスポート可能なログに記録します。" },
      "grok": { "title": "Grok アカウント", "description": "Grok Build の新規リクエストは現在のアカウントを使用します。切り替えは即時反映され、実行中のリクエストは元のアカウントを使います。", "loginStatus": "ログイン状態", "browserHint": "公式 Grok CLI のログイン処理を開始しました。認証後にアカウントが追加されます。", "loginIncomplete": "公式ログイン処理が完了しませんでした。", "defaultAccount": "Grok CLI の既定アカウント", "externalCli": "外部 CLI", "managed": "Vellum 管理", "reauthenticate": "再認証", "refreshModels": "Grok モデルを再検出", "unlink": "リンク解除", "empty": "利用可能な Grok アカウントはありません。", "add": "Grok アカウントを追加" },
      "opencode": { "title": "OpenCode Zen", "description": "API key だけで接続できます。これは OpenCode 公式アプリが既定で表示する内容と同じで、無料モデルのみです。有料の Zen モデルにはこの接続では確認できない購入済みクレジットが必要なため、実際にお持ちの場合は下の手動 Add Provider フローで追加してください。", "descriptionGo": "API key だけで接続できます。OpenCode Go は別のエンドポイントを持つ独立した小さいカタログで、OpenCode Zen にある GPT/Claude/Gemini などの上位モデルは含まれません。お使いの key が Go プランの場合はこちらを選んでください。Go 自体のエンドポイントは無料モデルを受け付けないため、Vellum は OpenCode Zen の無料モデルも別の Provider として自動的に接続します。", "freeRouteName": "{{name}}（無料モデル）", "catalog": "カタログ", "catalogZen": "OpenCode Zen（無料モデル）", "catalogGo": "OpenCode Go", "catalogHint": "実際に契約しているプランを選んでください。Zen は無料モデルのみに接続し、Go は Go プランのカタログに加えて同じ無料モデルも自動的に接続します。", "apiKey": "OpenCode Zen API key", "apiKeyPlaceholder": "OpenCode Zen API key を貼り付け", "connect": "接続", "connecting": "接続中…", "connected": "接続済み" },
      "addProvider": { "title": "Provider を追加", "steps": { "endpoint": "エンドポイントを接続", "endpointHint": "Provider のモデル一覧を取得します。", "probe": "選択して検証", "probeHint": "モデルを選び、そのモデルの Codex プロトコル対応だけを検証します。", "add": "追加", "addHint": "結果を確認して Codex モデルカタログに追加します。" }, "endpoint": "エンドポイント URL", "apiKey": "API key（任意・暗号化保存）", "apiKeyPlaceholder": "ローカルまたはアカウント認証済みのエンドポイントは空欄で可", "startProbe": "モデル一覧を取得", "probing": "モデル一覧を取得中…", "add": "Provider を追加", "reprobe": "やり直す" },
      "probe": { "reachable": "接続可能", "wire": "API プロトコル", "modelCount": "{{count}} 個のモデルを検出", "modelsMissing": "検出できません。入力してください", "context": "コンテキストウィンドウ", "required": "入力が必要", "streaming": "ストリーミング", "supported": "対応", "unsupported": "非対応", "toolCalling": "Codex ツールプロトコル", "typedToolCalls": "構造化ツール呼び出しを検証済み", "toolCallingUnavailable": "構造化ツール呼び出しは未検証", "chatOnly": "チャットのみ", "reasoning": "推論", "detected": "検出済み", "notDetected": "未検出", "serverResume": "上流で会話を保持", "remembers": "保持する", "localHistory": "保持しない（Vellum が履歴を保存）", "detectedModels": "検出されたモデル", "selectionHint": "選択して検証に通ったモデルだけを Codex カタログに追加します。", "verifyToImport": "選択してツール呼び出しを検証", "verificationRequired": "能力検証が必要", "timeout": "検出がタイムアウトしました", "unknown": "不明", "verifyModel": "検証", "verifyingModel": "検証中…",  "quotaWithRetry": "クォータ制限（HTTP 429 · {{seconds}}秒後に再試行）", "unauthorized": "認証失敗（HTTP 401/403）", "protocolError": "プロトコル形式エラー", "toolCallMissing": "構造化ツール呼び出しなし", "unsupportedOpenCodeProtocol": "Anthropic／Google ネイティブプロトコルは未対応です", "defaultModel": "既定モデル", "defaultModelHint": "すべてのモデルを Codex カタログに追加し、ここでは Provider 作成後の初期モデルだけを選びます。", "select": "選択", "wireHint": "Provider が受け付ける API プロトコルです。レスポンス JSON の形式ではありません。", "manualPlaceholder": "未検出。手動で入力", "settingsTitle": "エンドポイント検出設定", "requestSize": "リクエストサイズ", "preferredWire": "優先 API 形式", "preferredWireValue": "Responses を試し、次に Chat Completions", "contextSource": "コンテキスト長のソース", "contextSourceValue": "手動設定、エンドポイント情報、モデルキャッシュ、Codex カタログの順に使用", "history": "会話履歴の保存", "historyValue": "上流がセッション継続に対応しない場合、Vellum が暗号化して保存します。" },
      "modelCatalog": { "rename": "名前を変更", "renameProvider": "Provider の表示名", "renameModel": "モデルの表示名", "renameHint": "表示名だけを変更します。送信されるのは引き続き上流の id で、Codex の既存ルートにも影響しません。", "allowPrivateNetworkHttp": "プライベートネットワークへの平文 HTTP を許可", "allowPrivateNetworkHttpHint": "LAN や Tailscale などのアドレス向け。既定ではループバック以外への平文 HTTP は拒否されます。これを有効にするとプライベートネットワーク（LAN、CGNAT、Tailscale）アドレスへの接続が許可されます。公開インターネットへの平文 HTTP は引き続き拒否されます。", "renameModelPlaceholder": "空欄なら上流の id を表示", "renameSave": "保存", "renameCancel": "キャンセル", "free": "無料", "deprecated": "提供終了", "vision": "画像", "visionOn": "このモデルは画像入力に対応します", "visionOff": "テキストのみ。チェックすると Codex が添付を許可します", "visionHint": "画像対応は自動検出できません。テキスト専用のエンドポイントでも画像付きリクエストに 200 を返し、モデルが答えを作ってしまいます。モデルの実際の対応に合わせて設定してください。", "tokenUnit": "token", "effort": "Effort", "responses": "Responses", "chat": "Chat Completions", "title": "モデルカタログ", "hint": "選択したモデルが Codex モデルカタログに表示されます。", "empty": "Provider はまだありません。上のフォームから追加してください。", "countSelected": "{{selected}} / {{total}} 個", "count": "{{count}} 個", "unknownWindow": "ウィンドウ不明", "reasoning": "推論", "noReasoning": "推論なし", "auto": "自動", "capabilityMissing": "この Provider のモデル能力はまだ取得されていません。", "recent": "最近使用", "reprobe": "能力を再検出", "reprobeInProgress": "選択中の {{count}} モデルを検証しています…", "reprobeSummary": "選択中のモデル {{succeeded}}/{{targeted}} を検証しました", "reprobeSummaryWithFailures": "選択中のモデル {{succeeded}}/{{targeted}} を検証、{{failed}} 件失敗", "effortNotProbed": "未検出", "effortUnverified": "検証できません", "effortReasonIgnored": "Provider が意図的に無効な Effort 値も成功として受け入れたため、low／medium／high などが実際に反映されるか証明できません。同じ検証を繰り返しても結果は通常変わりません。", "effortReasonQuota": "Effort の検証は、Provider のクォータまたはアカウント権限によって妨げられました{{detail}}", "effortReasonProvider": "Effort の検証中に Provider エラー、タイムアウト、または不完全な応答が発生しました{{detail}}", "effortReasonUnknown": "対応レベルを検証するのに十分な証拠を取得できませんでした{{detail}}", "retryEffortProbe": "Effort の検証を再試行", "footerNote": "Provider の有効化・無効化はすぐ保存されますが、Proxy の再起動後に有効になります。モデルカタログは Codex の再起動後に再読み込みされます。完了までは「反映待ち」と表示します。", "windowEdit": "最大コンテキスト長を設定", "windowHint": "このモデルの最大コンテキスト長を設定します", "windowAuto": "自動", "windowManual": "手動で設定した値です。空欄にすると検出値に戻ります。", "windowUnavailable": "このモデルはまだ Codex カタログに入っていないため、設定先がありません。", "windowInvalid": "最大コンテキスト長は 0 より大きい数値か、自動にする場合は空欄にしてください。", "windowSaveFailed": "最大コンテキスト長を保存できませんでした：{{detail}}" }
    }
  },
  "context": {
    "awaitingCompaction": "この会話ではまだ圧縮が起きていません。",
    "transcript": {
      "eyebrow": "圧縮トランスクリプト",
      "title": "圧縮指示と置換本文",
      "hint": "モデルに送った指示と、返された置換本文を表示します。次のターンはこの置換本文を使用します。",
      "chars": "{{value}} 文字",
      "prompt": "送った指示",
      "promptNote": "モデルが受け取ったままの圧縮プロンプト。",
      "result": "書き戻された本文",
      "resultNote": "ここから先、この会話が持ち歩く内容。",
      "noPrompt": "この圧縮の指示は Vellum を通っていません。",
      "noResult": "この圧縮の結果はここからは読めません。",
      "empty": "まだ表示できるものがありません。圧縮がまだ起きていないか、Vellum が読めない場所で行われました。",
      "unavailable": {
        "codexDesktopOpaque": "Codex Desktop がこの圧縮を不透明な状態として保存したため、Vellum は指示と置換本文を読み取れません。",
        "notCompacted": "この会話ではまだ圧縮が起きていません。",
        "officialOpaque": "この圧縮は OpenAI Official が不透明な状態として管理しているため、Vellum は指示と置換本文を読み取れません。",
        "legacyUnreadable": "この古い圧縮記録には、読み取り可能な指示と置換要約がありません。"
      }
    },
    "sessions": {
      "eyebrow": "Enhanced Codex core",
      "title": "Enhanced core 上のセッション",
      "hint": "Enhanced Codex core で実行されている会話を表示します。OpenAI のネイティブコアで動く会話は Codex 内部で圧縮されるため、ここには表示されません。",
      "listTitle": "セッション",
      "loading": "セッションを読み込んでいます…",
      "empty": "Enhanced Codex core で動いている会話は今ありません。",
      "untitled": "無題のセッション",
      "count_one": "{{count}} 件のセッション",
      "count_other": "{{count}} 件のセッション",
      "pick": "{{label}} の圧縮を表示",
      "headroom": "圧縮まで {{percent}}%",
      "imminent": "しきい値に到達",
      "search": "タイトルで検索",
      "searchClear": "絞り込みを解除",
      "countFiltered": "{{total}} 件中 {{shown}} 件",
      "noMatch": "「{{query}}」に一致するものはありません。"
    },
    "title": "コンテキスト", "compactionEyebrow": "コンテキスト圧縮", "compactionTitle": "コンテキスト圧縮の状態とプレビュー", "compactionHint": "Codex App で /compact を入力して圧縮します。Vellum は状態と引き継ぎ履歴を表示します。", "before": "圧縮前", "after": "圧縮後", "opaque": "Opaque", "legendAria": "コンテキスト構成", "previewLoading": "圧縮プレビューを計算中…", "footerNote": "圧縮は会話を実行する Codex runtime が管理します。Vellum は観測イベントと旧 journal の読み取り専用表示だけを提供します。",
 "segmentAria": { "before": "{{label}}、圧縮前 {{tokens}} token", "after": "{{label}}、圧縮後 {{tokens}} token" },
    "segmentLabel": {
      "canonical": "Canonical チェックポイント",
      "reasoning": "Readable Reasoning Replay",
      "tool": "ツール継続状態",
      "retained": "保持された会話",
      "observedTotal": "Codex が報告した合計"
    },
    "segmentNote": { "canonical": "目標、制約、判断、進捗、ファイル、次の手順を保持します。", "reasoning": "第三者の private ciphertext を送らず、必要な推論を可読サマリーで再生します。", "tool": "安全に継続できるツール結果と呼び出しの対応状態を保持します。", "retained": "最近のメッセージと実行コンテキストを保持し、最大 {{turns}} ターンを優先します。", "observedTotal": "Codex クライアントが報告した token 合計の変化です。カテゴリ別の内訳は公開されていません。", "default": "Canonical 圧縮方針で処理します。" },
    "readout": { "tokenTransition": "{{before}} → {{after}} token", "savedDelta": "{{tokens}} token 削減", "tokenUnit": "token", "total": "合計", "officialStat": "{{before}} → OpenAI opaque canonical", "officialNote": "公式の圧縮状態は暗号化されています。Vellum は ciphertext のサイズから token を推定しません。", "saved": "{{saved}} token を削減（{{percent}}%）", "hover": "セグメントにカーソルを合わせると詳細を表示", "share": "圧縮前の {{percent}}%" },
    "summary": { "goal": "目標", "acceptanceCriteria": "受け入れ条件", "constraints": "制約", "userPreferences": "ユーザー設定", "done": "完了", "inProgress": "進行中", "blocked": "ブロック中", "decisions": "判断", "changedFiles": "変更ファイル", "relevantFiles": "関連ファイル", "commands": "コマンド", "tests": "テスト", "unresolved": "未解決", "errors": "エラー", "criticalContext": "重要な背景", "references": "参照", "nextSteps": "次の手順" },
    "errors": { "partialRefresh": "一部のデータを更新できませんでした：{{detail}}" }
  },
  "log": {
    "title": "ログ",
    "activityTitle": "Token アクティビティ",
    "loading": "ログを読み込み中…",
    "heading": "Token 使用量とリクエストログ",
    "days_one": "{{count}} 日",
    "days_other": "{{count}} 日",
    "boot_one": "起動 {{count}} 回 · PID {{pid}} · 前回起動 {{time}}",
    "boot_other": "起動 {{count}} 回 · PID {{pid}} · 前回起動 {{time}}",
    "bootFirst": "初回起動 · PID {{pid}}",
    "stats": {
      "total": "累計 Token 数",
      "peak": "Token ピーク",
      "longestRequest": "最長リクエスト時間",
      "currentStreak": "現在の連続記録",
      "longestStreak": "最長連続記録"
    },
    "providers": {
      "title": "Provider 別の累計 Token",
      "note": "OpenAI は Codex プロファイルに合わせ、第三者は upstream の input + output を集計します",
      "others": "その他の Provider", "fromProfile": "Codex プロファイル由来", "segmentAria": "{{provider}}：累計トークンの {{percent}}、{{value}}", "accountCount_one": "（{{count}} アカウント）",
      "accountCount_other": "（{{count}} アカウント）"
    },
    "tokens": "{{value}} Token",
    "requests": {
      "title": "リクエスト詳細",
      "note": "最新 {{count}} 件のみ表示",
      "empty": "リクエストログはありません。Codex が最初のリクエストを送ると表示されます。",
      "filterEmpty": "現在のフィルター条件に一致するリクエストはありません。",
      "filter": { "statusAll": "すべて", "statusFailed": "失敗のみ", "providerAll": "すべての Provider" },
      "cached": "キャッシュ {{percent}}%",
      "connection": "conn {{id}}",
      "streamQualityTitle": "ストリーム品質：{{quality}}",
      "accountIdentityTitle": "ハッシュ化されたコントロールアカウント A と実行アカウント B",
      "streamQuality": {
        "incremental": "増分",
        "end_flush": "終了時の一括出力",
        "buffered": "バッファ",
        "no_delta": "差分なし"
      },
      "subagentChildren_one": "サブエージェント {{count}} 件",
      "subagentChildren_other": "サブエージェント {{count}} 件",
      "subagentChildrenTitle_one": "派生したサブエージェント {{count}} 件を表示",
      "subagentChildrenTitle_other": "派生したサブエージェント {{count}} 件を表示"
    },
    "systemEvents": { "title": "システムイベント" },
    "compaction": {
      "title": "圧縮",
      "note": "自動圧縮の判定。リクエスト行とは分けて表示します",
      "empty": "圧縮イベントはまだありません。",
      "tokens": "{{before}} → {{after}}",
      "items": "アイテム {{before}} → {{after}}",
      "threshold": "しきい値 {{percent}}%",
      "window": "{{active}} / {{window}} コンテキスト",
      "checkpoint": "checkpoint {{id}}（第 {{generation}} 世代）",
      "noCheckpoint": "永続 checkpoint なし（stateless）"
    },
    "subagent": {
      "title": "サブエージェント",
      "note": "1 行が 1 つのサブエージェント実行 —— 展開すると requested → child → completed のタイムラインが見えます",
      "empty": "サブエージェントの実行はまだありません。",
      "summary": { "label": "サブエージェント実行の概要", "total": "合計 {{count}} 件", "completed": "完了 {{count}}", "active": "実行中 {{count}}", "attention": "要確認 {{count}}" },
      "call": "call {{id}}",
      "child": "child {{id}}",
      "parent": "親 {{id}}",
      "locateParent": "親リクエストを表示（{{id}}）",
      "locateChild": "子リクエストを表示（{{id}}）",
      "requestedAt": "要求 {{time}}",
      "completedAt": "終了 {{time}}",
      "linkLabel": "関連：{{value}}",
      "outcome": "結果：{{value}}",
      "error": "エラー：{{value}}",
      "unknownModel": "不明なモデル",
      "state": {
        "requested": "リクエスト済み",
        "running": "実行中",
        "completed": "完了",
        "failed": "失敗",
        "cancelled": "キャンセル",
        "ambiguous": "判定不能",
        "unlinked": "未紐付け"
      },
      "link": {
        "exact": "厳密な紐付け",
        "heuristic": "推定の紐付け",
        "unlinked": "紐付けなし"
      },
      "timeline": {
        "requested": "リクエスト",
        "child": "子リクエスト",
        "completed": "完了",
        "pending": "待機中",
        "noChildLink": "子リクエストが特定できません"
      }
    },
    "invokes": {
      "title": "デスクトップ呼び出し",
      "note": "直近 {{count}} 件の Tauri invoke — 成功と失敗",
      "empty": "このセッションで記録されたデスクトップ呼び出しはありません。",
      "ok": "ok",
      "error": "error"
    },
    "errors": {
      "partialRefresh": "一部のデータを更新できませんでした：{{detail}}"
    }
  },
  "remote": {
    "title": "リモートホスト",
    "blurb": "SSH ホストを、Codex App からそのまま使える状態に整えます。チャット・thread・session は引き続きホスト上の Codex native daemon が担当します。",
    "rescan": "再探索",
    "rescanning": "探索中…",
    "discovering": "Codex／OpenSSH の接続設定を読み込んでいます。操作は続けられます。SSH の状態はバックグラウンドで読み込まれます。",
    "featureDisabled": "このビルドでは Native Codex リモート管理は有効になっていません。",
    "legacyBrokerUnsupported": "サポート終了の Broker ペアリング設定が見つかったためスキップしました。SSH でこのホストを再検出してください。Vellum は旧 Broker ペアリングを移行しません。",
    "noHosts": "Codex App にも OpenSSH の設定にも、使える接続が見つかりませんでした。",
    "hostsAria": "リモートホスト",
    "probing": "調査中…",
    "probeFailed": "調査に失敗",
    "notProbed": "未調査",
    "updating": "更新中…",
    "lastUpdated": "{{when}} に更新",
    "sshInvalid": "SSH 設定に誤りがあります",
    "otherHostBusy": "{{host}} で処理がまだ進行中です。",
    "goToHost": "そちらを見る",
    "blockerUnknown": "このビルドがまだ認識していない状態に遭遇しました。",
    "releaseBlocked": "同梱のデプロイパッケージが署名検証を通過していないため、Codex のインストールと Agent の更新は無効です。デプロイ済みホストの計画と同期は引き続き利用できます。",
    "proxyImageUpdate": "この Vellum ビルドには新しい Proxy image が含まれています。「再同期」を選択してこのホストに適用してください。",
    "state": {
      "unreachable": "接続不可",
      "unmanaged": "未デプロイ",
      "readyToPlan": "デプロイ途中",
      "drifted": "設定にずれあり",
      "nativeActive": "使用中",
      "detachedReady": "切断後も継続可"
    },
    "verdict": {
      "unreachable": "このホストの Vellum Agent に接続できません。未インストールか、SSH 側が切れている可能性があります。",
      "unmanaged": "このホストはまだ Vellum の管理下にありません。デプロイすれば Codex App からそのまま使えます。",
      "readyToPlan": "Proxy は動いていますが、Codex のモデルカタログはまだ引き継いでいません。",
      "drifted": "ホスト上の設定が Vellum の記録と違います。再同期が必要です。",
      "nativeActive": "使用できます。Codex App はこのホストの native daemon を経由しています。",
      "detachedReady": "使用できます。Vellum を閉じても turn は動き続けます。"
    },
    "summary": {
      "threads_one": "{{count}} 件の thread が進行中",
      "threads_other": "{{count}} 件の thread が進行中"
    },
    "act": {
      "bootstrap": "このホストをデプロイ",
      "reconverge": "再同期",
      "plan": "モデルを変更…",
      "replan": "プレビューを更新",
      "apply": "確認して同期",
      "restartNative": "リモートの Codex daemon を再起動",
      "takeoverNative": "リモートの Codex daemon を引き継ぐ",
      "installCodex": "pinned Codex CLI をインストール",
      "updateAgent": "リモート Agent を更新",
      "syncDesktopCodex": "Desktop Codex runtime を同期",
      "repair": "codex ランチャーを修復",
      "bundle": "診断バンドルを書き出す",
      "restore": "Vellum の管理を解除…",
      "retry": "再試行",
      "stopAppOwned": "安全に停止して再試行",
      "grokLogin": "Grok アカウントにログイン",
      "grokCancel": "Grok ログインを中止",
      "grokRefresh": "Grok 資格情報を更新",
      "chatgptPair": "デスクトップで選択した ChatGPT アカウントとペアリング",
      "chatgptActivate": "デスクトップで選択した ChatGPT アカウントを有効化",
      "chatgptPairRow": "ペアリング", "chatgptActivateRow": "使用", "chatgptPairAll": "デスクトップの ChatGPT アカウントをすべてペアリング",
      "chatgptPairSkip": "このアカウントをスキップ",
      "executionLogin": "Official 実行アカウントを追加",
      "executionSelect": "モデル要求に使用",
      "executionRemove": "実行アカウントを削除",
      "devicePair": "このスマートフォンをペアリング"
    },
    "trust": {
      "title": "このホストの SSH 鍵を確認",
      "explain": "これは trust-on-first-use です。Vellum はこのホストの SSH 鍵をまだ見たことがありません。確認する前に、下記のフィンガープリントを別の経路で照合してください。",
      "factHost": "SSH ホスト",
      "factFingerprint": "フィンガープリント",
      "factCrossCheck": "照合手段",
      "crossCheckHint": "すでに信頼している経路 — ホスト自体のコンソール、Tailscale や VPN の管理画面など。",
      "confirm": "信頼して続行",
      "checking": "このホストが既に信頼済みか確認しています…",
      "fetchFailed": "このホストの SSH 鍵を取得できませんでした。",
      "untrustedError": "このホストの SSH 鍵はまだ確認されていません。"
    },
    "chore": {
      "restartNative": "新しい設定とモデルカタログを反映します。Proxy は止まりませんが、実行中の turn が中断される場合があります。",
      "takeoverNative": "現在 Codex App がこの daemon を直接保持しています。引き継ぐと Vellum 管理の native daemon に置き換わり、実行中の作業が中断される場合があります。",
      "installCodex": "同梱の pinned Codex CLI をホストに導入します。manifest と digest を検証してからアトミックに差し替えます。",
      "updateAgent": "ホスト上の vellum-remote-agent だけを更新します。Broker・Proxy イメージ・Codex CLI はこの操作では更新されません。",
      "syncDesktopCodex": "この Desktop core と完全に一致する公式 Linux Codex build を導入し、プロトコル検証後にリモート daemon を再起動します。Desktop {{desktop}}、リモート {{remote}}。",
      "repair": "codex コマンドのランチャーを作り直し、Codex App が管理下の CLI を見つけられるようにします。",
      "bundle": "Agent・Docker・Proxy・daemon と操作ログを、機微な値を伏せて書き出します。不具合の報告時に添えてください。"
    },
    "fact": {
      "changes": "変更するもの",
      "untouched": "触らないもの",
      "turns": "実行中の turn"
    },
    "confirm": {
      "gate": "続けるにはホスト名を入力してください：{{host}}",
      "bootstrap": {
        "changes": "Codex の設定ファイル・モデルカタログ・Proxy と必要な資格情報。初回は検証済みのモデルをすべて適用し、以降は保存した選択を引き継ぎます。",
        "untouched": "ホスト上のプロジェクト、通常の Codex thread、自分で入れたソフトウェア。",
        "turns": "拒否されます。強制的に中断することはありません。"
      },
      "restore": {
        "changes": "Codex 本来の設定ファイルとモデルカタログを復元し、アカウントの lease を返却し、Vellum 管理の Proxy と runtime を停止します。",
        "untouched": "あなたのプロジェクト、通常の Codex thread、ホスト上の Codex 本体と Agent。",
        "turns": "拒否されます。強制的に中断することはありません。"
      },
      "restartNative": {
        "changes": "ホスト上の Codex native daemon を再起動し、新しい設定とモデルカタログを反映します。",
        "untouched": "Proxy は動いたままで、設定ファイルとモデルカタログは書き換えません。",
        "turns": "実行中の turn は中断されます。"
      },
      "takeoverNative": {
        "changes": "Codex App が直接使っている app-server を、Vellum 管理の native daemon に置き換えます。",
        "untouched": "設定ファイル・モデルカタログ・ログイン済みのアカウント。",
        "turns": "Codex App 側で実行中の作業が中断される場合があります。"
      },
      "installCodex": {
        "changes": "同梱の pinned Codex CLI をホストに導入し、manifest と digest を検証してからアトミックに差し替えます。",
        "untouched": "Codex の設定ファイル・モデルカタログ・既存の thread。",
        "turns": "実行中の turn は止めません。新しいビルドは daemon の再起動後に反映されます。"
      },
      "updateAgent": {
        "changes": "vellum-remote-agent を、この Vellum に同梱されたバージョンへ更新し、digest を検証します。",
        "untouched": "Broker・Proxy イメージ・Codex CLI はこの操作では更新されません。",
        "turns": "Agent が自身で再起動するため、リモート操作が一時的に途切れます。"
      },
      "syncDesktopCodex": {
        "changes": "Desktop core と完全に一致する OpenAI 公式 Linux artifact を取得し、発行元の digest を検証して app-server プロトコルを確認したうえでアトミックに導入し、リモート daemon を再起動します。",
        "untouched": "Vellum Proxy イメージ、モデルカタログ、アカウント、プロジェクト、既存の task。",
        "turns": "リモート daemon の再起動時に、実行中の turn が中断される場合があります。"
      },
      "stopAppOwned": {
        "changes": "Codex App が直接使っている app-server を安全に停止し、デプロイをもう一度実行します。",
        "untouched": "設定ファイル・モデルカタログ・ログイン済みのアカウント。",
        "turns": "turn がまだ動いている場合、Vellum は停止を拒否します。強制的には切りません。"
      }
    },
    "blocker": {
      "agentUnavailable": "ホスト上の Vellum Agent に接続できません。",
      "codexRestartRequired": "設定は書き込み済みですが、再起動するまで反映されません。",
      "credentialsMissing": "このデプロイに必要な資格情報がホストにありません。",
      "desktopOfficialAccountMissing": "デスクトップ側で ChatGPT アカウントが選択されていないため、リモートとペアリングできません。",
      "dockerUnavailable": "ホストに使える Docker がなく、Proxy を起動できません。",
      "intelMacUnsupported": "Intel Mac は未対応です。Remote Manager の初版は Apple Silicon のみです。",
      "guiSessionUnavailable": "macOS のログインセッションがありません。Proxy はログイン後に常駐し、電源や自動ログインの設定は変更しません。",
      "proxyPortConflict": "リモート Proxy の既定ポートが使用中です。デプロイ計画で別ポートを指定するか、127.0.0.1:15722 を空けてください。",
      "insufficientDiskSpace": "ダウンロード・展開・ロールバック用の空き容量が足りないため、置換を中止しました。ユーザーデータは自動削除しません。",
      "incompleteObservation": "管理対象 runtime の turn / ツール / 承認を完全に観測できないため、破壊的操作を遮断しました。",
      "hostNotConfigured": "このホストにはまだ Vellum の設定が書き込まれていません。",
      "injectionRequiresReadyProxy": "モデルを Codex の一覧へ注入できるのは、Proxy が準備完了になってからです。",
      "invalidCompactionThreshold": "圧縮しきい値の設定が許容範囲外です。",
      "managedRuntimeRecoveryRequired": "管理下の runtime が異常な状態で止まっています。先に修復が必要です。",
      "nativeCodexVersionMismatch": "ホスト上の Codex CLI のバージョンが合いません。pinned 版の導入が必要です。",
      "nativeDaemonAppOwned": "Codex App がこのホストの app-server を直接使っているため、Vellum が引き継げません。",
      "noModelsSelected": "モデルが 1 つも選ばれていないため、同期するものがありません。",
      "officialAccountActivationRequired": "リモートの ChatGPT アカウントはペアリング済みですが、まだ有効化されていません。",
      "officialAccountPairingRequired": "デスクトップで選択した ChatGPT アカウントが、リモートとまだペアリングされていません。",
      "proxyConfigurationMissing": "ホストにはまだ Proxy の設定がありません。",
      "proxyConfigurationSchemaTooNew": "ホスト上の Proxy 設定は、この Vellum が理解できるより新しい形式です。上書きを避けるためデプロイをブロックしています — 先に Vellum を更新してください。",
      "proxyConfigurationUnreadable": "ホスト上の Proxy 設定を読み取れません（権限または I/O の問題）。安全に置き換え可能とみなさず、デプロイをブロックしています。",
      "systemdUserUnavailable": "ホストに使える user systemd がないため、daemon を常駐させられません。",
      "versionMismatch": "ホスト上の Agent のバージョンが、この Vellum と一致しません。"
    },
    "configuration": {
      "upgradeRequired": "リモートの Proxy 設定にはセキュリティ上のアップグレードが必要です。再デプロイすると自動的に修復されます。",
      "repairRequired": "既存の Proxy 設定が無効です。再デプロイすると再構築されます。",
      "incompatible": "ホスト上の Proxy 設定は、この Vellum が理解できるより新しい形式です。先に Vellum を更新してから、このホストへデプロイしてください。",
      "unreadable": "ホスト上の Proxy 設定を読み取れません（権限または I/O の問題）。デプロイの前にホスト側で解決してください。"
    },
    "phase": {
      "queued": "キューに追加",
      "cleanHostPreflight": "同梱物とクリーンなホスト基準を検証",
      "hostPreflight": "SSH・Docker・ユーザーサービスを確認",
      "resolvingDesktopCodex": "Desktop と一致する Codex runtime を解決・検証",
      "installCodex": "同梱の pinned Codex CLI を導入",
      "nativeDaemon": "常駐型の Codex リモート制御を有効化",
      "deploymentPlan": "対象モデルと資格情報を洗い出し",
      "deploymentApply": "Proxy・資格情報・モデルカタログを導入",
      "applying": "デプロイ計画を適用",
      "verification": "Proxy・daemon・切り離し準備を検証",
      "verified": "準備完了",
      "restorePreflight": "実行中の turn と管理状態を確認",
      "restoreLease": "Codex の設定とモデルカタログを復元",
      "restartNative": "復元した設定で native Codex を再起動",
      "stopProxy": "Vellum 管理の Proxy を停止",
      "restoreVerification": "Vellum がデータ経路から外れたことを確認",
      "restored": "管理を解除しました",
      "failed": "失敗"
    },
    "operation": {
      "bootstrap": "デプロイ中",
      "apply": "同期中",
      "restore": "管理を解除中",
      "desktopCodexSync": "Desktop Codex runtime を同期中",
      "elapsed": "経過 {{clock}}"
    },
    "error": {
      "discovery": "接続設定の読み込みに失敗しました。",
      "desktopCodexMismatch": "リモート Codex runtime は現在の Desktop プロトコルと互換性がありません。先に Desktop Codex runtime を同期してください。",
      "boundaryKeyProvisionFailed": "リモート Proxy の認証キーを修復できませんでした。再試行するか、解決しない場合は診断バンドルを書き出してください。",
      "actionFailed": "「{{action}}」に失敗しました。上のホスト状態は読み直した最新の内容です。",
      "operationFailed": "{{action}}が「{{phase}}」で止まりました。"
    },
    "detail": {
      "title": "ホストの詳細",
      "host": "ホスト",
      "runtime": "実行環境",
      "version": "バージョン",
      "sshResolved": "名前解決済み",
      "sshUnresolved": "名前解決できません",
      "agentAbsent": "未インストール",
      "cores_one": "{{count}} コア",
      "cores_other": "{{count}} コア",
      "diskFree": "空き {{free}} / 全体 {{total}}",
      "dockerAbsent": "未インストール",
      "proxyReady": "準備完了 · {{image}}",
      "config": "設定",
      "proxyNotReady": "動作中・準備未完了",
      "proxyStopped": "停止済み",
      "launcherLoginShell": "ログインシェル",
      "launcherBroken": "利用不可 — 修復を実行してください",
      "daemonRunning": "動作中 · PID {{pid}}",
      "daemonStopped": "停止",
      "daemonOwner": "Daemon の所有者",
      "durable": "常駐可",
      "notDurable": "常駐不可",
      "codexSource": "Codex の入手元",
      "releaseTrust": "パッケージの信頼性",
      "releaseUnverified": "未検証 · {{trust}}",
      "verified": "検証済み",
      "pinned": "pinned バージョン",
      "inventoryBlockers": "点検で見つかった阻害要因",
      "platform": "プラットフォーム",
      "proxyBackend": "Proxy バックエンド",
      "persistence": "常駐範囲",
      "loginResident": "ログイン後常駐",
      "lingerResident": "systemd linger で常駐",
      "managedHome": "管理対象 CODEX_HOME",
      "isolationLabel": "分離",
      "isolation": "リモート設定は、ローカルの Vellum / Enhanced / ~/.codex と分離されたままです。"
    },
    "desktopCodex": {
      "desktop": "Desktop Codex",
      "remote": "リモート Codex",
      "status": "プロトコル互換性",
      "states": {
        "current": "この Desktop で検証済み",
        "updateAvailable": "リモート runtime の更新が必要",
        "qualificationRequired": "プロトコルの再検証が必要",
        "agentUpdateRequired": "先にリモート Agent の更新が必要",
        "desktopUnavailable": "Desktop core を取得できません",
        "unavailable": "互換性を確認できません"
      }
    },
    "account": {
      "title": "アカウント",
      "chatgptUnavailable": "読み取れません",
      "chatgpt": {
        "synchronized": "同期済み",
        "pairingRequired": "ペアリングが必要",
        "pairingPending": "ペアリング中",
        "activationRequired": "有効化が必要",
        "desktopAccountUnavailable": "デスクトップ側でアカウント未選択"
      },
      "grokReady": "{{account}} でログイン済み、refresh timer 稼働中",
      "grokPartial": "資格情報は導入済みですが、リモート CLI または refresh token がまだ条件を満たしていません",
      "grokAbsent": "未設定",
      "pairingHint": "デスクトップで選択したアカウントだけでログインしてください：",
      "pairingRemaining": "残り {{remaining}} 件",
      "pairingActive": "このホストで使用中",
      "pairingPaired": "ペアリング済み・未使用",
      "pairingMissing": "このホストでは未ペアリング",
      "pairingUnknown": "読み取れません",
      "desktopDefault": "デスクトップの既定",
      "grokDeviceLogin": "Grok デバイスログイン：",
      "grokWaitingUrl": "URL を待っています…",
      "grokStarting": "Grok CLI を待機中…"
    },
    "control": {
      "title": "Remote コントロール",
      "identity": "コントロールアカウント A",
      "sameAccountHint": "Desktop とスマートフォンは同じ ChatGPT アカウントと workspace を使う必要があります。ペアリングしても、リモート daemon の ID は変わりません。",
      "deviceHint": "短時間だけ有効なデバイスペアリング情報：",
      "expiresAt": "{{when}} に失効"
    },
    "execution": {
      "title": "Proxy 実行アカウント",
      "independentHint": "Official 実行アカウント B はモデル要求だけに使用されます。選択しても Remote コントロールの状態は変わらず、切断もされません。",
      "empty": "Vellum 管理の Official 実行アカウントはありません。",
      "selected": "選択中",
      "select": "選択",
      "remove": "削除",
      "namePlaceholder": "Official アカウントの表示名",
      "loginHint": "Official デバイスログインを完了："
    },
    "maintenance": {
      "title": "メンテナンス"
    },
    "danger": {
      "body": "このホストに対する Vellum の管理を取り消し、Codex をデプロイ前の状態に戻します。これは切断ボタンではありません —— もう一度使うには再デプロイが必要です。プロジェクトと通常の Codex thread は削除されません。"
    },
    "sessions": {
      "cap": "ネイティブ session の観測",
      "title": "Codex daemon の thread",
      "unknown": "native session の状態をまだ取得していません。",
      "empty": "native daemon に見えている thread は今のところありません。",
      "unreadable": "native app-server の thread 一覧を今は読み取れません。",
      "turns_one": "{{count}} turn · 直近 {{last}}",
      "turns_other": "{{count}} turn · 直近 {{last}}",
      "observability": {
        "nativeAppServer": "観測可能",
        "unsupported": "非対応",
        "daemonDown": "daemon 停止"
      }
    },
    "plan": {
      "cap": "デプロイ計画",
      "title": "モデルとポリシーの同期",
      "ready": "同期できます",
      "blocked": "ブロック中",
      "configHash": "設定ハッシュ",
      "catalogHash": "カタログハッシュ",
      "reviewPolicy": "自動レビュー",
      "reviewPolicyPending": "設定が変更されました — 再適用待ち",
      "credentials": "資格情報",
      "noCredentials": "不要",
      "revision": "リビジョン",
      "revisionValue": "目標 {{desired}} / 現状 {{observed}}",
      "planHash": "計画ハッシュ",
      "rollback": "復旧方法",
      "managed": "管理対象の項目",
      "changed": "差分あり",
      "same": "同一"
    }
  },
  "onboarding": {
    "title": "Vellum をはじめる",
    "acts": { "what": "概要", "connect": "接続", "features": "機能", "launch": "起動" },
    "folio": { "1": "一", "2": "二", "3": "三", "4": "四" },
    "ui": { "providerSeparator": "、", "back": "戻る", "skipHint": "Provider を接続せずに進み、後でモデルページから追加できます。", "skip": "今はスキップ", "continue": "続ける", "enter": "Vellum に入る", "enterWithoutProxy": "Proxy を起動せずに入る", "start": "開始", "skipSetup": "設定済みなので直接入る", "unwritten": "第 {{step}} 幕、未開始", "visited": "既読（戻れます）", "login": "サインイン", "collapse": "折りたたむ", "fillEndpoint": "エンドポイントを入力", "connected": "接続済み", "unavailable": "利用不可", "waitingAuth": "認証を待機中", "notConnected": "未接続", "waitingAuthEllipsis": "認証を待機中…", "addAnother": "別の接続を追加", "enterCode": "ブラウザーでこのコードを入力", "providerType": "Provider の種類", "customEndpoint": "カスタム API エンドポイント", "opencodeHint": "OpenCode Zen は公式の固定エンドポイントを使用します。API key だけを入力すると、Vellum が対応モデルを取得して検証します。", "opencodeApiKey": "OpenCode Zen API key", "opencodeApiKeyPlaceholder": "OpenCode Zen API key を貼り付け", "displayName": "表示名", "displayNamePlaceholder": "例：セルフホスト vLLM", "endpoint": "エンドポイント", "optionalPlaceholder": "不要なら空欄", "endpointHint": "空欄にすると、このエンドポイントは検証されません。", "probing": "検出中…", "probeAndAdd": "検出して追加", "notConnectedYet": "まだ接続されていません" },
    "overture": { "lead": "削り、書き直す。", "history": "中世のヴェラムは貴重で、一度きりでは使えませんでした。ページがいっぱいになると、古い文字を削り落として書き直し、その痕跡はうっすらと残りました。", "mission": "Vellum もコンテキストに同じことをします。ウィンドウがいっぱいになると切り詰めるのではなく書き直し、大事な内容はそのまま残ります。", "subtitle": "このコンピューターだけで動く Codex Proxy — 次の四幕はこのページに記されています" },
    "what": { "axisTargets": "ChatGPT ／ Grok ／ カスタム", "title": "中央でつなぐもの", "lead": "Codex は元のエンドポイントを呼び続けます。Vellum は中央で、選択した Provider にリクエストを転送します。Codex の設定を編集せずに Provider、モデル、コンテキストを切り替えられます。", "axisAria": "Codex CLI からローカル Vellum Proxy を経由して ChatGPT、Grok、またはカスタムエンドポイントへ", "client": "あなたのクライアント", "local": "ローカル — 転送されません", "answering": "回答する側", "noConfigTitle": "設定ファイルを編集しない", "noConfigBody": "起動時に Vellum が Codex のエンドポイントを引き受け、停止時に元の設定へ戻します。手動編集や中途半端な設定は不要です。", "noTruncateTitle": "強制的な切り捨てなし", "noTruncateBody": "しきい値に近づくと、目標、完了した作業、次の手順をチェックポイントにして、履歴を切らずに続行します。", "reviewTitle": "別のモデルで承認を確認", "reviewBody": "以前は y/n の確認が必要だった操作を、別のモデルに評価させられます。サンドボックス、ネットワーク、ファイル権限は変わりません。", "privacy": "すべてこのコンピューター内で処理されます。Vellum にサーバーはなく、会話が私たちを経由することもありません。認証情報はローカルで暗号化されます。" },
    "connect": { "title": "Provider を接続", "lead": "はじめに必要な Provider は 1 つだけです。後から追加することも、複数を同時に有効にしておくこともできます。セッションごとに別の Provider を使え、上限に近い Provider が先に表示されます。", "chatgptClaim": "既存のサブスクリプションを使用", "chatgptDetail": "公式のデバイスコードフローでサインインします。API key は不要です。サブスクリプションの Reset クレジットはモデルページで利用できます。", "grokClaim": "公式 CLI のログインを使用", "grokDetail": "このコンピューターに Grok CLI が必要です。ログイン後は複数のアカウントを連携でき、残量は個別に集計されます。", "grokUnavailable": "Grok CLI が未導入か、現在検出できません。", "customName": "カスタムエンドポイント", "customClaim": "OpenAI 互換サービス", "customDetail": "エンドポイントと API key を入力すると、Vellum が利用可能なモデルとコンテキスト長を検出します。自前運用、Proxy、第三者サービスもこの経路を使います。", "codexToolProtocolUnavailable": "エンドポイントには接続できますが、Codex が実行できる構造化ツール呼び出しを返すモデルがありません。vLLM、Ollama、または llama.cpp でツール呼び出しを有効にしてください。", "credentialNote": "認証情報はシステムのキーチェーンで暗号化してローカルに保存します。Codex 設定やログには書き込みません。" },
    "features": { "title": "すでに利用できる機能", "lead": "追加設定なしで利用できます。この幕では機能の場所と、後で変更する場所を案内します。", "reviewTerm": "自動レビュー\nモデルを選べる", "settingsPage": "設定", "reviewBody": "Codex の承認リクエストを指定した Provider とモデルで評価し、フォールバックも設定できます。フォールバック使用時は明示されます。", "resetTerm": "Reset クレジット\nここで使用", "modelsPage": "モデル", "resetBody": "ChatGPT アカウントの Reset クレジットはアカウントごとに表示され、使用後すぐに更新されます。", "sessionTerm": "各ウィンドウ\n個別に計算", "todayPage": "現況", "sessionBody": "各 Codex ウィンドウは独自の会話とコンテキストを持ちます。現況ページでは平均値ではなく、圧縮しきい値に最も近いセッションを優先表示します。" },
    "launch": { "title": "Codex を接続", "runningTitle": "Vellum を使用中", "lead": "起動すると Vellum が Codex のエンドポイント設定を引き受け、停止時に元の状態へ戻します。いつでも元に戻せます。", "runningLead": "エンドポイントは管理され、リクエストは接続済みの Provider に転送されます。停止すると Vellum 導入前の設定に戻ります。", "startProxy": "Proxy を起動", "noProviderHint": "まだ Provider が接続されていません。起動はできますが、接続するまでモデルは回答できません。", "connectedProviders": "接続済み Provider", "howToStart": "開始方法", "howToStartValue": "新しい Codex ウィンドウを開きます。既存のウィンドウは、Vellum を使うには再起動が必要です。", "howToStop": "停止方法", "howToStopValue": "現況ページで「Proxy を停止して Codex を復元」を選びます。", "reopenGuide": "このガイドを再表示", "reopenGuideValue": "設定ページ" }
  }
};

export default ja;
