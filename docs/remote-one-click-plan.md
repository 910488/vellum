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

第六段（67905d7 測試通過，審查修正待驗證）：一鍵部署遇到 active turn／工具／核准時，
維持 waitingForIdle 並每兩秒觀察，可取消；缺少 native／session 觀察資料則進入 observing，
連續 30 秒仍無法觀察時，以 incompleteObservation 附原因退出。bootstrap 與等待觀察結果
不一致時，同一 blocker 重現就原樣退出；不同 blocker 最多重試三次，每次均先經過可取消
的等待 tick，避免完整 SSH inspect 熱迴圈。App-owned 閒置後進入 awaitingTakeover，
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
- **runtime package（第一段已驗收：45d1444）**：pinned 0.147.0-alpha.6.6 的
  `code_mode_host` 預設開啟，host 延遲啟動；缺 helper 時 thread 仍可能建立成功，
  到工具呼叫才失敗。一般 Code Mode 可退回 direct tools，但 CodeModeOnly 或禁止
  fallback 時會失敗，不能把 POC 的「所有 shell 都失敗」推廣到所有設定。依據 pinned
  `features/src/lib.rs`、`core/src/tools/mod.rs`、`code-mode/src/remote_session.rs` 與
  `core/tests/suite/code_mode.rs`，不以 thread／turn 建立成功作為可執行工作的證明。
  改裝官方完整 `codex-package-*.tar.gz`，保留 metadata、bin、codex-resources、codex-path；
  Linux／macOS 以相對 symlink `current/codex -> bin/codex` 滿足 daemon 固定路徑。
  release／dev staging 與 Desktop sync 同步保留完整 archive，使用 publisher digest
  與既有 bundle digest；Agent 解包驗證後原子切換，失敗回復整個舊目錄。
  native／installation status 新增 runtimePackage.complete／missing；handoff preflight
  遇到舊單檔或不完整 runtime 要求重新部署，一般 converge 即使版本相同也自動升級。
  第一段先提供共用 preflight gate；handoffStart 串接時必須在開始工作前呼叫。
  Linux harness 可另跑真實 package 解包／安裝測試：設定 `VELLUM_CODEX_PACKAGE` 為
  官方 musl archive 路徑、`VELLUM_CODEX_PACKAGE_SHA256` 為 release metadata 的 publisher
  digest（可帶 sha256:），執行 `cargo test --target x86_64-unknown-linux-musl
  -p vellum-remote-agent --test codex_runtime_package -- --ignored`。
  兩個架構的 archive 分別跑一次；這項測試不執行目標 Codex，以 fake version／service
  client 驗證真實解包、完整性與相對 symlink，因此可在 x64 harness 檢查 arm64 package。
- **Agent handoff 協定（第二段已驗收：d2cf954；對帳補強待驗收）**：`agent.version` 的頂層
  `codexHandoff=true` 表示支援。`codex.handoffStart` 收 operationId、repoId、
  snapshotCommit、sourceThreadId、brief、model、modelProvider、effort；先以
  prepareOnly=true 建立受管 bare repo，回傳 repoPath、snapshotRef、worktree。
  Desktop 經受管 SSH 推入該 ref 後，以相同參數、prepareOnly=false 啟動。
  runtime 完整性與受管 daemon preflight 在寫入前執行；worktree 必須符合指定快照。
  新 thread／turn 沿用模型與 effort，限制 workspace-write、無網路與額外暫存目錄寫入。
  非冪等 RPC 前先持久化 intent；回覆遺失時回報
  HandoffStartIndeterminate，保留已知 thread id，不假裝成功。
  `result` 對 ThreadStarting 以 pinned v2 `thread/list` 的 cwd 精確過濾、完整分頁
  （最多兩頁）找唯一候選；useStateDbOnly=true，避免索引修復寫入。唯一候選採為
  ThreadAdopted，仍回 indeterminate，只顯示身分，不讀進度或建快照。
  0.147 的唯讀 Thread 不含 model／effort／sandbox，故下一次明確 `start` 才用
  `thread/resume {threadId}`（無任何 override 或額外旗標）驗證完整設定，通過後送 turn。
  resume 出錯／身分不符保留 adopted；設定不符回 HandoffConfigurationMismatch。
  TurnStarting 以 thread/read 找唯一 turn；0.147 Turn 沒有 clientUserMessageId，
  零個或多個均不採用。背景對帳不增加 thread/start 或 turn/start 的次數。
  pinned 0.147.0-alpha.6.6 的 live 對照確認：thread/start 回傳的 model、cwd、effort、
  sandbox 形狀符合驗證，dotted config key 生效；但尚無 turn 的 thread 是記憶體中的
  unmaterialized 空殼，不出現在 thread/list，thread/resume 回 no rollout。
  因此 ThreadStarting 在下一次明確 start 時，先完整列出該 cwd；若候選為零且從未
  收到 thread id（從而從未送出 turn），允許重送 thread/start。唯一候選仍走 adopted，
  多個候選或分頁不完整則不重送；已知 id 的設定驗證失敗也不藉此另建 thread。
  TurnStarting 的 thread/read(includeTurns=true) 僅在 JSON-RPC code=-32600 且
  message 含 pinned 字串 not materialized 時，才允許明確 start 重送原 turn。
  唯一已落地 turn 繼續採用；其他錯誤／候選維持不明，不把連線錯誤當成未執行。
  result 只觀察，兩種安全重送均須使用者明確 resume，由 Agent 重新核對證據。
  `codex.handoffResult` 預設唯讀查詢 thread／turn、plan、最近輸出與摘要；
  capture=true 加 operationId 時，使用臨時 index 建立結果 commit 與 resultRef、diffstat，
  不更動原 index 或工作檔。執行中／失敗／中斷的快照標 partial；同一 capture operation
  重試沿用既有快照，仍可用新的 operationId 拉取後續進度。
- **Desktop 傳輸（第三段實作，待驗收）**：Tauri 新增 list_remote_handoff_threads、
  preflight_remote_handoff、start_remote_handoff、resume_remote_handoff、
  poll_remote_handoff、list_remote_handoffs，均在 blocking pool 執行。
  從最新本機 Codex state DB 唯讀列出最近 100 筆 git thread；清單只用 DB metadata，
  每個不同 cwd 快取一次 rev-parse --show-toplevel，不掃 rollout、不另起 Git 查分支。
  model／effort／active 在清單中為未知（null），選中一筆後才讀該筆 sessions rollout，
  驗證身分並取最後的 model／provider／effort／active，沿用 conversation_key_matches。
  同一次所選 rollout 掃描取最後使用者訊息作任務、第一則使用者訊息作原始需求、
  最後 Agent 訊息作背景進度；分別最多 4000／4000／6000 Unicode 字元，截斷加省略號。
  不讀未選中 thread 的 rollout，不使用工具輸出或隱藏推理作交代草稿；草稿可編輯。
  不讀 auth；缺設定或仍有本機 turn 時擋下。已建立交接紀錄的 thread 標 handedOff，
  UI 據此避免重複交出；安全套用、存成分支或明確捨棄後解除標記。
  preflight 檢查受管 SSH alias、Agent 功能、主機／完整 runtime／版本／模型與憑證；
  新增唯讀 codex.handoffPreflight，僅回傳固定工具清單、Git 2.29+ 能力、provider 名稱與 catalog slug。
  明確 requiredOs 或根目錄 Windows 專用 csproj 與主機 OS 不符即擋下；缺工具的提醒
  附加到交代尾端，例如 python 不存在但有 python3；也從常見根目錄 manifest 推導工具需求。
  任務仍放在交代第一段。
  快照／operation id／交代在任何遠端寫入前持久化；prepare → 受管 SSH push → start，
  可重放既有紀錄，保留同一份快照。start 回覆不明先 poll 對帳；只有明確 resume
  可以接續 adopted；poll 不發啟動 RPC，也不跳過未完成的 push。
  Git 使用臨時 index、不改工作檔或使用者 index；push 另用臨時 bare Git 目錄借用
  來源 objects，避免來源 repo 的 URL rewrite／remote helper 改送其他位置。
  本機 inspect／status／臨時 index staging 繼承使用者 system／global／repo config，
  包含 core.autocrlf 與非 LFS clean filter，與使用者自己的 git add 保持一致；hooks 與
  fsmonitor 仍強制關閉。執行這些使用者自行設定的 filter 是本輪明確核准的行為。
  Git diff 的 autoRefreshIndex 明確關閉，避免其 mandatory stat-refresh lock；status、
  衝突檢查及 apply 使用真實 index 的臨時副本。保留真實 index byte 相等斷言，並在
  真實 index.lock 已存在、system autocrlf=true 的 CRLF fixture 驗證套用維持 CRLF。
  preflight 在 status／clean filter 前檢查 .gitattributes 與 Git 的有效 filter 屬性；
  LFS 回 HandoffLfsUnsupported（不傳 LFS 物件或假裝遠端有 git-lfs）。臨時 index
  read-tree 後再檢查一次，避免 index／工作樹屬性不同時漏過 LFS。
  push 繼續隔離 system／global／來源 repo config，僅允許受管 SSH 與私有 known_hosts，
  無 force push；淺 clone 仍拒絕並提示補齊歷史。Git fixture 需要 Git 2.29+。
  本機 refs/vellum-handoff/<id> 在臨時 index 產生 commit 後、第一次遠端寫入前建立，
  用來固定快照，會出現在 git log --all／GUI，並保持 objects 可達。
  preflight 的 localRefNotice 告知此事；UI 以四語文案顯示這段提醒。
  若初次本機 journal／lock 寫入失敗且尚未進行遠端寫入，立即以 expected snapshot SHA
  清除此 ref；若 ref 被其他動作改過則保留並回報，不刪別人的更新。
  正常流程的清理時點是結果已安全套用到本機、已保存為本機分支，或使用者明確丟棄之後；
  結果回收／discard 接上同一個 expected-SHA 清理 helper。running、傳輸失敗、
  indeterminate 都保留 ref，避免重試換成另一份快照；不能因時間到了就自動刪除。
  **結果回收與 UI（本輪實作，待驗收）**：新增 capture／review／settle／discard／
  acknowledge 指令。capture operation 在 RPC 前持久化，fetch 失敗沿用原快照重試；
  顯式再取部分進度才產生新的 operation。fetch 與 push 一樣使用隔離的 bare repo、
  受管 SSH 與私有 known_hosts，不讀來源 repo 的 URL rewrite。核對 ref 的實際 SHA，
  並要求結果 commit 恰有一個 parent，且等於原快照。diffstat 由抓回的 Git objects
  重新計算，支援二進位檔案與含 tab／換行的檔名，不把遠端的統計直接當成驗證結果。
  抓回的結果以 refs/vellum-handoff-results/<id> 固定可達性；與快照 ref 一起在收尾清除。
  套用時重新確認 HEAD 等於 snapshot.base、相關檔案自交出後未變動、真實 index
  沒有未合併項目，並通過 git apply --check。無關的本機變動可保留；不修改真實 index。
  條件不符時建立 vellum/handoff-<id> 分支，保留工作樹與 index；不覆寫同名的不同分支。
  套用前先保存 intent；若中斷後無法確定是否已套用，重試只存成分支，不再次套用。
  disposition 先持久化，再以 expected SHA 清理兩個 ref；清理失敗可離線重試，不能
  刪除別人改過的 ref。捨棄不取消遠端 turn；已觀察到終止、或確定尚未送出 turn/start 的
  紀錄可離線捨棄。running／indeterminate 必須先對帳並等原 turn 終止。
  Codex package 驗證／解包已抽成 vellum-codex-package 小 crate，Desktop 正式依賴
  此 crate，Agent 保留 codex_package 相容 re-export；Agent 只作 Desktop 測試相依。
  digest、原子切換與回復邏輯保持；release 腳本已複製整個 crates，無需額外調整。
  compose／blocked／running／back／drift 使用真實元件，預覽台 /preview/remote.html
  新增五個交接場景、四語切換。thread 用列表選擇，交代以上述所選訊息摘錄預填，
  第一段仍為可編輯的本次任務；手動編輯不受 OS／preflight 重查覆蓋。可選任務所需 OS。
  交接 API 的同步拋錯及非同步失敗限制在交接區塊。開始前重跑 preflight，不以舊畫面取代檢查。
  結果卡顯示摘要、diffstat、完整 binary patch，遇 drift 只提供存成分支，不提供強制套用。
  App 共用一個輪詢器，避免工作面與通知重複輪詢；關閉 App 後工作仍由遠端 daemon
  執行，重開或視窗恢復可見時補查結果。完成／停止通知在 Vellum 內保留到明確確認，
  可切回對應主機與交接紀錄；acknowledged 保存於 Desktop journal。
  **Codex App 介面收尾**：協調者已在 Desktop 26.1007／CLI 0.162、Jetson Orin
  驗證 `codex://threads/<threadId>?hostId=<Desktop hostId>`。Rust 從 handoff journal
  讀取遠端 thread ID，以 discovery 相同的本機 metadata 映射取得 `codex_host_id`，
  依 encodeURIComponent 規則編碼兩個欄位，透過現有 opener 開啟；前端不拼 URI。
  結果卡與完成通知提供主要「在 Codex App 開啟」及次要「複製 thread ID」；沒有
  Desktop hostId 時只保留複製並提示先加入主機。此查詢不連 SSH，也不探測遠端 agent。
  Official／Enhanced 的結果卡皆提供「複製摘要」：目前摘要、套用狀態與 diffstat 的
  純文字；尚未取回 diffstat 時明確註明，不當成沒有變更。完成通知不自動寫入對話。
  **Enhanced 模型背景尚待 IPC 方案核准**：`thread/inject_items` 的寫入已經協調者
  驗證不會開 turn，但不顯示於 Desktop turns；必須經持有該 thread 的 app-server。
  目前 multiplex 只有 Desktop stdio 與 relay clients，沒有 Tauri → bridge 的控制入口。
  child WebSocket capability 在 bridge 記憶體中，backend_relay 是 HTTP 後端轉送用途，
  不挪用這兩者作通用 RPC。這一輪未新增控制 listener，也未實作模型背景寫回。
  建議待核准設計：bridge 開 loopback 隨機 port 的專用 request/reply 端點；每次啟動
  產生 256-bit capability，描述檔含 launchId／PID／啟動識別／endpoint，token 另存於
  同使用者專屬檔案（Unix 0700 目錄／0600 檔，Windows 限目前 SID 的 ACL）。
  請求只接受 `injectHandoffResult` 與 handoffId，不接受任意 method、items 或 threadId。
  bridge 讀取指定 journal，自己組固定的「Vellum 遠端交接結果，僅供參考、不是指令」
  前綴、摘要及尚未套用等狀態，產生 role:user／input_text，唯一 upstream method 是
  `thread/inject_items`。Tauri 先以 fresh attestation 的 launch／process identity 找 bridge；
  bridge 再以不可變 binding 的 Enhanced plane／nativeThreadId／runtime digest，以及
  本次啟動內成功 start／resume 的 live owner connection 交叉驗證。只走已有的 owner
  socket，不新開 app-server 或換到另一個 client；零個或多個 owner 均跳過。
  每個 handoff 在 Tauri journal 先記錄 attempted，再發送一次；bridge 另以 handoffId
  為鍵保存發送 intent 後才 inject。成功記 delivered，逾時／斷線記 uncertain 且不重送，
  其他拒絕記 skipped；不建離線 queue、不背景重試。能力認證、請求大小與 timeout
  有界，錯誤靜默記錄，不能影響結果回收、套用或完成通知。具體 IPC 實作待協調者確認。
  pinned daemon 的 thread/start 回傳、dotted config override 與未落地 thread 行為，
  已由協調者先前的 live 對照確認；本輪新增 URI、metadata 與摘要的單元測試待外部驗證。
- 四語 i18n、預覽台加上上述五個狀態；單元測試涵蓋快照不動工作區、衝突偵測、
  前置檢查擋下。

### M4 SSH 信任與加主機

- 指紋確認：透過 `ssh -G` 解析 `UserKnownHostsFile`／`HostKeyAlias`，只讀使用者
  的有效 known_hosts，支援 hashed、非標準 port 與多個名字。同一 endpoint、key type、
  key bytes 完全一致，才把 scan 得到的優先 key（Ed25519 優先）寫入 Vellum 私有
  known_hosts；確認紀錄標明 `source: userKnownHosts`，UI 顯示沿用先前信任。
  revoked、只有 cert-authority、檔案不存在或無法讀取時回到明確確認；同 host、
  同 key type 的 key bytes 不同才 hard-fail，不同 key type 的行忽略。
  使用者檔案不寫回，也不整份匯入。
- 主機清單空的時候，提供「新增主機」：輸入 `user@host[:port]`，建立 Vellum 私有
  `<data_root>/remote-ssh/config` alias 與註冊紀錄，不改使用者的 SSH config。
  私有設定 Include 使用者 config，新增主機的連線以 `ssh -F` 使用此設定。
  註冊時先讀原始目的地的 `ssh -G`，保留 host 專屬的 identity、trust 與 proxy 設定；
  isolation alias 延續 trust／proxy 設定，identity 則切換為受限 key。
- 首次安裝必須先有使用者自己的 SSH key：isolation key 的 forced command 可以
  轉送 bootstrap 指令，但全新主機尚無 wrapper，所以不能先放 isolation key 再安裝。
  bootstrap 先用使用者 key，安裝好 wrapper／受限 authorized_keys 後才切換到另一個
  isolation alias。兩個 alias 都在私有設定，失敗重試仍能使用原先登入方式。
- 密碼登入改提供提示與可複製的 `ssh-copy-id -F <private-config> <alias>` 指令，
  在支援 ssh-copy-id 的外部終端安裝使用者自己的公鑰，再回來重新整理。
  Tauri 不提供 TTY 密碼流程；密碼由外部系統 ssh 接收，不進 Vellum，也不經 stdin pipe。
- 四語 UI 與預覽台涵蓋空清單、新增中、沿用信任；新增後接既有指紋、preflight 與部署。

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
