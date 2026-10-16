# 遠端總管簡化與部署成功率計畫（工單）

> 協調者：Claude。執行者：Codex。分支：`codex/remote-one-click`（worktree
> `E:\code_E\vellum-worktrees\remote-one-click`，從 `origin/codex/dev` c2b8158 切出）。
> Rust target 共用 `E:\code_E\vellum-target`，不要放 C 槽。

## 0. 問題

遠端總管現在是給維運人員用的控制台，不是給一般使用者的功能。

**配置繁瑣。** 一台新主機要成功，使用者得自己理解並依序處理：SSH 指紋確認、
部署、規劃（勾模型）、套用、ChatGPT 帳號逐一配對、遙控身分綁定、Official
執行帳號、Grok 登入、Install Codex、Update Agent、Sync Desktop Codex、
Restart native、Repair。`Remote.tsx` 上有十幾顆動作按鈕、七張卡片。

**部署會卡死。** `one_click_bootstrap` 是遇到 blocker 就整個 `Err` 結束，而下列
blocker 在 `remoteVocabulary.ts` 裡 `remedy: null`，使用者只能看到一段英文修復指令：

| Blocker | 現況 | 根因 |
|---|---|---|
| `dockerUnavailable` | Linux 必須有 Docker 且使用者在 docker 群組 | Linux 走 Docker proxy；macOS 早已改用原生 `vellum-proxy-daemon` |
| `systemdUserUnavailable` / `lingerEnableFailed` | 擋死 | 只支援 systemd user＋linger 一種常駐方式 |
| `proxyPortConflict` | 擋死 | 固定 15721／15722 |
| `nativeDaemonAppOwned` | 要使用者自己去 Codex App 斷線再重按 | 沒有「等它閒置後接手」的流程 |
| `activeTurnInProgress` | 直接失敗 | 沒有等待重試 |
| `insufficientDiskSpace` | 擋死 | Docker image archive 很大 |
| `noModelsSelected` | 要先去規劃勾模型 | 沒有預設選擇 |
| `officialAccountPairingRequired` | 部署前必須配對 | ChatGPT 配對被放在部署的關鍵路徑上 |
| `versionMismatch` / `nativeCodexVersionMismatch` / `CodexDesktopProtocolMismatch` | 跳出另一顆按鈕要使用者按 | 修復動作沒有收進一鍵流程 |
| glibc 版本 | 舊發行版上 Agent 直接起不來 | Linux artifact 是 `*-linux-gnu` |

另外 `one_click_clean_host_bootstrap` 遇到既有 Vellum 足跡就拒絕（`CleanHostRequired`），
那是驗收閘門，不該是使用者路徑。

## 1. 目標與「100%」的定義

部署成功率 100% 的可驗證定義：

1. **支援矩陣內的主機，一鍵部署必成功。** 矩陣見 §5，每一列都要在 CI／本機
   自動化中真的跑過。
2. **矩陣外的主機，在動到主機之前就被前置檢查擋下**，且給出一句人話＋一個
   可行動的下一步。不允許「裝到一半才失敗」。
3. **任何中斷都可重入。** 部署中途斷線、Desktop 關閉、主機重開，再按一次同一顆
   按鈕必定收斂到成功或回到 §1.2 的明確拒絕；不需要手動清理、不需要 Restore。
4. **每次部署留下一筆本機紀錄**（host、平台、每個 phase 結果、失敗碼），用來
   計算真實成功率。只存本機，不上傳。

使用者面的目標：**新主機從「選主機」到「可在 Codex App 用」只需按一顆按鈕，
外加最多一次 SSH 指紋確認。** 其他東西全部變成部署後的選配。

## 2. 設計原則

- **部署只有一個動詞。** 「部署／重新部署」= 冪等收斂（converge）。Plan／Apply／
  Repair／Install Codex／Update Agent／Sync Desktop Codex／Restart native 都是
  converge 內部的步驟，不再是使用者要排順序的按鈕。
- **先檢查、後動手。** 新增純讀取的 `preflight`，一次回報所有問題；能自動修的
  列入計畫自動修，不能修的才擋。
- **能降級就不擋。** 沒 Docker → 原生 proxy；沒 linger → 不常駐模式；port 被占
  → 換 port；沒配對 ChatGPT → 先用其他已有憑證的 provider 部署。
- **安全邊界不放寬。** loopback-only、secret 只進遠端 0600、host key 必須被信任、
  不殺 App 擁有的進行中 turn、manifest／digest fail-closed —— 全部維持。

## 3. 工作項目

依序做，每個里程碑一個（或數個）commit，各自能編過、測過。

### M1 Linux 改用原生 proxy，移除 Docker 依賴（成功率最大的一塊）

- `vellum-proxy-daemon` 加 Linux 建置：`x86_64-unknown-linux-musl`、
  `aarch64-unknown-linux-musl`（靜態連結，消除 glibc 版本問題）。Agent 一起改 musl。
  `vellum-remote-broker` 是 diagnostic-only，Linux bundle 可直接不再打包。
- Agent `native_proxy.rs` 目前只有 `LaunchdClient`。抽出 service trait，新增
  `SystemdUserClient`（產生 `~/.config/systemd/user/vellum-proxy.service`，
  `systemctl --user daemon-reload/enable --now/status`）。
- `RemotePlatform::proxy_backend()` 的 Linux 改回 `Native`；`docker_is_blocker()`
  恆 false。Docker backend 保留為讀取／遷移用：已用 Docker 部署的主機，converge
  時停掉受管 container（只停有 Vellum label 的）、改起原生 proxy，設定與憑證沿用。
- `build-local-release.ps1/.sh`、manifest（升 schema 5）、`finalize-remote-*-manifest`
  同步更新；Linux 不再需要 Proxy image archive。
- `requires_agent_protocol` 照 macOS 的做法升版，舊 Agent 由 converge 自動更新（M3）。

### M2 常駐方式與 port 的降級

- **調查結論與已核准的支援條件**：精確 pinned tag
  `rust-v0.147.0-alpha.6.6` 指向 commit
  `15a7ef83c47fc1d92532d1ed44ad267e71703617`。該版本的 daemon 已是 PID
  backend：`bootstrap_locked` 直接啟動 `pid_backend`，程序以 `setsid()` 脫離
  terminal，stdin/stdout 接 null、stderr 寫 log，不依賴 systemd user。
  但它的 `read_process_start_time` 必須呼叫 `ps -p <pid> -o lstart=`；開始時間
  讀取失敗會終止剛啟動的 daemon，不能用 PID-only 略過。
  Alpine 預設 BusyBox `ps` 不支援此介面，僅新增 onDemand supervisor 無法解決。
  相容的 `ps`（例如 procps）是所有 Linux 主機的前置條件；linger／login／onDemand
  都使用相同的 pinned Codex PID backend，必須在部署前探測，不受 systemd user
  是否可用影響。純 BusyBox 或完全沒有 `ps` 的主機，由唯讀 preflight 在 SSH
  isolation、Agent 安裝等部署寫入前以 `psStartTimeUnavailable` 拒絕，附發行版
  安裝指令（Alpine：`apk add procps-ng`；Debian／Ubuntu：`apt-get install procps`），
  不自動安裝系統套件。
  協調者已在 `alpine:3.20` 確認 BusyBox 的探測失敗，安裝 `procps-ng` 後成功；
  Codex 本輪不執行建置、測試或實機驗證。
  來源：[pinned daemon lifecycle](https://github.com/openai/codex/blob/15a7ef83c47fc1d92532d1ed44ad267e71703617/codex-rs/app-server-daemon/src/lib.rs#L587)、
  [pinned PID backend](https://github.com/openai/codex/blob/15a7ef83c47fc1d92532d1ed44ad267e71703617/codex-rs/app-server-daemon/src/backend/pid.rs#L126)、
  [開始時間探測](https://github.com/openai/codex/blob/15a7ef83c47fc1d92532d1ed44ad267e71703617/codex-rs/app-server-daemon/src/backend/pid.rs#L701)、
  [BusyBox ps](https://busybox.net/downloads/BusyBox.html#ps)、
  [Alpine BusyBox 設定](https://github.com/alpinelinux/aports/blob/master/main/busybox/busyboxconfig)。
- 常駐方式依序嘗試：systemd user＋linger → systemd user（無 linger，登入期間常駐）
  → 無 systemd：Agent 內建的 supervisor（pidfile＋`setsid`，SSH 連入時由
  `ssh_launcher` 確認存活並拉起）。結果以 `persistenceScope` 回報
  （`linger`／`login`／`onDemand`），UI 用一句話說明「主機重開機後需要重新連線
  一次」之類的實際影響，不擋部署。
- 先查清楚：Codex 自己的 `app-server daemon` 在沒有 systemd user 時怎麼常駐。
  若它本身做不到，`onDemand` 模式下的 durable 判定要跟著調整，不能謊報 detachedReady。
- 實作分段：所有 Linux 的 `psStartTimeUnavailable` preflight 已於 `8a26b04` 修正。
  常駐降級段落加入 Agent `supervise`／`ensure-services`：onDemand 的 proxy 與 supervisor
  都有私有、原子寫入的 PID record，比對 UID、執行檔、argv、啟動時間與 boot ID；
  SSH 重連只恢復已啟用的服務。停止與 supervisor tick 共用 host mutation lock，避免
  明確停止後被復活。Codex 只在受管 lease 啟用且沒有執行中的 daemon 時，沿用既有
  pinned daemon bootstrap／ownership 檢查恢復；不接管 App-owned 程序。
  `detachedReady` 在 onDemand 下另外要求 `supervisorReady=true`。這段待協調者驗證，
  不代表已完成 Linux 實機驗收。`5edc38b` 已把恢復 hook 接到 Desktop 正式產生的
  wrapper，修正 POSIX 路徑；SSH 重連的兩把鎖都用 try-lock，鎖忙立即跳過。
- Proxy port：預設 port 被非受管程序占用時，自動在 15721–15799 內挑一個空的，
  寫入 desired state，catalog／config 的 base_url 跟著走。`proxyPortConflict`
  只在整段範圍都被占時才出現。
- M2 最後段落（待協調者驗證）：Agent 新增唯讀 `proxy.selectPort`；已在執行的受管
  proxy 保留埠，否則先試 desired／既有／平台預設埠，再掃描 15721–15799。
  plan 記錄 `hostPort` 與 desired state 的 `proxyPort`，並用該埠產生 config.listen。
  start 再探測一次，處理 plan 後埠被占用的情況；實際埠與 config hash 寫回 Agent，
  Desktop 把實際埠／hash 保存為 observed desired state。Codex provider 的 base_url
  沿用實際 ProxyStatus 產生；catalog 的上游網址不因 host-local 埠改變而重寫。
  wrapper 上傳也改成同目錄 mktemp → chmod 0700 → mv -f，避免截斷執行中的腳本。
  M2 的程式碼範圍已齊備；建置、測試與最後驗收由協調者執行，實機 smoke 仍待 Joe 同意。

### M3 一鍵 converge 吸收所有修復動作

實作分段（第一段 0188cd7 已驗收，非 M3 全部完成）：重開機後 installed／desired port
被外來程序占用時，Agent status 回 `proxyPortOccupied`，inventory 將該碼傳到 Desktop，
UI 提供重新部署 remedy。`proxyPortConflict` 仍僅代表整段候選範圍全滿。每次 converge
的 plan 都以 desired port 作為偏好重新探測，不能把已保存的 port 當作仍可用的證據；
fake 測試涵蓋舊埠被占用後重選、persisted listen/hash 與 Codex provider URL 一起更新。
下列 converge 狀態機仍待後續分段完成；broker／Docker 死碼留 M5。

第二段（26c2b29 已驗收）：Docker → native 遷移在停止舊容器前，於 secrets 目錄保存
私有回滾紀錄（舊 install、config、credential bytes、這次停止的容器 ID）。Agent
mutation 失敗及 Desktop apply 後續步驟失敗時恢復；native 驗證成功才清除紀錄。
只恢復本次停止、且 Vellum label、host ID、install ID 仍一致的容器；不碰外來或
原本已停止的容器。恢復失敗保留紀錄供重試，回報明確錯誤，不宣稱服務已恢復。
舊 Agent 以 feature flag 相容；尚無補償能力的 Agent 必須先更新才能遷移 Docker。
失敗注入涵蓋 service write/load、Docker restart、跨程序重試與 ownership 改變；
支援資料排除私有快照。

第三段（feb7787 與 b836c4d 已驗收）：
apply 完成後，首次 verify 失敗時只做一次冪等 proxy.start，
再 verify 一次；不重跑安裝、憑證同步、adopt 或 daemon restart。start 回覆遺失也會
再讀狀態，以實際 readiness 決定結果。第二次仍失敗即回報 verification failure，
UI 提供重新部署。驗證必須維持 proxy ready、native daemon running、detachedReady
及 apply 後的相同 proxy port；若 retry 期間 port 改變，不能宣稱 catalog 已同步，
需重新 converge 收斂。

第四段（af0771d 與 047066d 已驗收）：一鍵 converge 使用獨立的可用模型範圍，預設選擇啟用且
通過 qualification、Desktop 有可用憑證的模型；手動 plan 保留原本嚴格驗證。
沿用主機的 policy overrides。Official 只使用 Desktop 目前帳號已啟用的遠端 grant；
若其他模型可用，略過未就緒 Official 並在成功結果提示配對。只剩 Official 時，
提供目前帳號的單次配對／啟用 remedy，完成後續部署，不走全部帳號佇列。
暫缺憑證的選擇保存為 deferredCatalogIds，plan/apply 都保留，補憑證後再 converge
即可納入；舊 desired state 與儲存的 plan 仍可讀。尚無可部署模型時回報已知碼
noDeployableModels。preflight／autoFix／等待與取消／接手確認見後續分段；
模型／憑證的前置檢查已於下一段加入寫入關卡。

第五段（d03ea3f 已驗收）：bootstrap 拆成 inspect／write，以共同關卡拒絕有 blockers
的部署。先探測平台與 procps，再查 Desktop 模型憑證、signed bundle 的 agent／proxy／
Codex 檔案與 digest、磁碟保留。已是 bundled Agent 時，於寫入前觀察 port、Official
grant、config 與完整閒置狀態；Agent 需更新或尚未安裝時，先更新 Agent，再於 boundary／
linger／Codex lifecycle 寫入前重查上述項目。回報 autoFix 清單，Codex 版本或相容性
不符時自動安裝 pinned 版本；proxy／config／boundary 沿用既有收斂階段。fake transport
涵蓋全新主機被拒時沒有本次部署寫入呼叫，既有 recovery hook 保持不變。
active work／App-owned 的等待與取消、接手確認見第六段。

第六段（待協調者驗證）：一鍵部署遇到 active turn／工具／核准或觀察不完整時，
維持 waitingForIdle 並每兩秒觀察，可取消；App-owned 閒置後進入 awaitingTakeover，
使用者一次確認後沿用 stopAppOwned 的 ownership／觀察檢查。確認 token 綁定目前
operation、host、PID、daemon identity 與 CODEX_HOME；程序或活動改變就撤銷確認。
Agent 以 additive optional expectedDaemonPid／expectedDaemonIdentity 再核對一次，
舊手動 stop RPC 仍相容；自動接手需 confirmedTakeoverIdentity feature flag。
取消會在下一個檢查點停止，不中斷在途 RPC，也不刪除已完成的安裝；Vellum 重啟後
舊確認失效，原 operation 轉為可重試的中斷狀態。同主機同時只允許一筆一鍵部署。
補齊 inspect_bootstrap fake probe 的真實拒絕路徑測試；repair 改按 blocker 代碼對應，
多 blocker 不再把平台說明當成 ps 指令。並修正 Codex 版本欄位為 codexVersion，
避免已是 pinned 版本的主機每次都重裝。M3 程式碼範圍齊備，待本段建置測試驗收；
實機 smoke 仍須 Joe 同意，由協調者執行。broker／Docker 死碼維持留到 M5。

改寫 `one_click_bootstrap` 成一個狀態機（保留函式名以免破壞呼叫端）：

1. `preflight`（純讀）：SSH 可達、host key 已信任、OS/arch、磁碟、glibc 不再相關、
   常駐能力、port、Codex daemon 擁有者、進行中 turn、Desktop 有沒有至少一個可用
   provider 憑證、bundled release ready。產出 `{autoFix[], blockers[]}`。
   preflight 通過前，**本次部署不寫入任何東西**；既有受管安裝在 SSH 連入時觸發的
   `ensure-services` 冪等恢復不在此限，保留原本 recovery hook，不另設跳過路徑。
2. 自動修：Agent 版本不符 → 更新 Agent；Codex 版本／Desktop 協議不符 → 安裝／同步
   pinned Codex；proxy image／binary 過期 → 換；boundary key 缺 → provision；
   config `upgradeRequired`／`repairRequired` → 照現有邏輯收斂。
3. `activeTurnInProgress` / `nativeDaemonAppOwned`：不直接失敗。進入
   `waitingForIdle` phase，輪詢到 App 擁有的 app-server 沒有 active turn 後，
   **在使用者一次確認後**才接手（沿用現有 `stopAppOwned` 的安全檢查）。等待可取消；
   UI 顯示「等 Codex 目前這輪結束」而不是錯誤。
4. 模型選擇：desired state 沒有選擇時，預設 = Desktop 目前啟用且通過 qualification
   的模型。不再因 `noModelsSelected` 擋。
5. 憑證：只部署「Desktop 已有憑證」的 provider。ChatGPT 配對移出關鍵路徑 ——
   沒配對就不注入 Official 模型，部署照樣成功，結果頁提示「要在這台用 ChatGPT
   帳號？配對」。只有當 Desktop 上**唯一**可用的就是 ChatGPT 時，才在流程中
   插入一次配對（只配 Desktop 目前的帳號，不是全部）。
6. plan → apply → verify，verify 失敗自動重試一次（只重試 verify 與 proxy start，
   不重跑安裝）。
7. 每個 phase 的冪等性：中途失敗後重按，不能因為「上次做了一半」而走不同的錯誤。
   `CleanHostRequired` 只留在 qualification 入口，UI 永遠走一般入口。

錯誤輸出：每個失敗都要對到 `remoteVocabulary` 的已知碼＋人話＋remedy；
找不到 remedy 的 blocker 要在 §5 的測試裡被列為「矩陣外，preflight 拒絕」。

### M3b 遠端接手（handoff）：把本機做到一半的工作交給遠端 Codex

Joe 於 2026-10-16 核准方向，排在 M3 之後、M4 之前。依據是 handoff POC
（`E:\code_E\vellum-handoff-poc\REPORT.md`）：「交接單＋新 thread」在 Docker 遠端
端對端跑通；transplant 整份 rollout 不冪等，**不做**。

UI 參考渲染稿：`src/dev/handoff-mock.tsx`，開 `/preview/handoff-mock.html`
（只是設計稿、不進 repo，已在 `info/exclude`）。五個狀態：準備交出、前置檢查擋下、
遠端進行中、結果回來、本機在這段期間又改過。

- **位置**：遠端總管的主機工作面底下一張「把本機的工作交給 <主機>」卡，主機由左欄
  決定。主機清單的副標顯示「1 份工作進行中／1 份結果待拿回」。底下一張「交接紀錄」。
- **選 thread**：列出本機最近、cwd 在 git repo 裡的 Codex thread（名稱、repo 路徑、
  分支、時間）。不要用下拉選單。thread 的定位沿用現有對話身分邏輯。
- **帶過去的東西**：顯示改過／已 staged／未追蹤的數量與檔名。快照用臨時 index
  （`GIT_INDEX_FILE`＋`read-tree HEAD`＋`add -A`＋`write-tree`＋`commit-tree`），
  不動使用者的 index 與工作區；經受管 SSH（`ssh_isolation`／`ssh_trust`）推到 agent
  管理的 bare repo，**不經 GitHub**；`.gitignore` 生效。
- **交代**：可編輯，第一段必須是明確的任務；原始需求與最後進度只當背景。原始需求裡
  「先別做／留給別台」這類條款要明說指的就是這次交接（POC 第一輪就是被照字面遵守而
  什麼都沒做）。
- **前置檢查**（`host_probe`／`platform`）：主機就緒與 Codex 版本；工具鏈落差
  （例如只有 `python3`）→ 警告並寫進交代；平台綁死的工作（例如 Windows 建置交給
  Linux）→ 擋下。本機 thread 標成「已交出」，避免兩邊同時寫。
- **遠端執行**：agent 新增 `codex.handoffStart`／`codex.handoffResult`。`handoffStart`
  開 worktree，並透過 daemon 的 control socket 呼叫 `thread/start`＋`turn/start`
  （**不要另起 stdio app-server**，以免搶 `thread-writer-locks`），關掉 Vellum 也會繼續跑。
  sandbox 用 workspace-write，不用 danger-full-access。模型沿用本機 thread 的設定。
- **進行中**：顯示遠端 thread id、plan 步驟、最近的指令輸出；可「在 Codex App 開啟遠端
  thread」、「先拉回目前進度」。
- **結果回來**：顯示相對交出時快照的 diffstat；動作是「套用到本機工作區」「開成分支」
  「看完整 diff」。若本機在交出後又改過同一檔案，偵測到衝突時改以「開成分支」為主要
  動作並警告。本機 thread 收到一則「遠端已完成」的摘要。
- **先查清楚**：POC 發現 0.147 只裝單一執行檔時缺 `codex-code-mode-host`，所有 shell
  指令都會失敗而模型只會說「執行主機不可用」；`desktop_codex.rs:478` 選的正是單一執行檔
  archive。做 M3b 之前先確認並修正。
- 四語 i18n、預覽台加上上述五個狀態；單元測試涵蓋快照不動工作區、衝突偵測、
  前置檢查擋下。

### M4 SSH 信任與加主機

- 指紋確認：若使用者自己的 `~/.ssh/known_hosts` 已有**相同** Ed25519 key，
  視為使用者先前已信任，直接寫入 Vellum 私有 known_hosts，不再跳確認；
  不一致或不存在時維持現行確認流程。（只讀 known_hosts，不寫回、不匯入其他 host。）
- 主機清單空的時候，提供「新增主機」：輸入 `user@host[:port]`，Vellum 寫一筆受管
  SSH alias（沿用 `ssh_isolation` 的 key）。需要密碼登入的情況，用一次互動式
  `ssh-copy-id` 等價流程把 isolation key 放上去 —— 密碼由系統 ssh 的提示處理，
  Vellum 不經手。這項若超出範圍可先做成提示文字，回報給協調者。

### M5 介面簡化（Remote.tsx）

- 工作面只剩：狀態一句話、主要按鈕（部署／重新部署）、進度、失敗時的單一
  remedy。Plan 按鈕拿掉；模型選擇改成部署後「這台可用的模型」清單，可增減並自動
  重新 converge。
- 預設只顯示：主機清單、工作面、「這台可用的模型」、ChatGPT 帳號（只在有
  ChatGPT 模型時）。
- 收進「進階」摺疊（預設關）：遙控身分綁定、Official 執行帳號、Grok、手機配對、
  執行緒明細、主機明細、Revision／Rollback、Export support bundle。
- 維護 Tray 的 Install Codex／Update Agent／Sync Desktop Codex／Restart native／
  Repair 從預設介面移除（converge 會處理）；進階裡保留「重新部署」與 support
  bundle 即可。Restore 留在危險區。
- 四語 i18n（zh-TW／zh-CN／ja／en）同步；遵守 repo 既有排版規則（CJK 不用 `ch`
  做寬度、避免固定寬欄位）。不要加模板式的英雄區／三卡並排。
- 預覽台：`src/dev/remote-preview.tsx` 要能看到 待部署／等待閒置／進行中／失敗／
  就緒（含 onDemand 常駐）五個狀態。

### M6 部署紀錄與成功率

- Desktop 每次 converge 寫一行 JSONL 到 data root 下 `remote-deploy-log/<hostId>.jsonl`：
  時間、build 版本、平台、persistenceScope、每個 phase 的結果與耗時、最終結果、
  失敗碼。不含 secret、不含 hostname 以外的識別資訊。
- support bundle 帶上最近 20 筆。

## 4. 不做／不改

- 不放寬任何 fail-closed：manifest／digest、boundary key、loopback-only、secret 落地方式。
- 不殺有 active turn 的 App 擁有 app-server。
- 不支援 Intel Mac（維持 `intelMacUnsupported`，但要在 preflight 擋）。
- 不改 `-32070` 行為、不碰 Enhanced Runtime。
- Docker backend 程式碼不刪，只降為遷移用途。

## 5. 驗證（「100%」的證據）

1. 單元測試：preflight 每個分支、降級選擇（常駐方式、port）、converge 的冪等性
   （每個 phase 注入一次失敗後重入都會收斂）、模型預設選擇、ChatGPT 不在關鍵路徑。
2. 主機矩陣（自動化，Docker 容器跑 sshd＋systemd，或 `vellum-remote-testkit` 的
   fault injector；寫成可重複執行的腳本，放 `scripts/qa/remote/`）：
   - Ubuntu 22.04 / 24.04、Debian 11 / 12、Fedora 最新、Rocky 8、Alpine（無
     systemd → onDemand）、Arch；x64 與 arm64 至少各一。所有 Linux 列（包括有
     systemd 的最小安裝）都須有相容的 `ps -p <pid> -o lstart=`；另驗證 BusyBox
     與缺少 `ps` 時在部署寫入前拒絕，安裝 procps 後放行。
   - 有／無 Docker、有／無 linger 權限、無 systemd、port 15721 被占、磁碟剩 500 MB、
     既有舊版 Vellum（Docker proxy）升級、部署中途殺掉 SSH 後重按。
   - 每一列：一鍵部署 → `detachedReady`（或 onDemand 的對應狀態）→ proxy `/readyz`
     → 一次 provider 請求 → 重按部署為 no-op。
   - 矩陣外：Intel Mac、32-bit、非 Linux/macOS → preflight 拒絕且主機上沒有任何新檔案。
3. 實機：Jetson（既有 `live_jetson`）與 Mac mini 各跑一次完整一鍵部署＋從舊版升級。
   需要動實機時先停下來問協調者，不要自行對實機做破壞性操作。
4. 既有測試全綠：`cargo test`（workspace）、`pnpm test`、clippy、`pnpm typecheck`。
   main 上原本就紅的測試請列出來，不要順手改。注意：#50 之前的分支跑 Rust 測試
   會清掉真實的 HKCU `CODEX_CLI_PATH`；本分支已在其後，但若 log 出現
   `cleared under lease` 請回報。

## 6. 交付

- 每個里程碑一個 PR 到 `codex/dev`（或一個 PR 多個乾淨 commit，二擇一，先問）。
- commit 訊息英文，**不要**加 `Co-Authored-By: Claude` 之類的 trailer。
- PR 描述附上 §5 矩陣的實跑結果表（每列 PASS/FAIL＋紀錄路徑）。
- 卡住、需要實機、需要改安全邊界、或發現本計畫的假設不成立（例如 Codex daemon
  沒有 systemd 時根本無法常駐）時，停下來用中文回報，不要自行繞過。
