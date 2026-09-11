# Remote Manager 實機驗收方法

本文是目前 `main` 上 Remote Manager 的可重現驗收規格。所有 PASS 都必須有 machine-verifiable artifact；模型最後一句話不能代替 grader。文中的日期與 PASS/FAIL 是當次執行證據，不代表後續 build 自動通過。

## 1. 前置條件與固定測試矩陣

| Lane | Route/model | 目的 |
|---|---|---|
| Native GPT experience | OpenCode Go / `gpt-5.6-luna` | Codex App 原生 tool/session、detach、resume |
| Third-party | OpenCode Go / `deepseek-v4-flash` | Chat wire、reasoning、跨模型 handoff |
| e806 transport | `api.provider.example`（route `cc`） | 只驗證 API transport、SSE、typed tools、continuation、credential isolation；不跑能力 benchmark |
| Public dataset | OpenAI HumanEval/0 | hidden grader correctness |
| Extended gate | SWE-bench Verified selection | repository-level repair；需要 pinned bundle/image |

固定排除 route：`weikuwu`。任何 plan、catalog、credential requirement、container config 或 `/v1/models` 出現它都直接 FAIL。

固定排除於能力比較：e806 因模型能力不足，不參加 HumanEval、SWE-bench 或跨 Provider 能力比較；HumanEval、SWE-bench、`switch-matrix-6` 只跑 Luna、DeepSeek、Grok。

## 2. Control/data plane 健康檢查

```bash
vellum-remote-agent status
curl -fsS http://127.0.0.1:15721/readyz
curl -fsS http://127.0.0.1:15721/v1/models
```

通過條件：

- `dockerAvailable=true`、`userSystemdAvailable=true`、`lingerEnabled=true`；
- `nativeCodex.daemonOwner=codexCliDaemon`、`durable=true`、`restartSafe=true`；
- proxy `running=true`、`ready=true`，host port 15721；
- managed lease `active`，native runtime ready，broker 不成為 session authority；
- model/credential set 不含 `weikuwu`；
- `readyz.model_count` 與 `/v1/models` 相同。

## 3. Provider real-traffic smoke

對 Jetson loopback proxy 送非 streaming Responses request：

```json
{"model":"<catalog-id>","input":"Reply with exactly <MARKER>","stream":false}
```

必測：

- `vlm-5f033ca0b5-gpt-5-6-luna` → `VELLUM_LUNA_OK`；
- `vlm-1e832f2f4d-deepseek-v4-flash` → `VELLUM_DEEPSEEK_OK`；
- Grok catalog route → 固定 marker；
- 至少一個 NVIDIA route → 固定 marker。

HTTP 2xx、normalized Responses envelope、非零 usage 與完全相等 marker 才算 PASS。此 gate 曾找出並修復所有 upstream JSON POST 缺少 `Content-Type: application/json` 的問題。

## 4. API 層跨 provider continuation

1. Luna 建立含隨機 nonce 的 response，保存 response `id`。
2. DeepSeek request 使用 `previous_response_id=<luna-id>`，prompt 不重送 nonce，只要求取回前一輪 nonce。
3. Luna 再以 `previous_response_id=<deepseek-id>` 接續，仍只要求同一 nonce。

兩個方向都完全相等才通過。這驗證 Vellum durable history hydration，而不是 client 手動拼接 prompt。

## 5. Codex App 原生 detach/resume（最高優先）

使用 pinned HumanEval materializer 建立乾淨 workspace，不先把 hidden grader 放進 workspace：

```powershell
cargo run -p vellum --bin vellum-eval -- prepare --suite compaction-replay-gate --dataset-root ..\public_datasets
python evals\tools\materialize.py --dataset-root ..\public_datasets --kind humaneval --task-id HumanEval/0 --output target\remote-acceptance\humaneval-0
```

在 Jetson 以 user-systemd transient service 執行原生 `codex exec --json --model <luna-catalog-id>`，工作目錄指向該 workspace，JSONL 寫入 acceptance artifact directory。`systemd-run` 回傳後啟動 SSH 已結束。

執行中精確終止 Windows 上命令列含 `gpu-dev ... codex app-server proxy` 的 SSH process，不能依名稱大量終止。斷線後另開 SSH 驗證：

- transient unit 仍 `active/running`；
- `MainPID` 未變；
- JSONL 行數或最後事件繼續前進；
- proxy 與 native daemon 仍 ready。

turn 完成後才複製 hidden grader，執行：

```bash
VELLUM_EVAL_WORKSPACE=<workspace> python3 -I <hidden>/grader.py
```

必須輸出 `VELLUM_EVAL_PASS`。接著使用 JSONL 的 `thread_id`：

```bash
codex exec resume --json --model <luna-catalog-id> <thread-id> '<resume prompt>'
codex exec resume --json --model <deepseek-catalog-id> <same-thread-id> '<handoff prompt>'
```

通過條件：兩次 `thread.started.thread_id` 都等於原 ID；Luna resume 保存既有解答；DeepSeek handoff 看見並保存 Luna 建立的檔案、完成 focused verification，且 hidden grader仍 PASS。

已知警告：Luna 的 WebSocket 首次回 400，Codex 原生核心自動 fallback 至 HTTPS 後完成。整體 turn/resume PASS，但 WebSocket-native lane 應標為 recoverable warning，不能記為全綠。

## 6. e806 transport qualification（Gate 1）

e806 只跑接線與協定測試，不跑能力 benchmark。執行順序：

1. 經 Vellum probe 呼叫 `/v1/models`，保存脫敏 response shape；確認 `qwen3.6` upstream model 仍存在，不存在則停止，不自動換模型。
2. 非串流 marker request：`{"model":"<e806-catalog-id>","input":"Reply with exactly <VELLUM_E806_OK>","stream":false}`，必須回 HTTP 2xx、normalized Responses envelope、非零 usage 與完全相等 marker。
3. SSE marker request，必須收到合法 terminal event（`response.completed` 或 `[DONE]`），不得以 failed frame 或靜默斷流結束。
4. required typed-tool request 與 tool-result continuation；typed tools 未通過時該模型只標 chat-only，不得進入 Codex model picker。
5. 驗證 usage、`reasoning`／`reasoning_content` normalization、401/429/5xx 錯誤映射與 credential isolation（Authorization 不出現在 log、trace、journal、deployment plan JSON 或 fixture）。
6. 從 Desktop 產生 remote deployment plan，只選通過 probe 且使用者勾選的模型；public plan JSON 不含 token/Authorization/config secret。
7. Apply 至 Jetson：config/catalog hash 收斂；第二次相同 apply 為 no-op（`configChanged=false`、`catalogChanged=false`、`restartRequired=false`）。
8. 經 Jetson loopback Vellum Proxy 重跑 marker、stream 與 tool tests；remote `/v1/models` 只暴露已選取 catalog model。
9. 經 Codex App 遠端專案做一個輕量檔案工具任務及同模型 resume。

通過條件：transport、SSE、typed tools、continuation、credential isolation 全部通過。結果只能標記 `API transport qualified` 或 `Codex protocol qualified`，不得以 HumanEval/SWE 宣稱模型能力。

## 7. SWE-bench gate

0.2.3 的 release SWE gate 是單題 `swe-terminal-gate` / `psf__requests-1142`。通過條件是 official hidden grader（unpatched baseline FAIL、gold patch PASS），不是 agent 文字回覆。gold patch 與 hidden tests 不得進 agent workspace。

成品固定在不可覆寫的 GitHub Release tag
`swebench-grader-psf-requests-1142-a3ba3a231e3c`：image archive
`a3ba3a231e3c7a48f8410b5d7942911763bfa2ad2295b7cb6d79032a099f7055`、
config digest `sha256:64ac3483a165…`、bundle
`073c9c6b4957da5aaad21f24d1e5666da00a1e707eeb6e848536a0d9f635d929`。
Active suite 使用 `graderImageArchive`，不再引用 GHCR。

Verified-12 其餘 11 題是 post-release qualification pending，不得寫成 PASS，也不得為了發布產生假的 `swe-verified-12.json`。

## 8. 公開資料集與跨 Provider 接續矩陣（Gate 3）

先決條件：Luna、DeepSeek、Grok 各自通過 marker、SSE、typed tools、same-provider resume、Auto Review 與 usage gate。

第一層（HumanEval）：

- Luna、DeepSeek、Grok 各跑 HumanEval/0、1、2，repeat 1，每題使用 hidden grader（`VELLUM_EVAL_PASS`）。
- 任一 Provider 未通過時停止該 Provider 的後續 SWE promotion，但不阻擋其他 Provider 繼續。

第二層（`switch-matrix-6`）：

- 以只含 Luna、DeepSeek、Grok 的三模型 profile 執行；所有 ordered pair 與 round-trip，repeat 1：
  - Luna ↔ DeepSeek；Luna ↔ Grok；DeepSeek ↔ Grok。
- 每次切換必須保持相同 thread、workspace、DECISIONS checkpoint、tool state 與 canonical history。
- 禁止把來源 Provider 的 opaque response ID 或 reasoning object 原樣傳給目標 Provider。
- 每次切換後都執行 hidden grader 與 usage attribution 檢查。

第三層（SWE-bench Verified-12，post-release qualification pending）：

- 0.2.3 只宣告單題 `psf__requests-1142`。其餘 11 題的 canonical image/bundle 與完整 12 題 clean-host qualification 延後，不得寫成 PASS。
- 待其餘 artifact 齊備後，再驗證 pinned dataset revision、parquet SHA-256、每個 bundle SHA-256、repository commit 與 grader image digest，然後 Luna、DeepSeek、Grok 分別跑 Verified-12，repeat 1。
- 使用 official hidden grader，不得以文字判斷或模型自評代替。
- 中斷的 eval run 必須用同一 run ID `--resume`，不能重新計費或重跑已完成 instance。

e806 明確排除於上述三層。

## 9. Final restore/reapply（Gate 4）

本節的 apply/reapply 機械判定（第一次 apply 的步驟序列、`observedRevision` 追上、第二次 apply 為 no-op、不重啟無 drift 的 native daemon）已由 `remote_deployment_converges_and_a_second_apply_changes_nothing` 在 `pnpm run remote:e2e:full` 中自動驗證，見 `docs/remote-dev-loop.md`。那條迴圈跑的是可丟棄的容器主機，不涉及 provider 流量，因此**不取代**本節其餘需要真實 session 與 provider 額度的項目。

- Detach 後確認遠端 session 可繼續（Luna HumanEval/0 完成或中途狀態保持）。
- Restore native Codex 設定，驗證原設定備份可還原；使用者資料與 session journal 不得遺失。
- Reapply 相同 deployment，確認 credentials、catalog、proxy、native daemon 恢復且沒有 drift。
- 再做一次 Luna same-thread resume。
- 第二次相同 apply 必須為 no-op：`configChanged=false`、`catalogChanged=false`、`restartRequired=false`，且不得重啟無 drift 的 native daemon。
- 工作樹只包含預期修改；測試 artifact、token、remote logs 與 dataset bundle 不得進 Git。

## 10. M30–M32 pinned 安裝與 desired state 驗收

這些項目驗證 inventory V2、digest-pinned Codex 安裝與 per-host desired state。

1. `host.inventoryV2` 回傳 system（CPU/memory/disk/hostname）、docker（server/client version、context、permission）、codex source/version/CODEX_HOME、proxy 狀態與 host blockers/actions；desktop `inspect_remote_host` 合併進 `inventory`，舊 agent 不支援時不失敗。
2. 安裝包缺少 `manifest.json` 或任一架構 artifact，或 manifest 與 executable 內嵌 hash、schema、artifact digest 任一驗證失敗時，「Install Codex」與「Update Agent」按鈕 disabled，後端指令也 fail-closed，遠端不受影響。
3. 內建 schema v3 manifest 與 bundle 驗證後：
   - `codex.installPinned`：artifact 下載→SHA-256 驗證（desktop 與 agent 各一次）→原子安裝到 `packages/standalone/current`→版本 probe 對 pinned range→durable user service reconcile；任一步失敗回滾並保留原 install。
   - `codex.verifyInstallation`：回報 present/version/sha256/compatible；與 manifest `compatibleRange` 不符時 `compatible=false`。
   - `codex.updatePinned`：已 pinned 時 idempotent；遠端較新時拒絕降版；Codex App 擁有 active daemon 時拒絕覆寫（需先 detach）。
   - `services.reconcile`：systemd user unit 內容漂移時重寫，未 enable/active 時 enable/start；再次執行為 no-op。
4. per-host desired state：plan 產生新 `desiredRevision`；apply 成功後 `observedRevision` 追上；「Reapply desired state」以 persisted selection 重新 plan，`planHash` 相同代表同一 desired config 已套用。
5. UI：Revision（desired/observed）、Plan hash、Rollback summary、release trust readiness、host/inventory blockers 顯示；Repair／Export support bundle／Install Codex／Update Agent 按鈕依 `availableActions` 出現，且安裝／更新額外受 release readiness fail-closed gate 控制。

6. M33 parity：`plan()` 產出的 remote runtime config 與 local 共用同一組 `proxy_runtime_bridge` builder；`compaction_policy.threshold_percent` 與 `review.on_edit/before_send/before_compact` 在 stored plan 的 `config_toml` 與 local runtime config 一致（`remote_plan_carries_compaction_and_review_policy_into_the_runtime_config` 鎖定）。

   完整的 route parity 由 `src-tauri/src/remote/deployment_desktop_parity.rs` 鎖定：一組 route 矩陣，一邊問 Desktop 的 `runtime_route`、一邊把 stored plan 的 `config_toml` 經 `RuntimeRouteConfig::to_route()` 重建回來，兩邊序列化成 JSON 後**逐欄位**比對，所以往後新增的欄位會自動進入比對，不依賴有人記得補測試。刻意的差異只有兩個，且必須在 `INTENDED_DIVERGENCES` 具名並附理由：remote compaction policy（遠端主機自己是 compactor）、Official 的 `credential_id` 指向 `SELECTED_OFFICIAL_CREDENTIAL_ID`。此測試已找出兩個原本會靜默發生的落差：`insecure_http_policy`（Desktop 授與的明文 HTTP 例外在遠端被重設為 `deny`）與 `access_mode`（探測出的 `anonymousFree` 在遠端被依模型名重新推導回 `credentialed`），兩者現在都由 `RuntimeRouteConfig` 攜帶。Provider capability registry：`model_capabilities` 以 `HARNESS_PROBE_VERSION` gate tool_calling/vision/reasoning/streaming/wire，未過 typed-tool probe 的模型不進入 Codex catalog。
7. M34 session observability：`remote_session_summary` 只經 agent 的 `codex.sessionStatus` 查 native `codex app-server proxy` control API，回報 `managerState`／`detachedReady`／observability 與每個 native thread 的 status、activeTurnId、turnCount、lastTurnStatus；不以 Vellum Broker snapshot fallback。`l9_manual_detach_resume_observer` 必須從 running turn 取得 baseline，關閉 App 期間斷言 daemon PID 不變、turn 有進展且未 failed/interrupted，重開 App 後同一 native thread 仍可見。

## 11. Legacy active-lease／missing-key boundary key 升級驗收

驗證舊主機「native lease 已 active，但 boundary key 尚未建立或已遺失」的升級路徑：Desktop 必須在任何會啟動、安裝或重啟 Codex daemon 的操作前，重新 provision 與遠端一致的 per-host key，不要求使用者刪除 lease、重建 `~/.codex`、手動建立密鑰或清除既有工作階段；Agent 端的 fail-closed 行為（`resolve_boundary_key_for_daemon_lifecycle`，見 `docs/protocol-source-of-truth.md`）不得因此放寬。

前置條件：目標主機處於此升級前的狀態 —`profile_lease_is_active` 為 true，但 `BOUNDARY_CREDENTIAL_ID` 對應的 secrets 檔案不存在或內容與 Desktop 目前保存值不符（可能是 lease 建立於 boundary-key provisioning 加入之前、或先前部署中途失敗）。

1. 直接對此主機重試既有失敗的 deployment operation（不手動建立 key、不刪除資料、不重建 `~/.codex`）：operation 必須越過原本卡在 `installCodex`（或對應 daemon-lifecycle 步驟）的失敗點，完成 `deploymentApply`、`verification`，並到達 `detachedReady`。
2. 驗證修復後狀態：boundary credential present（`credential.status` 對 `BOUNDARY_CREDENTIAL_ID` 回真）、Proxy `ready=true`、`nativeCodex.daemonOwner=codexCliDaemon`、`durable=true`、`restartSafe=true`、managed lease `active`；升級前既有的 thread／workspace 仍存在，不被清除。
3. 再對同一主機執行一次相同 deployment：`configChanged=false`、`catalogChanged=false`、`restartRequired=false`，且 boundary key 不旋轉（第二次的 credential 內容與第一次相同）；第一次從 stopped 狀態啟動的 Proxy／native instance identity 必須已在 lifecycle 成功後確認，無 drift 時不得因殘留的 `none` marker 再重啟任一 consumer。
4. 經 Codex App 對該主機發起一個遠端模型請求，並對同一 thread 做 resume；確認 boundary header 確實由 durable native daemon（而非暫時性/broker 路徑）帶入請求。
5. 反面驗證：no-lease 主機（從未指向 Vellum provider）維持可執行與 Vellum 無關的原生 Codex lifecycle 操作，不因缺 key 被擋；provisioning 本身失敗時（例如 SSH／Agent 不可達），operation 必須停在 `RemoteBoundaryKeyProvisionFailed`，不得繼續 install/restart/proxy start，也不得把 `observedRevision` 標記為已追上新的 `desiredRevision`。

此驗收不改變既有 boundary authentication contract：Agent 仍是唯一的 secret 落地/掛載者，Desktop 保存的 per-host key 仍是唯一 authority，`resolve_boundary_key_for_daemon_lifecycle` 的 fail-closed／no-lease-tolerant 邊界維持不變。

第 1、2、3 項（重試即修復、修復後狀態、第二次不旋轉不重啟）由 `repairs_a_legacy_lease_whose_boundary_key_was_never_confirmed` 在 `pnpm run remote:e2e:full` 中自動驗證。前置狀態不是靠改主機造出來的：它用一個全新的 Desktop data root 指向前一個案例留下的、lease 已 active 且 daemon 在跑的主機 —— 那正是「Desktop 重裝／還原」與「lease 早於 boundary-key provisioning」看起來的樣子。租約 ID 與 `CODEX_HOME` 在修復前後相同，就是「沒有刪 lease、沒有重建 `~/.codex`」的機器可讀證據。第 4 項需要 provider 流量，第 5 項需要一台不可達的主機，兩者都不在假主機的能力範圍內，仍為人工。

## 12. Regression gates

每次修復後至少執行：

```powershell
cargo fmt --all -- --check
cargo test -p vellum-proxy-runtime
cargo test -p vellum-remote-agent
cargo test -p vellum --test proxy_runtime_parity
cargo clippy -p vellum-proxy-runtime --all-targets -- -D warnings
cargo clippy -p vellum-remote-agent --all-targets -- -D warnings
npm run typecheck
```

改動任何會影響部署設定的東西（`RuntimeRouteConfig`、`runtime_route`、`plan()` 的欄位投影、`to_route()`）時，另外跑：

```powershell
cargo test -p vellum --lib remote::deployment::desktop_parity
pnpm run remote:e2e:full
```

前者是秒級的 route parity 逐欄位比對，後者在真主機上確認 apply 收斂且第二次為 no-op。

ARM64 image/agent重新部署後，再跑 live deployment兩次：第一次應套用變更；第二次 `configChanged=false`、`catalogChanged=false`、`restartRequired=false`，且不得重啟無 drift 的 native daemon。

## 13. Promotion 判定

Promotion 標籤分開判定：

- e806：`API transport qualified` 或 `Codex protocol qualified`（不得宣稱模型能力）。
- Luna：`native remote production-qualified`。
- DeepSeek／Grok：`third-party remote production-qualified`。
- 全部 Gate 通過後：`remote manager production-qualified`。

任何 benchmark 缺少 pinned dataset、hidden grader、artifact hash 或實際執行證據時，只能標記未執行，不得推定 PASS。
