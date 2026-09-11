# Remote Manager 開發迴圈

Remote Manager 的驗收路徑（`docs/remote-acceptance.md`、`docs/remote-clean-host-acceptance.md`）
要求完整的 release bundle 與一台真實 Linux 主機。那是驗收該有的樣子，但拿它當開發
迴圈太貴：改一行 Agent 程式碼，要重跑兩個架構的 Rust build、重下載約 480 MB 的
pinned Codex、重建兩個 proxy image、打包 NSIS、安裝、再部署。

這份文件描述的是**開發時**的迴圈。它跑的是同一份 Desktop 程式碼、同一套 SSH 傳輸、
同一支 Agent binary，換掉的只有兩件事:目標主機是可丟棄的容器,以及只重建真的
變了的東西。

## 為什麼原本那麼慢

`scripts/build-local-release.ps1` 每次都會做這些事,而且對兩個架構各做一次:

| 步驟 | 改 Remote Manager 時會變嗎 |
|---|---|
| buildx 編 Agent + Broker | **會** |
| 下載 pinned Codex(約 240 MB/arch) | 不會,版本是釘死的 |
| buildx 建 proxy image 並匯出 tar | 只有改 proxy 才會 |
| 產生 manifest、把雜湊烤進執行檔 | 便宜 |
| Tauri NSIS 打包 + 安裝 | 開發時根本不需要 |

其中 arm64 在 amd64 機器上是走 QEMU 模擬,是整個流程最慢的一段 —— 而如果你這輪
是對著一台 amd64 主機測,那一段是**純粹的浪費**。

還有一件一直存在但沒被利用的事實:`pinned_install::release_resource_roots()` 的
最後一個候選是 `CARGO_MANIFEST_DIR/resources/remote`。也就是說 **`cargo test` 和
`pnpm tauri dev` 本來就直接讀原始碼樹裡的 payload,不需要 NSIS,也不需要安裝。**

## 四個層級

| 層級 | 指令 | 證明什麼 | 代價 |
|---|---|---|---|
| 0 單元 | `cargo test -p vellum`、`pnpm run test:build-remote` | 邏輯、payload staging 的負向測試、以及**遠端與桌面的 route 對齊**(見下節) | 秒 |
| 1 快車道 | `pnpm run remote:e2e` | 探測、host key 確認、Agent/Broker 安裝、linger、boundary key、Agent RPC —— 全部走真 SSH | 見下方實測 |
| 2 全車道 | `pnpm run remote:e2e:full` | 再加上 manifest 驗證、pinned Codex 安裝與驗證、**完整 deployment apply、第二次 apply 的 no-op 判定、以及 boundary key 的修復路徑** | 較久,但 Codex 與 proxy 走快取 |
| 3 真實機 | `crates/vellum-remote-testkit` 的 live 套件 | provider 真流量、detach/resume、跨模型接續 | 需要主機與額度 |
| 4 發版 | `pnpm run build:main` | 完整 bundle 與安裝檔 | 最久 |

層級 1 和 2 是這次新增的。層級 3、4 沒有被取代 —— 它們證明的事情不一樣。

這台機器上的實測(Docker Desktop 29.7.2 / linux-amd64):

| 動作 | 時間 |
|---|---|
| 假主機 image 首次建置 | 約 40 秒 |
| 假主機開機到就緒(每次 reset) | 約 10 秒 |
| Agent + Broker 冷建(快取全空) | 2 分 17 秒 |
| Agent + Broker 增量(改一個 crate) | 58 秒 |
| 快車道兩個案例實際執行 | 14.7 秒 |
| 全車道三個案例實際執行(含推 250 MB Codex) | 88.6 秒 |
| 全車道五個案例實際執行(再加上完整 deployment 與金鑰修復) | 66.7 秒 |
| **全車道端到端(Codex 冷下載)** | **7 分 18 秒** |
| **全車道端到端(快取都熱)** | **4 分 58 秒** |
| **全車道端到端(快取熱、Desktop 重編、五個案例)** | **6 分 52 秒** |

Desktop 本身要不要重編是最大的變數:manifest 一變 `build.rs` 就會重烤雜湊,lib
跟著重編,那大約兩到三分鐘。快車道不碰 manifest,所以沒改 Rust 程式碼時它就是
上面前幾項相加。

## 遠端跟桌面對不對得起來

部署不會把 Desktop 的 `RuntimeModelRoute` 送過去。`plan()` 把每條 route 投影成
`RuntimeRouteConfig`、序列化成 TOML,遠端 proxy 再用 `to_route()` 把它重建回
`RuntimeModelRoute`。中間有三段手寫轉換,每一段都可能默默掉東西而編譯照過:

1. `plan()` 裡逐欄位複製 —— `RuntimeRouteConfig` 沒有的欄位就是沒被抄;
2. TOML 來回 —— `#[serde(default)]` 把「沒有這個 key」變成一個看起來合理的值,
   而不是一個錯誤;
3. `to_route()` —— 有些欄位是從 `base_url` 跟 `upstream_model` **重新推導**的,
   不是讀 Desktop 已經算好的結果。

所以「遠端與本機共用同一組 runtime configuration surface」這件事沒辦法用讀型別
確認。`src-tauri/src/remote/deployment_desktop_parity.rs` 用值來確認:建一組
route 矩陣、分別問 Desktop 與 plan 各自會跑出什麼、再把兩邊序列化成 JSON 逐欄位
比對 —— 用 JSON 而不是列欄位,是為了讓**之後新增的欄位自動被比到**,不必有人記
得回來加。

刻意要不一樣的欄位放在該檔的 `INTENDED_DIVERGENCES`,每個都附理由(目前是
compaction policy 與 Official 的 credential 指標)。除此之外的差異一律是 bug。

這一層跑在單元測試裡,幾秒鐘。全車道再從真主機那一頭確認同一件事:第二次 plan 的
`configChanged=false` 代表主機**真的**持久化、正規化、雜湊出了 Desktop 算出來的
那份設定 —— 同一個比對,但中間隔著真的 agent、真的檔案、真的容器。

## 那台假主機

Remote Manager 在部署前會確認主機提供三件事:Docker、user systemd、以及開啟的
lingering。一般容器一件都沒有,這就是為什麼開發迴圈以前非得借一台真機不可。

`deploy/dev-host/Dockerfile` 三件都給:systemd 當 PID 1、**自己的 dockerd**
(不是宿主的 socket,所以 proxy 發布的埠會落在這台主機自己的 network namespace)、
以及一個斷線後 session 仍存活的登入使用者。

```bash
pnpm run dev:host:up       # 建 image、開機、寫 ~/.ssh/config、等就緒
pnpm run dev:host:status   # uname / docker / userSystemd / linger / agent
pnpm run dev:host:reset    # 丟掉,換一台全新的
pnpm run dev:host:down
```

`up` 會在 `~/.ssh/config` **最前面**寫入一段有標記的 `Host vellum-dev-host`。
之所以直接寫進去而不是用 `Include`,是因為 Vellum 自己的主機探索是把那個檔案當
純文字掃 `Host` 行,不會跟進 `Include` —— 用 `Include` 的話 `ssh` 通得了,
Remote Manager 卻看不見。要移除就刪掉那段標記之間的內容。

驗證過的狀態:

```text
uname=x86_64  docker=29.8.0  userSystemd=running  linger=yes  agent=absent
```

`docker=29.8.0` 與宿主 Docker Desktop 的 29.7.2 是不同版本,這正好證明它跑的是
自己的 daemon,而不是借宿主的。

`/var/lib/docker` 與 `/var/lib/containerd` 各掛一個匿名 volume。這不是為了效能,
是巢狀 dockerd 能不能**跑起東西**的前提:這個容器自己的 root 是 overlay 掛載,
而 overlayfs 不能疊在 overlayfs 上。放在 image layer 裡的話,`docker run` 會失敗
在 `failed to mount ... fstype: overlay ... invalid argument`。兩個路徑都掛,是
因為快照落在哪一個取決於 daemon 用的是 containerd image store 還是傳統
graph driver。官方 dind image 為 `/var/lib/docker` 宣告 volume 就是同一個理由。

用匿名而不是具名 volume,是因為它們必須跟容器同生共死 —— 否則 reset 出來的新主機
會拿到上一台的 image,那就不是乾淨主機了。所以 `docker rm` 一律帶 `-v`。

注意兩件事:它需要 `--privileged`(systemd 與巢狀 dockerd 的前提),而且它是測試
夾具,不釘 digest。它不是會出貨的東西。

## 快車道

```bash
pnpm run remote:e2e
```

做四件事:用 buildx 編 amd64 的 Agent 與 Broker、把它們透過 `VELLUM_REMOTE_AGENT_AMD64`
/ `VELLUM_REMOTE_BROKER_AMD64` 這個既有的 seam 交給 bootstrap、開一台全新的假主機、
然後跑 `src-tauri/tests/remote_dev_host.rs`。

這條路徑完全不碰 manifest —— `bootstrap()` 對 Agent/Broker 是自己算檔案的 sha256
再跟遠端比對,不查 manifest(只有 Codex 走 `pinned_install`)。所以改 Agent 程式碼
到看見它跑在真主機上,中間沒有任何 payload staging。

測試斷言的是:主機是 Linux/amd64、Agent 裝上了、Docker/user systemd/linger 三項
為真、除了 `nativeCodexUnavailable:` 以外沒有其他 blocked reason、boundary key
有被 provision。第二個案例再跑一次 bootstrap,斷言 digest 已相符時**不會**重裝。

測試會先確認 `vellum-dev-host` 解析到 loopback,不是的話直接拒跑 —— 這些案例會
安裝軟體並開啟 lingering,不能因為一個過期的 alias 就改到別人的機器。

### SSH host key 的信任

第一次跑會撞到 `SshHostKeyNotConfirmed`,這不是接線問題,是 Vellum 自己的 host key
信任閘門在運作:它有一份自己的 known_hosts,沒有人確認過指紋就不連。產品裡是人
看過指紋按確認;測試裡由測試扮演那個人。

有兩個細節值得知道:

1. **會寫兩個地方。** `bootstrap` 查的是 `AppState` 的 data root,而
   `RemoteAgentClient` 走 `ssh_trust::runtime_data_root()`,也就是真的 app data
   目錄。所以跑測試會在你自己的信任庫留下一筆 `127.0.0.1:22222`。這是不去假造
   傳輸層的誠實代價;loopback 的斷言把它的範圍限死了。
2. **每次都重新確認,不吃舊的。** reset 過的主機是新的 key,如果沿用上一台容器
   留下的確認,那正好就是這個閘門存在要防的過期信任。所以測試每次都重新
   keyscan、重新確認。副作用是你的 known_hosts 每 reset 一次會多一行 —— 只會是
   loopback 的 dev host。

## 全車道

```bash
pnpm run remote:e2e:full
```

先跑 `scripts/stage-remote-dev.ps1 -Arch amd64`,再 `cargo build` 讓 `build.rs`
重新把 manifest 雜湊烤進去,然後連 pinned Codex 安裝與**整套 deployment**一起測。

### 部署那一段測的是什麼

`remote_deployment_converges_and_a_second_apply_changes_nothing` 走完整條
plan → apply → 再 plan → 再 apply,斷言的是 `docs/remote-acceptance.md` 第 9 節
與第 10.4 節本來只靠人看畫面確認的那幾件事:

- 第一次 apply 走到 `credentials.boundaryReady`、`proxy.configured`、
  `proxy.ready`、`nativeAdopt.applied`、`nativeDaemon.restarted`,狀態
  `nativeActive`;
- apply 後以主機上的 pinned Codex 完成 `initialize` / `initialized` 握手並呼叫
  `model/list`,確認每個 selected catalog id 真的出現在 Codex 回覆中。這會抓到
  「檔案與 hash 都正確,但單一 entry 缺少 required field,導致 Codex 拒絕整份目錄
  並退回內建模型」的 regression;
- `observedRevision` 追上這次的 `desiredRevision`;
- 第二次 plan 的 `configChanged`、`catalogChanged`、`restartRequired` 全部為
  false,且 `planHash` 與第一次相同(否則「Reapply desired state」認不出這是同一
  份 desired config);
- 第二次 apply 只出現 `proxy.alreadyReady` 與 `nativeAdopt.alreadyActive`,
  **不得**出現 `proxy.stoppedForConfiguration`、`proxy.configured` 或
  `nativeDaemon.restarted` —— 重啟一個沒有 drift 的 native daemon 不只是浪費,
  它會把主機正在扛的 session 丟掉;
- 收尾再確認一次 acceptance 第 2 節的控制面(proxy running/ready、
  `daemonOwner=codexCliDaemon`、`durable`、`restartSafe`),證明 no-op 那條路徑
  沒有默默拆掉什麼。

這個案例把 seeded 的 Official 與 Grok route 關掉,自己建一條指向
`https://dev-host-lane.invalid/v1` 的 OpenAI-compatible route。部署是設定操作不是
請求 —— apply 寫設定、送 credential、起 proxy、adopt native daemon,從頭到尾不會
對 `base_url` 送任何 token,所以一個連不通的 URL 部署起來跟真的一模一樣,而且不
可能花掉任何人的額度。

`credentials_accept_selected_official_control_account_without_an_override` 另在 proxy
啟動前透過遠端 Agent 寫入 `credential_id = "official-selected"` 的 schema 2 設定,
再由 `host.status` 斷言 `credentialRefs` 保留該 selector 且 `credentialsReady=true`。
這個 selector 代表沿用 Codex control account,本來就不應存在同名 secret file。

### 金鑰修復那一段測的是什麼

`repairs_a_legacy_lease_whose_boundary_key_was_never_confirmed` 蓋的是 acceptance
第 11 節:主機的 native lease 已經 active,但**這台 Desktop 從來沒有確認過**它的
boundary key。那不是造出來的狀態,那是 Desktop 重裝、從備份還原、或 lease 早於
boundary-key provisioning 時看起來的樣子。

前置狀態也不是靠改主機造的:用一個全新的 Desktop data root,指向前一個案例留下的
那台(lease active、daemon 在跑)。新的 root 有自己的 per-host key、沒有 sync
marker,所以 provisioning 必須推一把新金鑰並重啟 daemon 才能送達 —— 那正是會踩到
問題的那條路。案例本身會先斷言前置狀態真的成立(lease 是 `active`、daemon 在跑、
兩個 consumer 都尚未確認),否則它會為了錯的理由通過。

斷言的是第 11 節說「不應該要求使用者做」的那些事都沒有發生:重試就是全部的補救,
而且**租約 ID 與 `CODEX_HOME` 在修復前後相同** —— 那是「沒有刪 lease、沒有重建
`~/.codex`」的機器可讀證據,如果修復是靠砍掉重來,這兩個值一定會變,使用者既有的
session 也會跟著沒了。最後再跑一次 provisioning,斷言金鑰**沒有被旋轉**、兩個
consumer 都**沒有被重啟**。

第 4 項(經 Codex App 發遠端請求並 resume)需要 provider 流量,第 5 項(provisioning
失敗時必須停在 `RemoteBoundaryKeyProvisionFailed`)需要一台不可達的主機。兩者假主機
都給不了,仍在 `docs/remote-acceptance.md`。

staging 的快取規則:

- **Agent / Broker** —— `deploy/remote-bundle/Dockerfile` 現在帶 cargo registry
  與 target 的 cache mount,依 target platform 分開。改一個 crate 不會重編整棵
  依賴樹。(release 路徑用的是同一個 Dockerfile,所以它也跟著變快。)
- **Codex** —— 依版本與 target triple 快取在 `target/remote-dev-cache/codex/`。
  釘死的東西下載一次就好。
- **proxy image** —— 依 `Get-VellumProxySourceFingerprint` 快取,和 release image
  tag 用的是同一個指紋。沒動 proxy 就不重建;動了就一定重建。
- **另一個架構** —— 沿用已經 staged 的檔案,並且**重新計算雜湊**,所以 manifest
  不會宣稱磁碟上沒有的位元組。完全沒 staged 過會直接報錯,不會偷偷放行。

產出的 payload 的 `releaseVersion` 帶 `+dev.<fingerprint>` 後綴。這不是裝飾:
安裝檔建置會跑 `Assert-VellumStagedRemotePayload`,而它比對的是確切版本字串,
所以**一個 dev payload 不可能混進 release**。同一個後綴也會顯示在 Remote Manager
的 Release trust 上,所以畫面上看得出來現在裝的是什麼。

### 一定要從乾淨主機開始

`remote-e2e.ps1` 每次都會先 reset 假主機,這不是保守,是必要的:每個測試案例自己
開一個丟棄式的 Desktop data root,而 boundary key 是「一個 data root ↔ 一台主機」
的配對。如果主機上還跑著一個綁定到已經消失的 data root 的 Codex daemon,下一次
就會停在 `BoundaryNativeRestartIdentityMissing: daemon identity unavailable`。

那是設計在運作,不是 flake。直接 `cargo test` 之前先跑 `pnpm run dev:host:reset`。

## 手動用 UI 迭代

假主機在 `~/.ssh/config` 裡之後,`pnpm tauri dev` 起的 Desktop 會像看待任何一台
主機一樣看見它,而且直接讀原始碼樹裡的 payload。要重來就 `pnpm run dev:host:reset`,
幾秒鐘就有一台全新的乾淨主機 —— 這是 clean-host 路徑唯一需要的東西。

## 這條迴圈證明不了什麼

誠實地說:

- **provider 真流量**。假主機不會消耗任何額度,也就不會驗證任何 provider 行為。
  那是層級 3。
- **arm64**。快車道與預設 staging 都是 amd64。Jetson 的驗證仍然要真的建 arm64。
- **detach/resume 的完整語意**。假主機有 user systemd 與 linger,transient unit
  跑得起來,但 Codex App 的原生 detach/resume 驗收仍在 `docs/remote-acceptance.md`。
- **安裝檔**。NSIS 那段沒有被測到,依然只有發版時才會跑。
