# Claude Desktop 本機 Code 額度路由

本功能是可進行實測的實驗版本。目標是保留 Claude Desktop 的帳號 A 與既有
**本機 Code** 工作階段，以獨立授權的 B／C 處理推論。不涵蓋一般 Chat、Cowork、
SSH／WSL 或雲端 Code，也不把獨立 CLI 成功視為 Desktop 驗收。

## 操作與設定範圍

在 Vellum「模型」頁、ChatGPT 帳號下方的 **Claude Desktop Code** 卡片中：
帳號列提供登入、選用、剩餘用量與移除確認；專案與憑證操作在「專案接入設定」中。

1. 輸入既有 Code 工作階段的專案資料夾。
2. Windows 使用「準備本機憑證」，核對 SHA-256 及信任範圍，勾選同意後
   「暫時信任此 CA」。本版實測需此步驟；不會因 TLS 失敗自動安裝。
   保留「原帳號（透傳測試）」，接入專案，先確認原對話仍可使用。
3. 若顯示等待，恢復原本的 Code 工作階段，使新程序讀取專案設定。第一次接入
   尚不保證免重啟；不要建立新對話來替代原工作階段的驗收。
   若回報憑證錯誤，先停用並確認直連恢復；恢復工作階段本身不保證能解決 CA 問題。
4. 點選「登入 Claude」，在瀏覽器選擇預期的 B。授權回呼只監聽
   loopback，優先 `127.0.0.1:54545`，若埠被占用或系統保留則改用空閒埠，
   五分鐘到期。這不會登出或更換 Desktop 的 A。授權只要求 profile／inference，
   可以在尚未接入專案時先完成 B／C 授權。
5. 選擇 B，在原 Code 對話送出下一個請求。確認卡片的實際路由帳號與結果。
6. 授權 C 並手動切換；選擇在下一個推論請求生效。在途串流維持其原選擇。

接入寫入所選專案的 `.claude/settings.local.json`，只新增 `HTTPS_PROXY` 與
`NODE_EXTRA_CA_CERTS`。不修改 Desktop 安裝檔、登入憑證或聊天資料。
Windows CA 信任則是額外、明確同意的使用者層級變更，影響其他程式，不能
宣稱只有專案設定變更；不會寫入全機 LocalMachine 信任庫。
代理的本機存取密碼會出現在該專案設定中；Claude OAuth token 與憑證私鑰則只
保存在 Vellum 的加密憑證儲存。不要將接入中的本機設定納入版本控制。

其他讀取相同專案設定的客戶端可能經過此代理，但只有辨識為 `claude-desktop`
的原生 Code 推論會替換授權。User-Agent 是相容性辨識，不能證明 OS 程序身分；
知道本機代理密碼的程序處於同一信任範圍。

已有 proxy、CA 或 TLS 驗證覆寫的專案／使用者設定會回報衝突，不自動取代。
程序啟動環境、企業 managed settings 或其他工具鏈的設定仍可能優先，必須以
真正 Desktop Code 流量確認接入。HTTPS 私有網路目的地會被拒絕；需要私網
連接器／工具的專案，應先驗證是否受影響。

不讀取 B／C 的聊天清單。不過，推論所需的目前對話內容會交給 B／C 授權的
服務處理；引用 A 專屬資源的附件、連接器或伺服器端工具不保證可跨帳號存取。

## 狀態與用量

「等待本機 Desktop Code」只代表代理與設定已就緒。「已觀察到 Code 請求」
代表收到符合接入規則的推論，不代表上游已接受或額度已實際歸屬 B。

最近請求顯示 session 雜湊、實際帳號、推論／Token 估算、HTTP 狀態與傳輸結果。
Token 估算成功不能代替真正模型回覆的驗收。不記錄 prompt、
工具內容、thinking 或 token。傳輸完成不等於模型／工具成功。

「更新訂閱用量」讀取該額度帳號的五小時／七天已用百分比並標示觀察時間。
未知回應格式或沒有該資訊的帳號顯示不可用，不捏造剩餘額度；不改寫 Desktop
原本顯示的 A 用量。手動更新會發出真實帳號 API 請求。

## 停用與復原

沒有進行中的請求時，使用「停用並還原設定」。有進行中的工作時先等待；
「取消推論並還原」會明確中斷它，不會跨帳號重送。

還原只移除仍符合 Vellum 寫入值的欄位，保留其他設定及使用者修改。
原本不存在且還原後空白的本機設定檔會刪除；CA 公開憑證與加密金鑰保留供
之後重新接入。由卡片新增的 Windows 臨時信任會在停用時移除；既有信任不會
被接管或移除。若程序異常中止，重開後卡片仍顯示殘留信任，可獨立移除，
即使已沒有專案 lease。信任變更前先保存加密 journal，在記憶體中的憑證寫入
隨機所有權標記，再連同標記匯入並驗證；標記遺失或被更動時，不自動刪除
憑證，須依指紋手動檢查。
卡片之外手動加入的 CA 不屬於 Vellum 管理，必須另外移除。

設定還原後，已運行的 Code 程序可能仍記得舊 proxy；恢復原 Code 工作階段
後再確認直連。不要將「已還原設定」誤認為所有舊程序已重新載入。

若 Vellum 異常終止，重新開啟後卡片會顯示需要修復。可對原專案「重新接入」
復用原埠與 CA，或「停用並還原設定」。若原埠被占用、設定變成連結或欄位
已被外部修改，依具體錯誤處理，不會強制覆寫。

OAuth refresh 在發出前先持久化保護狀態。若更新結果不明或程序中途終止，
帳號會要求重新授權，避免重用可能已消耗的 refresh token。

## 驗收關卡

原生測試工具可加上 `--evidence-file <本機 JSON 路徑>`，每秒原子更新有界
路由快照，包含帳號 ID、請求類型、狀態碼、完成狀態與 session 雜湊；
不包含 token、帳號 email、專案路徑或對話內容。此檔供真實 Code 對話驗收，
避免終端工作階段結束後失去逐筆證據。`count_tokens` 仍不能代替 `messages`。

離線測試使用暫存專案、假帳號與本機假上游；不讀取使用者聊天或登入資料。
它們驗證 TLS／CONNECT、目的地及代理授權限制、A／B／C 授權隔離、原始
Messages／gzip／thinking／工具內容保存、串流增量與中止、設定復原、加密儲存、
refresh 合併與失敗隔離。這些測試不能證明 Anthropic 接受跨帳號歷史。

另有選擇性 `bundled_cli_offline_probe`：啟動指定的真實 Code 執行檔，清空繼承
環境、使用暫存 config／專案及假 OAuth token，並拒絕所有對外 tunnel。
2026-10-19 已用本機 Desktop 內建 Code **2.1.299** 通過此測試：專案的
proxy／CA 設定被讀取，原生 `claude-desktop` 請求使用假帳號 B，且收到本機
假上游的完整回應。這證明內建執行檔的設定與傳輸相容性，仍不是 Desktop
UI、既有真實工作階段或訂閱額度的端到端驗收。後續新增不帶 `--print` 的
SDK `--await-initialize` 離線測試，不能用它取代原對話驗收。

實測排查另發現舊憑證缺少 Authority Key Identifier，會被 OpenSSL 嚴格驗證
拒絕。已補上 CA／leaf 的 AKI；舊格式僅在沒有接入 lease 時升級，避免在
使用中的專案下直接更換信任根。這個缺陷與 Desktop 的憑證錯誤是否同因，
仍須由真正 Desktop 再驗證，不能只靠離線握手宣告修復。

2026-10-19 修正後的再次實測仍為 `SELF_SIGNED_CERT_IN_CHAIN`：代理收到
24 次 CONNECT、22 次 TLS 失敗，零 API／推論請求。目標 Code 子程序的
環境中確實已有預期 `NODE_EXTRA_CA_CERTS` 路徑與 proxy；已還原專案設定、
停用代理並停止該子程序，沒有進行 B／C 授權或消耗其額度。
因此 AKI 缺陷已修正，但 Desktop 信任問題尚未解決。

後續已由使用者正常完全結束 Desktop，再以 `Start-Process -Environment`
只對該次程序樹傳入 `NODE_EXTRA_CA_CERTS` 啟動同一安裝版本；新主程序與
子程序均未留下此變數，尚未要求使用者送出推論即停止代理並還原設定。
唯讀解析 Desktop **2.42304.2.0** 執行檔的 Electron fuse wire：version 1、
length 9、index 2（EnableNodeOptionsEnvironmentVariable）為 `0x30`（disabled）。
這確認啟動期注入受該版封裝限制，不能再以普通重啟或同樣的環境注入要求重試。
[Electron 官方文件](https://www.electronjs.org/docs/latest/tutorial/fuses) 與
[Node 初始化原始碼](https://github.com/electron/electron/blob/main/shell/common/node_bindings.cc)
說明關閉該 fuse 會忽略並移除 `NODE_EXTRA_CA_CERTS`。未修改 fuse 或安裝檔。
這個結果解釋了父程序注入失敗；尚不能據此斷言原生 Code 為何拒絕設定內的 CA。

經使用者明確授權，已暫時將該 CA 加入 Windows **目前使用者** Root 信任庫：
SHA-256 `b60fcf617a09c74e157816b5b8d8e1ad327c18c28bc812390ed4e90641ecee9b`。
原 Desktop Code 對話接續成功，使用者確認 `VELLUM_CODE_POC_OK`；代理觀察到
session 雜湊 `ac935a6abe5fbe24`，兩筆原帳號推論均為 HTTP 200、傳輸完成，
TLS 失敗為零。這只通過需要使用者 CA 信任的 A → A 接入，B／C 未授權或驗證。
清理時另一筆請求在狀態查詢後進入，被舊版 harness 的強制 quit 中斷；已將
harness 的 stop／quit 改為預設 drain，只有明確 `force:true` 才取消在途工作。
已停用代理、移除專案設定及本次加入的 CA，信任庫確認剩餘零張；只停止
記住舊代理的目標 Code 子程序，Desktop 主程序與其他對話保留。
目前使用者 CA 會改變該使用者其他程式的信任範圍，不能作為「僅專案設定」
模式的靜默 fallback。另一候選仍是 Desktop 支援的 Code 啟動前網路環境入口。

後續實作已把這段信任操作納入卡片與 `CodeManager`，包含指紋確認、所有權、
停用清理與異常復原；新管理流程與 B／C 的實際驗收結果須另外確認，不能以
先前手動匯入成功取代。測試工具新增 `--authorize-only`，不建立代理或修改專案，
可先授權帳號；`start` 才接入指定專案。原固定 OAuth 埠在本機的 Windows
排除範圍內，已改為優先固定埠、失敗使用 ephemeral 埠，未更動系統保留設定。

B 帳號已完成瀏覽器 OAuth，與 Desktop A 為不同帳號；測試工具重啟後可從
本機加密儲存載入，且用量 API 正常回應。這只驗證帳號授權與持久保存，
尚未證明原 Code 對話已使用 B 推論。首次新信任管理實測在匯入後寫入所有權
標記時失敗，已回滾並確認 CA 不存在，代理未啟用；改為匯入前標記記憶體
憑證，再連同屬性複製至信任庫，實際信任庫的重測結果須另外確認。

2026-10-20 重測：匯入前標記通過原生記憶體存放區測試，但實際 CurrentUser
ROOT 重開後仍找不到憑證，因此管理流程拒絕啟用；已清理 journal，未接入
專案代理。獨立的原生 API、.NET `X509Store.Add` 與 `certutil -user -addstore`
也出現呼叫成功、重新查詢不存在的現象。唯讀確認目前使用者身分、CryptSvc
與可新增的 `.Default` 實體存放區正常，尚未定位根因；不能將記憶體測試通過
當成實際信任庫修復。下一步以使用者操作匯入精靈作對照，不變更系統政策、
LocalMachine 信任庫或 Desktop 安裝檔。B 憑證仍保存在本機加密儲存中。

手動精靈首次匯入後，只在 CurrentUser `CA`（中繼）找到同一指紋；再次明確
選擇 Root 並回報精靈成功後，Root 仍不存在。`certutil -verify` 識別出自簽名
CA，鏈錯誤只有 `CERT_E_UNTRUSTEDROOT`，未發現簽章或 CA 格式錯誤。
已移除這次匯入的中繼副本，Root／中繼均無本次 CA；代理保持停用，B 的
加密帳號資料保留。信任管理新增匯入成功但重開不存在的明確錯誤及回歸測試，
這只改善診斷與拒絕誤啟用，Windows 根憑證無法保存的根因仍未解決。

後續已由 computer-use 逐頁核對原生精靈：目前使用者、指定 Root、完成頁
明確列出「受信任的根憑證授權單位」，最後顯示「匯入執行成功」，關閉後
仍不存在。提升權限、同一 Windows 使用者的 API 對照也重現。經使用者另行
授權，已僅重新啟動 CryptSvc（沒有執行中的相依服務），確認恢復 Running；
重測結果未改善。API 回報 `CERT_ACCESS_STATE_WRITE_PERSIST_FLAG` 與明確
commit 成功，所有權標記仍失敗為 Windows error 2，重開仍無 CA。
每次對照後均確認本次 CA 不存在，沒有修改信任政策或 LocalMachine，也未
進入 B／C 推論驗收。CryptSvc 重啟不能視為修復；完整 Windows 重啟尚未測試。

不重開機的後續診斷發現：`ProtectedRoots\Certificates` 已含本次 CA 的 SHA-1，
但 Root 的實際憑證項目不存在。針對這張已核准的 CA，透過 Windows 文件化的
`CERT_STORE_PROV_REG` 開啟既有 CurrentUser Root，使用 `ADD_NEW` 補入後，
所有權屬性、重新開啟及一般邏輯 Root 讀取均成功；同一對照的實體移除亦成功。
這證明可在不重開機、不改政策及 LocalMachine 的條件下保存本次憑證，尚不能
證明原系統 provider 已恢復。接續以正常 Root 刪除 API 清除該測試項目，Windows
顯示正確指紋的刪除確認；確認後仍需重測一般匯入與管理流程。未重設整份
ProtectedRoots。

正常刪除確認後，受保護清單確實移除了該指紋，但緊接的一般匯入又重現相同
缺失，因此不宣告系統 provider 已修復。後續先由正式 manager 建立加密信任
journal，再以一次性修復工具核對固定 CA SHA-256、既有核准指紋、journal DER
與所有權，透過 Registry provider `ADD_NEW` 補回；不讀取帳號憑證、不輸出金鑰。
一般 Root 重新讀取及所有權核對成功，manager 回報 `trusted=true`、
`managed=true`，啟動後 `recoveryRequired=false`、`settingsMatch=true`。
B 用量 API 正常；原對話的 B 推論仍等待實測。此次證實可不重開機恢復本次
代理接入前置條件。後續已將原生 System provider 回報匯入成功、重開卻不存在
時的精確補入流程加入 Windows 信任管理：只開啟 CurrentUser Root 既有實體
Registry store，先檢查目標 DER 仍不存在，寫入同一張帶所有權標記的憑證，
最後仍由一般 Root 介面及 journal 驗證。這只在使用者已明確核准該 SHA-256
後執行；不修改整份保護清單，也不向 LocalMachine 寫入。

原對話已回覆 B 測試詞，B 五小時用量由 21% 增為 38%；此結果與路由相符。
本次未能取得執行中代理的逐筆紀錄，故請求歸屬仍待該紀錄核對，不能以
用量增幅單獨證明該次推論確由 B 完成。使用者已要求暫停 C 測試；待授權
入口已關閉，本機加密帳號庫仍只有 B，C 未新增。B 代理目前仍維持運作，
本次 CA 的管理 journal 仍在，待 B 驗收結束或後續明確停用時清理。

2026-10-20 補做 A→B 逐筆驗收：重新啟動同一專案代理，保留既有信任及
原 Code 對話。使用者確認原對話收到 `VELLUM_CODE_POC_OK`（與本次建議的
`VELLUM_CODE_AB_ACCEPT_OK` 不同，因此不聲稱逐字回覆匹配）。代理保存
兩筆 `operation=messages`、`state=completed`、`upstreamStatus=200`：
`01M5AKCDF9YJJC51VB76V0R3Z9`、`01M5AKCFM6RMYS0ZR9M63B2KSG`，
兩筆 session 雜湊均為 `5fc665ba7c0ab85e`，路由帳號均為 B
`067f1c9f-5049-407f-9afc-7b2edab145a6`，TLS failures 為 0。
本機有界證據快照位於隔離 POC data root 的 `acceptance-routing.json`。
這次可以確認 B 的真正推論與串流完成，A→B 路由驗收通過；C、工具／
thinking／長對話及 A 自然達限後接替等關卡仍維持原狀態。
驗收後兩筆推論均已完成，因背景非推論請求持續存在，採立即停止完成收尾。
使用者確認 Windows 刪除提示後，已驗證 Root 無本次 CA、trust journal 與
project lease 均不存在、專案 `settings.local.json` 已移除、52039 已無 listener；
B 的加密帳號檔仍在。最終有界證據顯示 running=false、activeRequests=0、
trusted=false、managed=false、recoveryRequired=false，並保留上述完成紀錄。
此項確認設定與信任清理完成；清理後原對話直連重試仍未在本輪執行。

`bundled_sdk_project_auth_token_does_not_override_host` 另以假 A／B token、
隔離專案及拒絕對外 tunnel 的本機代理驗證：在模擬 Desktop SDK 啟動模式中，
專案 `ANTHROPIC_AUTH_TOKEN` 沒有取代主程序傳入的 A OAuth。不能據此走
免 CA 的專案 token 切換，也不能推論所有 CLI 認證方式都有相同優先順序。

`bundled_sdk_project_base_url_does_not_override_host` 使用假 token、本機假上游，
並保留拒絕對外目的地的代理保護，驗證專案 `ANTHROPIC_BASE_URL` 不能覆蓋
啟動環境指定的 API base URL；目前不能直接用單一專案 base URL 取代 TLS 接入。

```powershell
$env:VELLUM_CLAUDE_CODE_EXE = '<Desktop bundled claude.exe path>'
cargo test -p vellum-proxy-runtime bundled_cli_offline_probe --lib -- --ignored --nocapture
```

真實驗收依序執行，每項保留版本、session 雜湊、路由帳號及結果，不保存內容或憑證：

| 關卡 | 通過條件 | 目前狀態 |
| --- | --- | --- |
| Desktop A → A | 原本的本機 Code 對話、工作目錄、工具權限維持可用 | 暫時加入目前使用者 CA 後通過；僅專案 CA 模式仍未通過 |
| 同一對話 A → B | 原對話接續成功，上游使用 B 授權；用量 API 可取得時交叉確認 | 通過：原對話回覆成功，兩筆 B messages HTTP 200 completed，TLS 0；詳見上方逐筆紀錄 |
| B → C | 下一個推論使用 C，在途請求仍使用 B | 待實測 |
| thinking／工具／長對話 | 簽章、工具結果配對、壓縮後接續均被真正上游接受 | 待實測 |
| 多工作階段／子代理 | 可識別請求歸屬，並行操作不混用在途授權 | 待實測 |
| A 額度耗盡 | Desktop 仍送出請求，能由 B 完成 | 待自然達限帳號 |
| 停用／重啟／恢復 | 原工作階段可繼續，沒有其他帳號聊天出現在 Desktop | 已還原設定並確認原對話直連回覆 `VELLUM_CODE_POC_OK`；跨帳號與重啟仍待實測 |

不可刻意大量消耗額度來製造 A 達限。沒有可用條件的關卡維持待驗證。
若 Desktop 在請求送到代理之前就因 A 達限而阻擋，純代理方案不能宣告完整
透明接替；需另行研究該阻擋點，不自動擴大為修改 Desktop。

本版只有明確的手動帳號切換，沒有自動額度輪替。真實相容性關卡通過後，才
加入明確 quota 拒絕後的有限重試與冷卻。已輸出部分串流、工具副作用或處理
狀態不明的請求始終不能自動重送。
