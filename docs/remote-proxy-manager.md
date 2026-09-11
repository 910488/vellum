# Remote Manager → Codex App 原生遠端專案

> Current architecture reference. Normative proxy behavior is defined in
> [protocol-source-of-truth.md](protocol-source-of-truth.md); dated machine
> results live in [remote-acceptance.md](remote-acceptance.md).

Remote Manager 不另做聊天介面。production 路徑是：

```text
Codex App → SSH → Codex native app-server daemon
                     ↓
              127.0.0.1 Vellum Proxy
                     ↓
             Official / OpenCode / Grok / other providers
```

Codex 原生 daemon 是 thread、turn、approval、workspace 與 session 的唯一 authority。`vellum-remote-broker` 僅保留 legacy/diagnostic 相容用途，不位於 production data path。

## Remote Manager 能做什麼

1. 匯入 Codex App「連線」與 OpenSSH config 中已設定的 SSH host；同時支援 Codex 舊版 `alias` 與新版 `hostname: user@host` connection snapshot。
2. 探測 Linux/CPU architecture、Docker mode、user systemd、linger、Codex CLI/standalone/native daemon ownership。
3. 以 digest 驗證方式 bootstrap ARM64/AMD64 Agent、Codex standalone 與 proxy image。
4. 在 Desktop 端產生不含 secret 的 deployment plan，顯示 model、credential requirement、config/catalog hash、drift、restart requirement 與 blocker。
5. Apply 時依序安裝並啟動 loopback-only proxy、注入 credential、套用 config/catalog、以 three-way lease adopt `~/.codex` 的受管欄位，再由 Codex CLI daemon 接管 durable app-server。
6. 讓 Codex App 的原生遠端專案直接選用 Vellum catalog 中的模型；context、tool、compaction、Auto Review、usage 與 continuation 都經共享 proxy runtime。
7. Detach 只中斷 Desktop control connection，不停止 remote proxy、Codex daemon 或正在執行的 user-systemd job；重新連線後仍使用原生 Codex session ID。
8. Reconnect snapshot 必須保留 Codex 的 typed turn/item timeline（reasoning、tool call/result、agent message、時間戳與 duration）；不能只留下最後結果。
9. Restore 只還原 Vellum 擁有的欄位；非受管使用者修改保留，受管欄位衝突則 fail closed。

## 安全邊界

- Proxy 只 publish `127.0.0.1:15721`，不對 LAN 公開。
- Proxy container 不掛載 Docker socket、`~/.codex` 或任意 workspace。
- Agent 不接受 renderer 傳來的任意 shell command。
- SSH host key 必須先顯示指紋並由使用者明確確認，之後只寫入 Vellum 私有的 `known_hosts`。Windows 若偵測到標準位置的 Git for Windows，優先使用其相容版 `ssh-keyscan`，並以系統 OpenSSH 作後備；兩者都只讀取遠端公開 host key，不會匯入或自動信任使用者既有的 `known_hosts`。
- Secret 只存在 remote `0600` credential file，不進 renderer、log、container label 或 CLI argument。
- 退役 route 由 deployment planner 無條件排除；環境清理紀錄不得寫入此架構文件。
- Image 更換時即使 config hash 無 drift，也必須重新套用 desired config，避免 bootstrap mock config 取代正式 catalog。

## 每個 release 必須重跑的 promotion gates

下列項目是 release qualification requirements，不是未完成的架構 backlog。e806 是遠端 OpenAI-compatible API，不是 Jetson 本機 Ollama：

1. e806 transport qualification（marker、SSE、typed tool、continuation、credential isolation）；
2. per-model capability probe（尤其 reasoning/tool/vision/context），未通過 typed-tool probe 的模型只標 chat-only，不進 Codex catalog；
3. 只有「qualified 且使用者勾選」的模型注入該 host 的 catalog；
4. Remote deployment 的 public plan 與 runtime config 不含任何 secret；
5. Provider promotion gates：Luna／DeepSeek／Grok 各跑 HumanEval/0-2、`switch-matrix-6`（三模型 ordered pair 與 round-trip）與 0.2.3 SWE gate `psf__requests-1142`；Verified-12 其餘 11 題為 post-release qualification pending。e806 只標 `API transport qualified`／`Codex protocol qualified`。

逐項判定與實機步驟見 [remote-acceptance.md](remote-acceptance.md) 的 Gate 1–4。

## 現行 inventory、pinned 安裝與 per-host desired state

- `host.inventoryV2`（agent RPC）：單一呼叫回傳 system（os/arch/hostname/CPU/memory/disk）、docker（version/daemon/context/permission）、codex（binary/version/source/CODEX_HOME/app-server compatibility）、proxy 狀態、host-level blockers（`dockerUnavailable`／`codexBinaryMissing`／`proxyNotReady`）與 available actions（`installCodex`／`repair`／`exportSupportBundle`／`updateComponents`）。Desktop `inspect_remote_host` 合併進 `RemoteHostStatus.inventory`；舊 agent 不支援時靜默 fallback。
- 本機建置腳本產生單一 schema v3 manifest，連同 Linux amd64/arm64 Agent、Broker、Codex 與 Proxy image archive 一起包入 NSIS。manifest SHA-256 在編譯時嵌入 executable；runtime manifest 不符即 fail-closed。Desktop 依遠端 arch 選擇 bundle artifact、驗證 SHA-256，再經 SSH stdin staging；agent 端二次驗證後原子安裝 Codex，Proxy archive 則以 allow-listed `proxy.loadImage` 執行 `docker load`。
- RPC：`codex.installPinned`／`codex.updatePinned`（不自動降版、不覆寫 Codex App 擁有的 active daemon）／`codex.verifyInstallation`／`services.reconcile`。legacy bootstrap 不再使用未驗證 `curl | sh`：未配置 pinned artifact URL＋digest 時 fail-closed。
- per-host desired state（`remote-desired-state/<hostId>.json`）：`desiredRevision`／`observedRevision`／`lastPlanId`／`selectedCatalogIds`／`configHash`／`catalogHash`；plan 時 revision 遞增，apply 成功後 `observedRevision` 更新；`planHash` 為 selection＋config/catalog hash 的 content fingerprint。UI 提供「Reapply desired state」（以 persisted selection 重新 plan）與 Revision／Rollback 展示。
- UI 維運動作：Bootstrap、Plan/Apply、Restart native、Detach、Restore、Repair、Export support bundle、Install Codex、Update Agent，以及 Sync Desktop Codex runtime。Desktop 以內附 Codex core 的完整 prerelease 版本與 generated app-server schema hash 作為協議 identity；Remote Manager 只接受同版 `openai/codex` 官方 Linux artifact 與 GitHub publisher SHA-256，經新版 Agent staged probe 後原子替換並重啟。UI 顯示 bundled release readiness、Desktop/remote compatibility、host/inventory blockers 與 native app-server thread/active-turn 明細；任何 digest、架構、Agent protocol 或 probe 驗證未通過時都 fail-closed。

## 維運注意事項

- 安裝包必須包含 `manifest.json` 與所有架構 artifact；manifest 必須等於 executable 內嵌 hash，且所有 artifact digest 必須吻合。runtime 環境變數不能替換 manifest。任一資源缺失或 schema/digest 不合法時，Install Codex、Update Agent 與新機 Bootstrap 都 fail-closed，不會觸碰遠端。
- Update Agent 只替換遠端 agent binary（原子安裝＋rollback），下一個 RPC 由全新 process 回應；broker 仍為 diagnostic-only，proxy image 於下一次 deployment apply 切換，UI 不再把三者誤稱為一次更新。
- Sync Desktop Codex runtime 只更新 host-native Codex；不重建或替換 Proxy image。若 Desktop core identity 改變或操作回報 `CodexDesktopProtocolMismatch`，Remote Manager 會提示同步，而非原樣重試；舊 Agent 不具最新版 staged catalog probe（含 `shell_type` 等 required fields）時必須先 Update Agent。
- Restore 只還原 Vellum 管理欄位；衝突欄位不覆寫。Detach 不停止遠端 daemon／Proxy／turn／session。
