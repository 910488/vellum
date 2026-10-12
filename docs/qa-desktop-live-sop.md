# Desktop 實機測試 SOP：透過 Codex Desktop 對話，觀察 Vellum 的行為

操作文件，不取代 [qa-standard-process.md](qa-standard-process.md)。它補上那裡 `live` lane 缺的部分：
用**真正的 Codex Desktop**（不是 `codex exec`）跑對話，再從 Vellum 自己的紀錄判斷它做得對不對。

工具在 `scripts/qa/desktop-live/`：

| 檔案 | 做什麼 |
|---|---|
| `run.py` | 建立測試 workspace、透過 codex-desktop bridge 開 thread／送訊息／等完成、呼叫 observer、判定 |
| `observe.py` | 唯讀。給一個 Desktop thread id，對齊 rollout 與 Vellum 的紀錄，輸出檢查表與證據 JSON |

輸出在 `qa/runs/desktop-live-<時間>/`（git-ignored）：`msg-*.md` 送出的訊息、`baseline-*.json` 每步前的快照、
`obs-*/report-<tid>.md` 與 `evidence-<tid>.json`、`verdict-*.json`、`SUMMARY.md`。

## 1. 原理：thread id 是兩邊的對接鍵

```
Claude（協調者） ──bridge──▶ Codex Desktop ──HTTP/WS──▶ Vellum proxy :15721 ──▶ 上游
                                │                         │
                    ~/.codex/sessions/…/rollout-*-<tid>.jsonl   %LOCALAPPDATA%\Vellum\usage.sqlite3 / history.sqlite3
                                                           %LOCALAPPDATA%\com.vellum.desktop\logs\Vellum.log
```

- Codex 把每次模型回應的 token 數寫進 rollout 的 `token_count.last_token_usage`。
- Vellum 把同一次請求寫進 `request_usage`，`conversation_identity = codex:<tid>:<tid>`，
  `agent_attribution_json` 帶 `threadId` / `depth`（子代理 depth ≥ 1，`sessionId` 是 parent）。
- 所以「Desktop 發生了什麼」與「Vellum 記了什麼」可以**逐筆**對上：rollout 每一次回應都必須在
  Vellum 找到 input/output token 完全相同的一列。找不到＝那次請求沒經過 Vellum，或 Vellum 漏記。

Proxy 的 `/healthz`、`/readyz`、`/diagnostics/usage` 要 boundary key，**本 SOP 不讀任何憑證**，只讀上面三個地方。

## 2. 角色與邊界

- **Claude** 寫工作單、送訊息、等完成、判定、回報。不點 UI（除了第 6 節的手動檢查點）。
- **Codex Desktop** 執行回合。`bridge new` 的第一輪（「就緒」）是 bridge 自己開的 app-server 跑的；
  之後排入佇列的訊息才由 Desktop 跑（Enhanced 開著時就是 Enhanced core）。判定只看第二輪以後。
- **Vellum** 是受測物。測試期間不改它的設定；需要改的情境（換帳號、重啟）在第 6 節，逐次問使用者。
- 不用 `danger-full-access`；thread 一律是 bridge 的「代我核准」（on-request + auto_review + `:workspace`）。
- **thread 必須掛在 Desktop 專案「Vellum QA」底下**（root `E:\code_E\vellum-qa\workspace`）。
  沒有專案的 thread 由 Desktop 執行時 cwd 會變成 `C:\`，沙盒起不來（2026-10-12 實測：S2「執行環境初始化失敗」）。
  專案歸屬只能在 Desktop 關著時寫進 `.codex-global-state.json`，所以 S1 會關掉並重開 Desktop 一次，
  **一輪只開一個 Desktop thread**，S1–S4、S7–S9 依序在裡面跑。這一步要明確加 `--allow-close-desktop`；
  沒加時 Desktop 情境記 `NOT_RUN`，只跑 app-server 情境（S5、S6）。別的 thread 在跑時 bridge 會拒絕關閉，
  不要加 `--force`；runner 自己開的 thread 不算。**使用者正在用 Desktop 或 Vellum 時，不要加這個旗標。**
- **模型由 Desktop 的選單決定**，不是 thread 建立時給的 model：bridge 第一輪用指定模型，但佇列裡的回合
  一律用 Desktop 選單目前的模型與 effort；`codex queue -m` 也被忽略。runner 的 `judged-turn-model`
  檢查會把「實際跑的模型 ≠ 要測的模型」判成 `BLOCKED`，不會假 PASS。要測其他模型就先在 Desktop 選單切換（§6 M0）。

## 3. 前置

```bash
python -I -X utf8 scripts/qa/desktop-live/run.py preflight
```

全部 PASS 才往下：`vellum-proxy-desktop` 與 `ChatGPT` 程序在跑、`config.toml` 的 `model_provider = "vellum"`、
15721 在聽。記下 `bootCount`；整輪結束時它不能變（變了＝Vellum 中途重啟或當機，見 §8）。

測試 workspace 放 `E:\code_E\vellum-qa\workspace`（`VELLUM_QA_PROJECT_ROOT` 可改，專案名 `VELLUM_QA_PROJECT`），**不要放 OneDrive**——寫入慢，
Codex 會誤判工具被中斷。每次執行都新建一份：`calc.py`（`add` 故意寫錯）、`test_calc.py`、
`notes.txt`（隨機 marker），並 `git init` 好讓 diff 可以客觀判定。

## 4. 模型範圍

| 路由 | 用哪個 | 備註 |
|---|---|---|
| Official | `gpt-6.1-sol`，effort `low` | 主線。整套 S1–S9 約數萬 token，大多是 cached |
| 第三方 | 只有免費模型：`[OpenCode Zen] ling-3.0-flash-fin-free`、`nemotron-3.5-lightning-free` | OpenCode 訂閱已停。**先 probe**；probe 不過就整條第三方標 `BLOCKED`，不算 Vellum 的 FAIL |

```bash
python -I -X utf8 scripts/qa/desktop-live/run.py probe --model vlm-e3fdadcb34-ling-3-0-flash-fin-free
python -I -X utf8 scripts/qa/desktop-live/run.py probe --model vlm-523426b760-nemotron-3-5-lightning-free
```

2026-10-12 實測結果：兩個都不能用，第三方 lane 記 `BLOCKED (provider-policy)`。
`nemotron` 回 401 *free tier can only be used from within OpenCode*；`ling` 回上游 400 *Model is unavailable*。
Vellum 這端的路由（`opencode-zen`）、模型名稱、錯誤分類（`provider_unauthorized`）都正確。
另外觀察到：401 時 Codex 會連續重試約 12 次；第三方的第一個請求會被 Vellum 以
`chat shape: … system-only payloads are rejected` 400 擋掉。那是 Codex 的 WebSocket prewarm
（`generate: false`），PR #11 起由 Vellum 在本地回覆，不再送上游（見 §9）。

Probe 的判定分兩層：Vellum 有看到請求、而且路由正確 → 路由這段是好的；上游回 401
`free tier can only be used from within OpenCode` → 那是供應商政策，記 `BLOCKED (provider-policy)`。
子代理與 Guardian 在第三方模型上本來就不支援（Codex 依目錄能力欄位篩 `spawn_agent`，Vellum 刻意不對第三方宣告），只在 Official 測。

## 5. 自動情境

```bash
# 全部情境；S1 會關掉並重開 Desktop 一次
python -I -X utf8 scripts/qa/desktop-live/run.py round --model gpt-6.1-sol --alt-model gpt-6-luna --allow-close-desktop
# Desktop 有人在用：只跑 app-server 情境
python -I -X utf8 scripts/qa/desktop-live/run.py round --model gpt-6.1-sol --alt-model gpt-6-luna --only S5,S6
```

每一步都是：快照 → 送訊息 → 等 thread 閒置 → observer → 情境專屬檢查 → 判定。

情境分兩條路，報告裡的 `path` 欄位會標出來：
- **desktop**：訊息經 `codex queue` 由 Desktop 執行，模型是 Desktop 選單上的那個（S1–S4、S7–S9）。
- **app-server**：runner 自己開的 `codex app-server`，同樣走 Vellum，但模型、壓縮可以用程式指定（S5、S6）。
  Desktop 的 `/compact` 和選單切換都沒辦法用腳本操作，所以這兩個情境改走這條路。

| 情境 | 對 Codex 說什麼 | 情境專屬檢查（客觀） | 主要看 Vellum 的什麼 |
|---|---|---|---|
| S1 純對話 | 不用工具，只回覆隨機 token | 回覆含 token | 有經過 proxy、模型與 Desktop 選的一致 |
| S2 讀檔 | 回覆 notes.txt 的 marker | 回覆含 marker、有工具呼叫 | 工具呼叫與輸出成對（沒有被斷線吃掉） |
| S3 改檔＋測試 | 只修 `add`，跑 unittest | `git diff` 只有 calc.py、**我們自己**重跑 unittest 會過 | 多步工具回合每次回應都有 usage 列 |
| S4 續聊 | 不用工具，憑記憶回答函式名與 marker | 回覆含 `add` 與 marker | 同一個 conversation identity；**這一段第一個請求有 cache 命中** |
| S7 子代理 | 派兩個子代理，各回固定 token | 回覆含兩個 token | 兩個不同的 child threadId（depth 1）、child rollout 也走 vellum |
| S8 Auto-review | 在 workspace 以外寫檔（需要提權） | 回覆 APPROVED/DENIED 且與磁碟一致 | `codex-auto-review` 請求有經過 Vellum、**至少一次 200** |
| S9 Web search | 查 Python 3.13.0 發布日期 | 有 `web_search_call`、日期 2024-10-07 | 搜尋請求有被處理 |
| S5 壓縮（app-server） | 讀 marker → `thread/compact/start` → 不用工具憑記憶回答 | rollout 有 `compacted`、壓縮後仍答得出 marker | 壓縮那一次的摘要請求也有 usage 列。Official 是 Codex 自己發摘要請求，**不會**寫 Vellum 的 `compaction_journal`，journal 只記在報告裡 |
| S6 換模型（app-server） | 用 `--model` 回一個 token → 換 `--alt-model` → 換回 | 每一段都答得出上一段的 token | 每一段的列都是那一段的模型 |

## 6. 手動檢查點（每次需要使用者同意或動手）

這些 bridge 做不到，或會影響別的工作。操作者動手、Claude 只用 observer 判定：

```bash
python -I -X utf8 scripts/qa/desktop-live/observe.py snapshot --out qa/runs/<run>/baseline-M.json
# ……在 Desktop 動手……
python -I -X utf8 scripts/qa/desktop-live/run.py observe --thread <tid> --baseline qa/runs/<run>/baseline-M.json
```

| 情境 | 動手 | 預期 |
|---|---|---|
| M0 換到要測的模型 | 在 Desktop 的模型選單選目標模型（第三方或其他 Official 模型），再讓 runner 送下一步 | `judged-turn-model` PASS |
| M1 中斷 | 長工具回合進行中在 Desktop 按停止 | rollout 有 `turn_aborted`、沒有孤兒工具呼叫；Vellum 那筆是 499 `client closed`，之後沒有「成功完成」 |
| M2 壓縮 | 在 S1–S4 的 thread 輸入 `/compact` | rollout 有 `compacted`、壓縮請求有 usage 列；壓完再問 S4 的問題還答得出來。`compaction_journal` 只記 Vellum 自己做的壓縮（43 字元 `cmp_vellum_` id），Official 由 Codex 自己壓時不會新增，不是判定條件 |
| M3 換模型 | 同一個 thread 從 Official 換到 probe 過的第三方再換回 | 每一段 `route_id` 跟著換；換回後 Official 不報 `previous_response_id`/reasoning 相關 400 |
| M4 換帳號 | Vellum UI 手動切 Official 帳號後續聊 | `diagnostic_events` 有 `official_account_selected`；之後的列 `execution_account_hash` 換成新帳號；預期第一個請求 cache 歸零（帳號綁快取） |
| M5 Vellum 重啟 | 閒置時從托盤結束再開 Vellum，然後續聊 | `bootCount` +1（預期內）；續聊成功、session id 不變、沒有 thread-writer-lock 錯誤 |
| M6 長連線 | 同一 thread 超過 60 分鐘 | 上游 60 分鐘斷線後自動重連，續聊不報錯；觀察重連後第一個請求的 cache |

## 7. 判定

沿用 `PASS / FAIL / BLOCKED / NOT_RUN`。observer 另外輸出 `WARN`（不擋 PASS，但要寫進報告）與 `INFO`。

| observer 檢查 | FAIL 的意思 |
|---|---|
| `routed-through-vellum` | rollout 的 `model_provider` 不是 `vellum`/`vellum-official`＝繞過了 proxy，帳號切換、usage 全部失效 |
| `turns-completed` | 有回合沒完成、被中止，或 `task_complete.error`（例如 `Invalid previous_response_id`） |
| `tool-calls-paired` | 有工具呼叫沒有輸出＝那次呼叫被斷線吃掉 |
| `usage-matches-rollout` | 快照之後 Codex 記到的每一次回應，都要在 Vellum 找到 input/output token 完全相同的列；找不到＝沒經過 Vellum 或漏記 |
| `vellum-only-requests`（INFO） | 反方向：Vellum 有、rollout 沒有的列。正常會有失敗後重試、prewarm、Desktop 的標題產生（`gpt-5.6-luna`）。只有數量暴增才要查 |
| `model-matches-selection` | 和 rollout 對上的那些列，模型不是 Desktop 那一輪的模型 |
| `judged-turn-model`（runner） | 被判定的那一輪不是在要測的模型上跑 → 整個情境 `BLOCKED` |
| `vellum-not-restarted` | 測試期間 `bootCount` 變了 |
| `request-status`（WARN） | 有非 200；重試後成功的話回合仍會完成，但次數要記 |
| `cache-continuity`（WARN） | 同一 thread 的後續請求 >20k input 卻 0 cached：多半是重連 |
| `account-stable`（WARN） | 一個 thread 用了多個帳號、但期間沒有手動切換 |
| `auto-review`（WARN） | 審查請求有非 200 |

觀測視窗從快照的時間點開始（精確到毫秒）。只要整秒，就會把上一步收尾時的 `token_count` 算進這一步。
Codex 會在回合開始或壓縮後重送 `token_count`，但 total 不變，observer 只把 total 有變動的那筆算成一次回應。

情境 PASS＝情境專屬檢查全過、而且 observer 沒有 FAIL。**回覆裡宣稱做了不算數**；檔案、測試結果、Vellum 的列才算。

## 8. 失敗時去哪裡查

- 每列的完整欄位在 `evidence-<tid>.json` → `vellum.rows`：`error_category`、`connection_id`、`execution_account_hash`、`stage_times_ms` 已去掉（太長）。
- `Vellum.log` 的視窗（從快照的檔案大小開始）已經擷取在 `vellum.log`；`[Dispatch] upstream headers … status=` 是每次上游回應。
- `bootCount` 變了：Windows 事件記錄檔的 `Application Error` 過濾 `vellum`，`0xC0000409` 是 Rust panic-abort。
- 對話身分有三種形狀（`codex:<s>:<t>`、sha256、裸 uuid），不要自己 hash 去比；observer 用 `like '%<tid>%'`。
- 快取問題：`request_usage` 的 `cached_input_tokens` 依 `connection_id` 分組看，換連線才歸零是已知行為。

## 9. 已知問題基線（2026-10-12）

跑之前先知道哪些 WARN 不是這次造成的：

1. **Auto-review 一半失敗再重試**。10-06 22:48 起，`codex-auto-review` 有 715 次 `400 Unsupported parameter: previous_response_id`、67 次 `Unsupported parameter: generate`。
   原因：Codex 每個 WebSocket 回合會先送 `generate: false` 的 prewarm，再把真正的回合接在 prewarm 的 response id 上，只送增量 input。
   guardian 與第三方路由在 Vellum 走 portable HTTP 重放，`generate` 被原樣送給上游，接在後面的 `previous_response_id` 也就失效了。
   修在 PR #11（portable 路徑在本地回覆 prewarm，並把 prewarm 的 input 接回下一回合）。裝上含這個修正的版本之前，S5、S6、S8 都會出現這個 `auto-review` WARN。
2. **OpenCode Zen 免費模型**在 10-03 回 401 `free tier can only be used from within OpenCode`。
3. 上游 `upstream closed` 502、60 分鐘 websocket 上限、`client closed` 499 每週各有個位數到幾十次，屬於上游／使用者行為。

## 10. 一輪的節奏

1. `preflight` → 2. 兩個免費模型 `probe` → 3. `round`（Desktop 沒人用才加 `--allow-close-desktop`）→ 4. 需要時做 §6 手動檢查點 →
5. 讀 `SUMMARY.md` 與各 `report-*.md`，把 FAIL/WARN 寫成報告，FAIL 附 thread id 與 evidence 路徑。
6. 結束時再看一次 `bootCount`。測試 workspace 留著當證據，確認報告寫完才清。
