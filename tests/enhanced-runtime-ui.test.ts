/**
 * Enhanced Runtime 之後，壓縮不再是 Vellum 的產品面。
 *
 * 官方那邊由 Official Codex 原生壓縮，第三方那邊由 Enhanced Codex 自己做
 * 本機壓縮與上下文回復 —— 兩邊都不吃 Vellum 的 Canonical 策略，所以設定頁
 * 不該再有可調的壓縮策略，前端也不該留任何寫入策略的入口。
 *
 * 這份測試不是重複型別檢查：型別檢查只會告訴你現在編得過，不會擋住有人
 * 之後又從別的地方把這個產品面接回來。
 */
import { describe, expect, it } from "vitest";
import apiSource from "../src/lib/api.ts?raw";
import proxySource from "../src-tauri/src/commands/proxy.rs?raw";
import managerSource from "../src-tauri/src/enhanced_runtime/desktop_manager.rs?raw";
import protocolSource from "../src-tauri/src/enhanced_runtime/protocol_compat.rs?raw";
import lockfileSource from "../crates/vellum-enhanced-codex/src/lockfile.rs?raw";
import digestSource from "../crates/vellum-enhanced-codex/src/digest.rs?raw";
import lockJsonSource from "../enhanced-runtime.lock.json?raw";
import statusBarSource from "../src/components/StatusBar.tsx?raw";
import enhancedCoreSource from "../src/screens/EnhancedCore.tsx?raw";
import zhTwSource from "../src/i18n/locales/zh-TW.ts?raw";
import zhCnSource from "../src/i18n/locales/zh-CN.ts?raw";
import enSource from "../src/i18n/locales/en.ts?raw";
import jaSource from "../src/i18n/locales/ja.ts?raw";
const localeSources: Record<string, string> = {
  "zh-TW": zhTwSource,
  "zh-CN": zhCnSource,
  en: enSource,
  ja: jaSource,
};
import onboardingSource from "../src/screens/Onboarding.tsx?raw";
import appSource from "../src/App.tsx?raw";
import settingsSource from "../src/screens/Settings.tsx?raw";
import typesSource from "../src/types.ts?raw";
import packageSource from "../package.json?raw";
import sidecarBuildSource from "../scripts/build-sidecar.mjs?raw";
import tauriConfigSource from "../src-tauri/tauri.conf.json?raw";
import { APP_LOCALES } from "@/i18n/locale";
import { resourceKeySet } from "@/i18n/resources";

/** 舊的公開壓縮策略指令。前端一個都不該再叫。 */
const RETIRED_COMMANDS = [
  "get_session_compaction_policy",
  "set_session_compaction_policy",
  "get_route_compaction_policy",
  "set_route_compaction_policy",
  "get_global_compaction_policy",
  "set_global_compaction_policy",
  "get_global_compactor",
  "set_global_compactor",
  "resolve_compaction_policy",
  "resolve_auto_compact_token_limit",
  "run_compaction",
  "restore_latest_compaction",
] as const;

/** 舊的策略型別。CompactionPreview／CompactionDetail 是上下文頁的觀測資料，留著。 */
const RETIRED_TYPES = [
  "CompactionStrategyKind",
  "CompactionStrategy",
  "CompactorSelector",
  "SessionCompactionPolicy",
  "ResolvedCompactionPolicy",
  "GlobalCompactorConfig",
  "RequestPipeline",
] as const;

/** 舊的策略字串命名空間。log.compaction.* 與 context.* 是別的東西，不在此列。 */
const RETIRED_KEY_PREFIXES = [
  "compaction.strategy.",
  "compaction.pipeline.",
  "compaction.continuity.",
  "compaction.replay.",
  "settings.page.compaction.",
  "settings.page.errors.compaction",
  "settings.page.errors.effortSave",
] as const;

describe("compaction policy product surface is retired", () => {
  it("invokes no compaction policy command from the renderer", () => {
    for (const command of RETIRED_COMMANDS) {
      expect(apiSource).not.toContain(command);
    }
  });

  it("declares no compaction policy wire types", () => {
    for (const name of RETIRED_TYPES) {
      expect(typesSource).not.toMatch(new RegExp(`(?:interface|type) ${name}\b`));
    }
  });

  it("keeps the strategy selector out of Settings", () => {
    expect(settingsSource).not.toContain("@/lib/compactionPolicy");
    expect(settingsSource).not.toContain("settings.page.compaction.");
    expect(settingsSource).not.toMatch(/setRouteCompactionPolicy|strategyOptionsForProvider/);
  });

  it("does not advertise Vellum Canonical during onboarding", () => {
    expect(onboardingSource).not.toContain("onboarding.features.canonicalTerm");
    expect(onboardingSource).not.toContain("onboarding.features.canonicalBody");
    for (const locale of APP_LOCALES) {
      const keys = resourceKeySet(locale);
      expect(keys).not.toContain("onboarding.features.canonicalTerm");
      expect(keys).not.toContain("onboarding.features.canonicalBody");
    }
  });

  it("ships no compaction policy strings in any locale", () => {
    for (const locale of APP_LOCALES) {
      const offenders = resourceKeySet(locale).filter((key) =>
        RETIRED_KEY_PREFIXES.some((prefix) => key.startsWith(prefix)),
      );
      expect({ locale, offenders }).toEqual({ locale, offenders: [] });
    }
  });
});

describe("Settings states the execution plane instead", () => {


  it("translates the plane rows in every locale", () => {
    for (const locale of APP_LOCALES) {
      const keys = resourceKeySet(locale);
      for (const key of [
        "settings.page.enhancedRuntime.contextNote",
        "settings.page.enhancedRuntime.planeOfficial",
        "settings.page.enhancedRuntime.planeEnhanced",
        "settings.page.enhancedRuntime.planeUnbound",
      ]) {
        expect(keys).toContain(key);
      }
    }
  });
});

describe("Remote Control diagnostics", () => {
  it("does not present optional filesystem probes as connection failures", () => {
    expect(enhancedCoreSource).toContain("REMOTE_FAILURE_STAGES");
    expect(enhancedCoreSource).not.toMatch(/REMOTE_FAILURE_STAGES[\s\S]{0,300}fs\/readFile/);
  });
});

/**
 * 「可以啟用」跟「Codex Desktop 現在真的走這條」是兩件事。
 *
 * 之前 Settings 只有一個 ready，而 ready 只代表 artifact 驗過 —— 於是畫面
 * 說 Ready、Qwen 也聊得動，實際跑的還是 Official Codex。這裡把兩件事分開，
 * 並且擋住有人再把 active 從 artifact 推導回去。
 */
describe("Settings separates a verified artifact from an adopted bridge", () => {
  it("exposes artifactReady, active and ready as three different fields", () => {
    expect(typesSource).toMatch(/artifactReady: boolean;/);
    expect(typesSource).toMatch(/active: boolean;/);
    expect(typesSource).toMatch(/bridgeObserved: boolean;/);
    expect(typesSource).toMatch(/ready: boolean;/);
    expect(typesSource).toMatch(/activationState:/);
    expect(typesSource).toMatch(/environmentState:/);
    expect(typesSource).toMatch(/restartRequired: boolean;/);
    expect(typesSource).toMatch(/launchId: string \| null;/);
    expect(typesSource).toMatch(/bridgePid: number \| null;/);
    expect(typesSource).toMatch(/officialChildPid: number \| null;/);
    expect(typesSource).toMatch(/enhancedChildPid: number \| null;/);
    expect(typesSource).toMatch(/promotionReady: boolean;/);
    expect(typesSource).toMatch(/lastQualification: EnhancedQualificationResult \| null;/);
  });

  /* 一顆開關收掉了三段式交接，但收掉的是排版，不是那三件事必須分開判斷
     的理由。狀態字只能從 ready 推 —— 從 artifactReady 推就是「畫面說
     Ready、實際跑 Official Codex」那個 bug 的回歸。 */


  /* 一顆亮著的開關就是在說「這個功能正在生效」。驗證一過設定檔就記下
     enabled = true，但接管要等 Desktop 重啟並回報接上 —— 中間任何一步
     失敗，跟著 enabled 走的開關就會亮著，而那時候跑的還是原生 Codex。 */


  /* Codex Desktop 沒開著的時候就沒有東西可以重啟，但那條路徑已經接管好了，
     它下次自己啟動就會讀到 bridge。把這種情況講成失敗，會害使用者去修一個
     沒有壞的東西。 */


  /* 使用者自己重開過 Codex Desktop，或上一次注入其實成功了 —— 兩種都會讓
     Desktop 已經跑在這座 bridge 上。這時候「重新啟動」只是把人家正在用的
     視窗關掉再開一次，什麼都沒換到。 */


  /* Proxy 起得來跟 Enhanced 武裝得起來是兩件事：Enhanced 只決定第三方對話
     由哪個 Codex runtime 執行，Proxy 決定它們到不到得了 Provider。把後者綁在
     前者上，等於為了一個只改變「怎麼跑」的升級，讓使用者失去所有第三方模型。 */
  it("keeps a failed Enhanced arming out of the Proxy's own success", () => {
    // 回傳 () —— 呼叫端在型別上就沒有東西可以失敗。
    expect(proxySource).toMatch(/fn arm_enhanced_runtime_for_proxy\(state: &AppState, generation: u64\)\s*\{/);
    expect(proxySource).not.toMatch(/arm_enhanced_runtime_for_proxy\(state\)\?/);
    // 但接管一定要交回去：留著一個準備不起來的 launch 才是真正要防的半吊子狀態。
    const arming = proxySource.slice(proxySource.indexOf("fn arm_enhanced_runtime_for_proxy"));
    expect(arming).toContain("release_desktop_launch");
    expect(arming).toContain("enhancedDesktopRuntimeNotArmed");
  });

  /* bridge 的路徑不是選擇（UI 只陳述它），但它會自己過期：每次組建都會放一支
     新的、以雜湊命名的 sidecar。一個使用者不能編輯的欄位不該擋得住任何事。 */
  it("corrects a stale bridge path instead of failing on it", () => {
    expect(managerSource).toMatch(/fn adopt_packaged_bridge/);
    // 每一次 sync 都要修，因為 Proxy 啟動走的就是這條。
    const sync = managerSource.slice(managerSource.indexOf("pub fn sync_desktop_launch"));
    expect(sync.slice(0, 400)).toContain("adopt_packaged_bridge(data_root)?");
    // 舊 bridge 的租約要先用它自己的名字還回去，之後就沒有東西還得動它。
    const adopt = managerSource.slice(managerSource.indexOf("fn adopt_packaged_bridge"));
    expect(adopt.indexOf("release_configured_bridge")).toBeLessThan(
      adopt.indexOf("settings.bridge_executable = packaged"),
    );
  });

  /* Proxy 沒在跑的時候  /* Proxy 沒在跑的時候 `sync_desktop_launch` 會刻意釋放那條路徑而不是接管，
     所以「注入」在那個狀態下不可能成功。先講清楚再停下來，不要讓它跑到
     一半再回報一個低層的通知碼。 */


  /* 這一整段的重點是一顆開關，不是三顆按鈕。 */


  /* 三支執行檔沒有一支是選擇。Enhanced core 會被逐位元組比對
     enhanced-runtime.lock.json 的雜湊，所以整台機器上只有一個檔案能通過 ——
     叫使用者去找它，是把一個固定答案變成尋寶。上線前把這個選擇拿掉。 */


  /* 接管是在按下 Proxy 的那一刻發生的，而失敗跟成功長得一模一樣：Codex
     照常回話，只是回話的是原生 Codex。所以失敗要當著使用者的面講一次，
     不管他人在哪一頁。 */
  it("surfaces a failed takeover outside Settings, without blocking anything", () => {
    // 通知本來只長在設定頁；shell 也要看得到同一個事實。
    expect(appSource).toContain("enhancedDesktopRuntimeNotArmed");
    expect(appSource).toContain("enhancedDesktopBridgeNotObserved");
    expect(appSource).toContain('className="alerts"');

    // 不阻塞：不是 modal，不搶焦點，不惰性化背景。
    expect(appSource).not.toMatch(/<Confirm[\s>]/);
    expect(appSource).not.toContain("showModal");
    expect(appSource).not.toContain("window.alert");

    // 關掉的是「這一次這個原因」。同一種失敗換了原因就是新的一件事。
    expect(appSource).toMatch(
      /`\$\{notice\.code\}:\$\{JSON\.stringify\(notice\.params \?\? \{\}\)\}`/,
    );
    expect(appSource).toContain("dismissedAlerts");
  });

  /* 自動接管與自動還原都在 Proxy 的生命週期上，不是設定頁的按鈕。 */
  it("arms on proxy start and releases on proxy stop", () => {
    const start = proxySource.indexOf("pub async fn start_proxy");
    expect(start).toBeGreaterThan(-1);
    expect(proxySource).toContain("arm_enhanced_runtime_for_proxy(&arm_state, generation);");
    // 接管失敗不能讓 Proxy 起不來：這個函式沒有回傳值可以失敗。
    expect(proxySource).toMatch(/fn arm_enhanced_runtime_for_proxy\(state: &AppState, generation: u64\) \{/);
    // 停掉時一定要把 CODEX_CLI_PATH 還回去，否則下次 Codex 自己啟動會
    // 讀到一個後面沒有資料面的 bridge。
    expect(proxySource).toContain("release_desktop_launch(&state.data_root())");
    expect(managerSource).toMatch(
      /if proxy_running \{\s*DesktopLaunchDesire::Arm\s*\} else \{\s*DesktopLaunchDesire::Disarm/,
    );
  });

  /* 缺 core 是組建壞了，不是使用者還沒設定完。這兩件事講法不能一樣。 */


  /* 微型終端機只印指令真的回報的東西。為了讓進度看起來在動而編出來的行，
     會讓它在唯一該被相信的時候不能被相信。 */


  /* 原始狀態沒有消失，只是不再跟「我開了沒有」搶同一塊視線。 */


  it("never reports the browser preview as active", () => {
    // The preview has no Codex Desktop, so it has no attestation to read.
    expect(apiSource).toMatch(/function previewEnhancedRuntimeStatus\(\)/);
    expect(apiSource).toMatch(/active: false,/);
  });

  it("translates every new row and notice in all four locales", () => {
    for (const locale of APP_LOCALES) {
      const keys = resourceKeySet(locale);
      for (const key of [
        "settings.page.enhancedRuntime.inject.on",
        "settings.page.enhancedRuntime.inject.off",
        "settings.page.enhancedRuntime.inject.blocked.noCore",
        "settings.page.enhancedRuntime.inject.blocked.proxyStopped",
        "settings.page.enhancedRuntime.inject.blocked.stuck",
        "settings.page.enhancedRuntime.inject.mean.armed",
        "settings.page.enhancedRuntime.term.preflight",
        "settings.page.enhancedRuntime.term.proxyStopped",
        "settings.page.enhancedRuntime.term.armed",
        "settings.page.enhancedRuntime.term.alreadyInjected",
        "settings.page.enhancedRuntime.term.missingHelper",
        "settings.page.enhancedRuntime.missingHelpers",
        "settings.page.enhancedRuntime.inject.working",
        "settings.page.enhancedRuntime.inject.switchLabel",
        "settings.page.enhancedRuntime.inject.mean.on",
        "settings.page.enhancedRuntime.inject.mean.off",
        "settings.page.enhancedRuntime.inject.mean.wanted",
        "settings.page.enhancedRuntime.inject.mean.working",
        "settings.page.enhancedRuntime.enhancedBinaryHint",
        "settings.page.enhancedRuntime.coreMissing",
        "settings.page.enhancedRuntime.term.idle",
        "settings.page.enhancedRuntime.term.verify",
        "settings.page.enhancedRuntime.term.verifyOk",
        "settings.page.enhancedRuntime.term.verifyFailed",
        "settings.page.enhancedRuntime.term.restart",
        "settings.page.enhancedRuntime.term.restartBack",
        "settings.page.enhancedRuntime.term.restartRefused",
        "settings.page.enhancedRuntime.term.adopt",
        "settings.page.enhancedRuntime.term.injected",
        "settings.page.enhancedRuntime.term.notInjected",
        "settings.page.enhancedRuntime.term.release",
        "settings.page.enhancedRuntime.term.releasedOk",
        "settings.page.enhancedRuntime.term.envUnset",
        "settings.page.enhancedRuntime.activationLabel",
        "settings.page.enhancedRuntime.environmentLabel",
        "settings.page.enhancedRuntime.desiredState",
        "settings.page.enhancedRuntime.desiredEnabled",
        "settings.page.enhancedRuntime.desiredDisabled",
        "settings.page.enhancedRuntime.environmentValue",
        "settings.page.enhancedRuntime.environmentUnset",
        "settings.page.enhancedRuntime.staleBridge",
        "settings.page.enhancedRuntime.observedBridge",
        "settings.page.enhancedRuntime.protocolHash",
        "settings.page.enhancedRuntime.details",
        "settings.page.enhancedRuntime.officialBinaryHint",
        "settings.page.enhancedRuntime.bridgeBinaryHint",
        "settings.page.enhancedRuntime.activation.active",
        "settings.page.enhancedRuntime.activation.awaitingDesktopRestart",
        "settings.page.enhancedRuntime.activation.disablePendingRestart",
        "settings.page.enhancedRuntime.activation.environmentDrift",
        "settings.page.enhancedRuntime.environment.leased",
        "settings.page.enhancedRuntime.environment.orphanedBridge",
        "settings.page.enhancedRuntime.launchDetail",
        "settings.page.enhancedRuntime.runInstalledGate",
        "settings.page.enhancedRuntime.installedGateHint",
        "settings.page.enhancedRuntime.lastQualification",
        "settings.page.enhancedRuntime.qualificationPassed",
        "settings.page.enhancedRuntime.qualificationComponentOnly",
        "settings.page.enhancedRuntime.qualificationFailed",
        "settings.page.enhancedRuntime.qualificationReport",
        "runtime.notice.enhancedDesktopBridgeReady",
        "runtime.notice.enhancedDesktopBridgeFailed",
        "runtime.notice.enhancedDesktopBridgeNotObserved",
        "runtime.notice.enhancedDesktopRuntimeNotArmed",
      ]) {
        expect(keys).toContain(key);
      }
    }
  });
});

describe("development builds pin the sidecar bytes they execute", () => {
  it("stages the debug sidecar before Tauri compiles the development host", () => {
    const packageJson = JSON.parse(packageSource) as { scripts: Record<string, string> };
    const tauriConfig = JSON.parse(tauriConfigSource) as {
      build: { beforeDevCommand: string; beforeBuildCommand: string };
    };
    expect(packageJson.scripts["build:sidecar:dev"]).toContain("--profile=debug");
    expect(tauriConfig.build.beforeDevCommand).toMatch(/^pnpm run build:sidecar:dev &&/);
    expect(tauriConfig.build.beforeBuildCommand).toContain("pnpm run build:sidecar");
    expect(sidecarBuildSource).toContain('stageUnchangedSkip(built, staged)');
  });
});

/* Codex Desktop 會自己更新。這一組把「更新之後會發生什麼」釘住：路徑自動
   跟上、判定分級、以及畫面要分得出「在跑但沒驗過」和「已經回退」。 */
describe("Enhanced survives a Codex Desktop update", () => {
  /* Official core 跟 bridge 一樣不是選擇（UI 只陳述它），而且一樣會自己過期
     —— Codex Desktop 更新時會換一個以雜湊命名的新目錄。 */
  it("corrects a stale Official core path instead of failing on it", () => {
    expect(managerSource).toMatch(/fn adopt_discovered_official/);
    const sync = managerSource.slice(managerSource.indexOf("pub fn sync_desktop_launch"));
    expect(sync.slice(0, 400)).toContain("adopt_discovered_official(data_root)?");
    // 狀態卡也要修：那個錯誤就是出現在卡片上，而卡片上沒有能修它的控制項。
    const status = managerSource.slice(managerSource.indexOf("pub fn desktop_runtime_status"));
    expect(status.slice(0, 400)).toContain("adopt_discovered_official(data_root)");
  });

  /* 這是這次改動的核心：協定不再用整份 schema 的雜湊相等去擋。那個雜湊會
     自己過期，而且任何一個不相干的新方法都會讓 Enhanced 整組解除武裝。 */
  it("grades protocol compatibility instead of comparing one hash", () => {
    const verify = managerSource.slice(managerSource.indexOf("fn verify_settings"));
    const body = verify.slice(0, verify.indexOf("fn hex_sha256_file"));
    expect(body).toContain("protocol_compatibility(");
    expect(body).toContain("protocol.verdict.may_arm()");
    // 舊的等式判定不能留著：留著就等於這條路還在。
    expect(body).not.toMatch(/desktop_identity\.schema_sha256.*!=\s*APP_SERVER_PROTOCOL_HASH/s);
    // 只有已證明 breaking 的 routed 差異可以擋。比較器無法判定的 routed
    // shape 仍要留下來診斷，但不能把「不知道」升格成「已破壞」。
    expect(protocolSource).toMatch(/fn may_arm/);
    expect(protocolSource).toContain("Incompatible");
    expect(protocolSource).toContain("fn blocks_adoption");
    expect(protocolSource).toContain(
      "self.routed && self.kind != DeltaKind::ShapeUnverified",
    );
    const compare = protocolSource.slice(protocolSource.indexOf("pub fn compare"));
    expect(compare).toContain("deltas.iter().any(ProtocolDelta::blocks_adoption)");
  });

  /* 釘選的 appServerProtocolHash 從來沒有被拿去跟任何 binary 比對過:它是
     一個人手維護的宣告，每次 rebase 都要記得改，改漏了也不會有人發現。
     protocol_compat 已經在執行期量兩支 core 真正的協定，所以這個釘選只剩
     維護成本。它也不是 runtime digest 的獨立輸入 —— schema 一變，
     codexUpstreamCommit 跟 artifactSha256 一定跟著變。 */
  it("keeps the retired protocol pin from coming back", () => {
    const sources: Array<[string, string]> = [
      ["lockfile.rs", lockfileSource],
      ["digest.rs", digestSource],
      ["enhanced-runtime.lock.json", lockJsonSource],
      ["types.ts", typesSource],
      ["desktop_manager.rs", managerSource],
      ["api.ts", apiSource],
    ];
    for (const [name, source] of sources) {
      expect(source, name).not.toContain("APP_SERVER_PROTOCOL_HASH");
      expect(source, name).not.toContain("appServerProtocolHash");
      expect(source, name).not.toContain("app_server_protocol_hash");
    }
    // 留下來的是量測，不是釘選:兩支 binary 各自算出來的 schema 雜湊。
    expect(protocolSource).toContain("pinned_schema_sha256");
    expect(protocolSource).toContain("desktop_schema_sha256");
  });

  /* 匿名的 oneOf 變體沒有 title 也不在 definitions 裡。只認名字的話，
     elicitation 換掉整組欄位這種真的線上變更會讀成「沒有差異」。 */
  it("collects anonymous union members, not just named types", () => {
    expect(protocolSource).toMatch(/fn collect_anonymous/);
    expect(protocolSource).toContain("oneOf");
  });

  /* 「可用但不保證正確性」是掛在「在跑但沒驗過」的，不是掛在回退上：回退
     之後跑的是原生 Codex，那本身沒有不正確，只是 Enhanced 的功能都不在。 */
  it("says unverified and fallen back in different words", () => {
    expect(typesSource).toMatch(/unverified: boolean/);
    expect(statusBarSource).toContain("enhanced?.unverified");
    expect(statusBarSource).toContain("status.enhancedUnverified");
    // 未驗證是 warn，不是 ok —— 綠燈會把「沒人測過」講成「測過了」。
    expect(statusBarSource).toMatch(/unverified \|\| !enhanced\?\.ready\s*\?\s*"warn"/);
    /* 按鈕反映的是「有沒有在服務」，不是「設定是不是最新的那一份」:上一次
       啟動留下、仍在服務每一輪的 bridge，說成未載入是假的。 */
    expect(statusBarSource).toMatch(/const loaded = Boolean\(enhanced\?\.serving\)/);
    expect(managerSource).toContain("status.serving = attestation.is_serving() && adopted;");
    expect(typesSource).toMatch(/serving: boolean/);
    const missing = APP_LOCALES.filter((locale) => {
      const keys = new Set(resourceKeySet(locale));
      return (
        !keys.has("status.enhancedUnverified") ||
        !keys.has("settings.page.enhancedRuntime.protocol.verdict.unverified") ||
        !keys.has("settings.page.enhancedRuntime.protocol.fallback") ||
        !keys.has("settings.page.enhancedRuntime.protocol.details") ||
        !keys.has("settings.page.enhancedRuntime.inject.mean.staleLaunch")
      );
    });
    expect(missing).toEqual([]);
  });

  /* 回退只有一個意思:本來要接管、被協定判定擋下來。寫成「enabled 但還沒
     ready」的話，「接管好了，等 Codex 下次啟動」也會命中，於是同一張浮層
     會一邊說啟動路徑已接管、一邊說已回到原生 Codex。 */
  it("calls it a fallback only when the protocol gate actually withheld arming", () => {
    const fellBack = statusBarSource.slice(
      statusBarSource.indexOf("const fellBack"),
      statusBarSource.indexOf("const details"),
    );
    expect(fellBack).toContain('protocol?.verdict === "incompatible"');
    expect(fellBack).not.toContain("!enhanced?.ready");
  });

  /* 判定是關於兩支 binary 的事實，跟「現在跑的是誰」是兩條軸。把後者寫進
     判定說明，就會出現「Enhanced 正在跑」跟「已回到原生 Codex」同框。 */
  it("keeps the verdict copy from claiming what is running", () => {
    for (const locale of APP_LOCALES) {
      const bundle = localeSources[locale] ?? "";
      const mean = bundle.slice(
        bundle.indexOf('"protocol": {'),
        bundle.indexOf('"delta": {'),
      );
      for (const claim of ["正在跑", "正在运行", "は稼働中", "Enhanced is running"]) {
        expect(mean).not.toContain(claim);
      }
    }
  });

  /* 四個 blocker 加兩個差異攤開來比整張卡還長，而且浮層是 absolute 定位，
     撐破了不會有人接住捲軸。 */
  it("keeps the detail in a tray", () => {
    expect(statusBarSource).toMatch(/<details className="runtime-popover__tray">/);
    // 常駐的只有三行狀態；PID 與 digest 只在托盤裡出現。
    const resident = statusBarSource.slice(
      statusBarSource.indexOf("<dl>"),
      statusBarSource.indexOf("<details"),
    );
    expect(resident).not.toContain("officialChildPid");
    expect(resident).not.toContain("activeRuntimeDigest");
  });

  /* 判定本身不夠：使用者要看得到差在哪，才有辦法自己決定重不重要。 */
  it("shows the differences, not just the verdict", () => {
    expect(statusBarSource).toContain("details.map");
    expect(statusBarSource).toContain("detail.routed");
    const missing = APP_LOCALES.filter((locale) => {
      const keys = new Set(resourceKeySet(locale));
      return [
        "methodUnservable",
        "methodUnknownToDesktop",
        "fieldNewlyRequired",
        "fieldRemoved",
      ].some((kind) => !keys.has(`settings.page.enhancedRuntime.protocol.delta.${kind}`));
    });
    expect(missing).toEqual([]);
  });
});
