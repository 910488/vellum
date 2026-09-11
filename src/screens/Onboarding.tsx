/**
 * 初次設定 —— 一張犢皮紙。
 *
 * ── 構想 ───────────────────────────────────────────────────
 *
 * vellum ＝ 犢皮紙。中世紀的犢皮紙太貴，寫滿了就把字刮掉重寫
 * （palimpsest，重寫本），而刮不乾淨的舊字跡會從底下透出來。
 *
 * 那就是這個程式做的事：上下文滿了不截斷，重寫一次，該留的留著。
 * 產品的核心隱喻長在名字裡。
 *
 * 所以這裡不做「精靈」，做那張紙：
 *
 *   · 不換頁。四幕寫在同一張紙上，後一幕把前一幕刮掉。
 *   · 刮掉的字留在左側頁邊 —— 那就是進度，不需要 stepper。
 *   · 還沒寫的部分是空白橫線 —— 那就是「還有幾步」，
 *     但你讀不到還沒寫的內容，因為它還不存在。
 *   · 沒有 logo。品牌是紙、格線、刮痕與襯線本身。
 *
 * ── 為什麼沒有應用程式圖示 ─────────────────────────────────
 *
 * Rail.tsx 早就把品牌區拿掉了，理由寫在那裡：「常駐在自己機器上的
 * 工具，使用者不需要每次抬頭確認它叫什麼。」把圖示貼回左上角是
 * 每個範本都做的事，而這個專案已經刻意刪過它一次。
 *
 * 名字還是要講 —— 但用寫的：序幕上「Vellum」以 132px 落在第一道
 * 格線上，像寫在紙上，不是一枚圓角方塊。那是品牌時刻，不是 logo 位。
 *
 * ── 版面 ───────────────────────────────────────────────────
 *
 * 左頁邊 + 文字欄，全部左對齊。置中堆疊是範本的版面；手稿的版面是
 * 一道寬頁邊配一欄正文，標題吊出頁邊（hanging indent）。
 *
 *   ┌──────────┬────────────────────────────────┐
 *   │ 一 認識  ╱│  它站在中間          ← 吊出頁邊  │
 *   │ 二 連線  ╱│                                │
 *   │ 三 功能   │  Codex ── Vellum ── 供應商      │
 *   │ ─────    │  ⋯                             │
 *   └──────────┴────────────────────────────────┘
 *     刮過的      正在寫的
 *
 * ── 排版慣例都用真的 ───────────────────────────────────────
 *
 *   folio        頁邊的漢字序號（一二三四），不是 01/02/03
 *   marginalia   第一幕的三個要點是眉批，不是三張並排卡
 *   register     第二幕是登記簿的列，不是卡片格
 *   glossary     第三幕是詞條（術語吊在頁邊），不是編號清單
 *   colophon     第四幕是牌記 —— 手稿末尾記「這份東西的狀態」
 *
 * 每一個都比它取代掉的那個範本元件更貼近語意。三個並排的卡片沒有
 * 閱讀順序；眉批有主從。編號清單假裝有步驟；詞條就是詞條。
 *
 * ── 動態 ───────────────────────────────────────────────────
 *
 * 字是由左往右寫出來的，所以進場是 clip-path 由左往右揭開，不是
 * 淡入。刮痕是一道由左往右畫過的橫線。全部只在換幕時發生一次，
 * 到位後靜止 —— base.css 已經為了對比度把散景光斑改成靜止的，
 * 同一條紀律在這裡也成立。全部包在 prefers-reduced-motion 裡。
 *
 * ── 這個檔案的邊界 ─────────────────────────────────────────
 *
 * 純畫面：不呼叫 api、不自己抓狀態。事實從 props 進、動作從 callback 出。
 */
import { useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { getLocalePreference, setLocalePreference } from "@/i18n";
import { type LocalePreference } from "@/i18n/locale";
import {
  OPENCODE_ZEN_BASE_URL,
  OPENCODE_ZEN_NAME,
  providerPresetDraft,
  type ProviderPreset,
} from "@/lib/providerPresets";
import { Btn, Row, Rows, State } from "@/components/ui";

/* ---------- 對外契約 ---------- */

export type ConnectKind = "chatgpt" | "grok" | "custom";

export interface OnboardingConnection {
  /** 已接上的帳號或端點名稱。空陣列 = 還沒接。 */
  accounts: string[];
  /** 這台機器上不存在這個選項（例如沒裝 Grok CLI）。填的是原因，會直接顯示。 */
  unavailable?: string | null;
  /** 官方登入流程進行中要顯示的一次性代碼。 */
  pending?: { code: string; uri: string } | null;
}

export interface CustomEndpointDraft {
  name: string;
  baseUrl: string;
  apiKey: string;
}

export interface OnboardingProps {
  /**
   * 0 = 序幕（空白的紙），1–4 = 四幕。由外面持有，才能在別處
   * （例如設定頁的「重看導覽」）翻到指定的一幕。
   */
  step: number;
  onStep: (next: number) => void;
  connections: Record<ConnectKind, OnboardingConnection>;
  /** 正在跑哪一家的登入流程。null = 沒有。 */
  busy: ConnectKind | null;
  error: string | null;
  proxyRunning: boolean;
  proxyBusy: boolean;
  onConnect: (kind: ConnectKind) => void;
  onCancelConnect: (kind: ConnectKind) => void;
  onAddCustom: (draft: CustomEndpointDraft) => void;
  onStartProxy: () => void;
  /** 走完（或中途離開）。外面負責寫下「已看過」並切到主畫面。 */
  onFinish: () => void;
}

/* 幕表。序幕不在裡面 —— 紙還是空白的時候沒有東西可以刮。
   序號不是裝飾：01/02/03 是規格書的編號，葉碼是手稿的編號，而且它跟
   這套語彙的明體是同一個聲音。所以葉碼跟著語言走（漢字／羅馬數字），
   不寫死漢字 —— 英文介面裡的「一二三四」不是同一個聲音。 */
const ACTS = [
  { folioKey: "onboarding.folio.1", labelKey: "onboarding.acts.what" },
  { folioKey: "onboarding.folio.2", labelKey: "onboarding.acts.connect" },
  { folioKey: "onboarding.folio.3", labelKey: "onboarding.acts.features" },
  { folioKey: "onboarding.folio.4", labelKey: "onboarding.acts.launch" },
] as const;

export function Onboarding({
  step,
  onStep,
  connections,
  busy,
  error,
  proxyRunning,
  proxyBusy,
  onConnect,
  onCancelConnect,
  onAddCustom,
  onStartProxy,
  onFinish,
}: OnboardingProps) {
  const { t } = useTranslation();
  const linkedNames = (Object.keys(connections) as ConnectKind[]).flatMap(
    (kind) => connections[kind].accounts,
  );
  const blank = step === 0;

  return (
    <div className="sheet" data-blank={blank || undefined}>
      {/* 語言只出現在序幕。這是整個 app 唯一一次「你還沒告訴我你讀哪種文字」
          的時刻 —— Windows OOBE 也把語言放在第一屏，選完就收起來。
          往後要改在「設定」頁，不需要在四幕上都掛一個常駐控制項。

          之前這裡什麼都沒有：四個語言的字翻好了，但初次設定只能靠
          作業系統語言猜。猜錯的人第一眼就讀不懂，而那正是最不該讀不懂的一屏。 */}
      {blank ? <LanguagePick /> : null}

      {/* 左頁邊。序幕上它是空的 —— 紙還沒被寫過。 */}
      <div className="sheet__margin">
        <Palimpsest step={step} onStep={onStep} />
      </div>

      {/* key 綁 step：換幕時整欄重新掛載，字才會重新被「寫」一次。 */}
      <div className="sheet__text" key={step}>
        {error ? <p className="note sheet__error">{error}</p> : null}
        {blank ? <Overture onStart={() => onStep(1)} onSkip={onFinish} /> : null}
        {step === 1 ? <ActWhat /> : null}
        {step === 2 ? (
          <ActConnect
            connections={connections}
            busy={busy}
            onConnect={onConnect}
            onCancelConnect={onCancelConnect}
            onAddCustom={onAddCustom}
          />
        ) : null}
        {step === 3 ? <ActFeatures /> : null}
        {step === 4 ? (
          <ActLaunch
            proxyRunning={proxyRunning}
            proxyBusy={proxyBusy}
            linked={linkedNames}
            onStartProxy={onStartProxy}
          />
        ) : null}
      </div>

      {/* 頁腳。序幕自己有動作（跟大字同一組），所以不出頁腳 ——
          頁腳出現＝你已經在紙上了。 */}
      {blank ? null : (
        <div className="sheet__foot">
          <button type="button" className="sheet__back" onClick={() => onStep(step - 1)}>
            {t("onboarding.ui.back")}
          </button>
          <div className="sheet__forward">
            {step === 2 && !linkedNames.length ? (
              <span className="sheet__aside">
                {t("onboarding.ui.skipHint")}
              </span>
            ) : null}
            {step < 4 ? (
              <Btn onClick={() => onStep(step + 1)}>
                {step === 2 && !linkedNames.length ? t("onboarding.ui.skip") : t("onboarding.ui.continue")}
              </Btn>
            ) : (
              <Btn onClick={onFinish}>
                {proxyRunning ? t("onboarding.ui.enter") : t("onboarding.ui.enterWithoutProxy")}
              </Btn>
            )}
          </div>
        </div>
      )}
    </div>
  );
}

/* 語言選單。
 *
 * 用原生 <select>：鍵盤操作、螢幕閱讀器、輸入法、觸控的長清單捲動
 * 全部由作業系統處理，自己刻一個下拉一定會漏掉其中幾樣。外觀用
 * appearance: none 收乾淨，剩下的箭頭在 CSS 畫 —— 語意留原生，外觀自己來。
 *
 * 選項一律用該語言自己的寫法（繁體中文／简体中文／English／日本語），
 * 不翻譯。日本人在中文介面裡找「日文」是找不到的，找「日本語」才找得到 ——
 * 這正好是這個控制項存在的整個理由。（這些 key 沿用設定頁那一組，
 * 不新增字串。）
 *
 * 語言是全域偏好不是這一頁的狀態，所以直接讀寫 i18n，不往 props 加參數 ——
 * 這個畫面的接線負擔已經夠重了。
 */
function LanguagePick() {
  const { t } = useTranslation();
  const [pref, setPref] = useState<LocalePreference>(getLocalePreference());

  const options: LocalePreference[] = ["system", "zh-TW", "zh-CN", "en", "ja"];

  return (
    <div className="sheet__lang">
      <span className="langpick">
        <select
          aria-label={t("settings.language.title")}
          value={pref}
          onChange={(event) => {
            const next = event.target.value as LocalePreference;
            setPref(next);
            void setLocalePreference(next);
          }}
        >
          {options.map((code) => (
            <option key={code} value={code}>
              {t(`settings.language.option.${code}`)}
            </option>
          ))}
        </select>
      </span>
    </div>
  );
}

/* ============================================================
   頁邊：重寫本
   ------------------------------------------------------------
   這是進度，但它不是 stepper。差別在**還沒發生的那幾步讀不到**：
   stepper 會把四個標籤一次攤開，而這張紙只顯示已經寫過的字，
   沒寫的地方就是一道空白橫線。

   空白橫線仍然回答「還有幾步」—— 那是使用者真正需要的那半個資訊；
   另外半個（下一步叫什麼）本來就該留到下一步。

   刮過的字仍然可讀而且可以點回去：它們被刮掉了，不是被刪掉了。
   ============================================================ */

function Palimpsest({ step, onStep }: { step: number; onStep: (next: number) => void }) {
  const { t } = useTranslation();
  return (
    <ol className="palimpsest">
      {ACTS.map((act, index) => {
        const at = index + 1;

        /* 還沒寫到。只留一道空白的格線 —— 讀不到內容，但看得到還有幾行。 */
        if (at > step) {
          return (
            <li className="palimpsest__unwrit" key={act.labelKey}>
              <span className="sr-only">{t("onboarding.ui.unwritten", { step: at })}</span>
            </li>
          );
        }

        const scraped = at < step;
        return (
          <li className="palimpsest__line" data-scraped={scraped || undefined} key={act.labelKey}>
            <button
              type="button"
              className="palimpsest__hit"
              aria-current={scraped ? undefined : "step"}
              disabled={!scraped}
              onClick={() => scraped && onStep(at)}
            >
              <span className="folio" aria-hidden="true">
                {t(act.folioKey)}
              </span>
              <span className="palimpsest__label">{t(act.labelKey)}</span>
              {scraped ? <span className="sr-only">{t("onboarding.ui.visited")}</span> : null}
            </button>
          </li>
        );
      })}
    </ol>
  );
}

/* ============================================================
   序幕 —— 空白的紙
   ------------------------------------------------------------
   紙上只有一道格線，字寫在線上。沒有圖示、沒有標語卡、不置中。

   典故那三行是連著的一段話，不是「大字 + slogan」：
   先講犢皮紙是什麼，再講這個程式做同一件事。第二句才是重點 ——
   不接上去的話，第一句只是一個好聽的字源。
   ============================================================ */

function Overture({ onStart, onSkip }: { onStart: () => void; onSkip: () => void }) {
  const { t } = useTranslation();
  return (
    <div className="overture">
      {/* 逐字揭開需要一個字元一個節點，但讀屏軟體不該聽到
          「V、e、l、l、u、m」—— 字元全部隱藏，名字掛在容器上。 */}
      <h1 className="writ" aria-label="Vellum">
        {"Vellum".split("").map((ch, index) => (
          <span
            className="writ__ch"
            style={{ ["--i" as string]: index }}
            aria-hidden="true"
            key={`${ch}-${index}`}
          >
            {ch}
          </span>
        ))}
      </h1>

      <div className="overture__gloss">
        {/* 引語與後文之間的空白交給 CSS，不寫在字串裡。
            拉丁文需要一個詞距；漢字的句號「。」自帶右側空白，
            再補一個空格就會多出半個字的洞。這是 :lang() 的事，
            不是翻譯者的事 —— 而寫死的那個空格四個語言只能對一個。 */}
        <p>
          <b>{t("onboarding.overture.lead")}</b>
          {t("onboarding.overture.history")}
        </p>
        <p className="overture__turn">
          {t("onboarding.overture.mission")}
        </p>
      </div>

      <p className="overture__what">
        {t("onboarding.overture.subtitle")}
      </p>

      <div className="overture__go">
        <Btn onClick={onStart}>{t("onboarding.ui.start")}</Btn>
        {/* 重裝或重看的人不該被四幕擋住。做成文字不做成按鈕 ——
            它不該跟「開始」搶。 */}
        <button type="button" className="sheet__back" onClick={onSkip}>
          {t("onboarding.ui.skipSetup")}
        </button>
      </div>
    </div>
  );
}

/* 每一幕的標題。字由左往右揭開 —— 寫字就是這個方向，淡入不是。
   左緣做了光學補償（負的 text-indent）：襯線大寫字母的左側白空間
   比方框多，切齊方框反而看起來凸出來。 */
function ActHead({ title, lead }: { title: string; lead?: string }) {
  return (
    <header className="acthead">
      <h2 className="acthead__title">{title}</h2>
      {lead ? <p className="acthead__lead">{lead}</p> : null}
    </header>
  );
}

/* ============================================================
   第一幕 —— 它站在中間
   ============================================================ */

function ActWhat() {
  const { t } = useTranslation();
  return (
    <>
      <ActHead
        title={t("onboarding.what.title")}
        lead={t("onboarding.what.lead")}
      />

      {/* 不用三個方塊加箭頭。那句話本身就是一條線，
          而「站在中間」是可以直接排出來的 ——
          兩端小、中間大，中間那個字就在線的中央。 */}
      <div
        className="axis"
        role="img"
        aria-label={t("onboarding.what.axisAria")}
      >
        <span className="axis__end">
          <b className="literal">Codex CLI</b>
          <i>{t("onboarding.what.client")}</i>
        </span>
        {/* 連線是版面的一部分，不是圖示。兩道實際存在的格線把
            「中間」這件事排出來 —— 中間那個字就落在紙的中線上。 */}
        <i className="axis__rule" aria-hidden="true" />
        <span className="axis__mid">
          <b>Vellum</b>
          <i>{t("onboarding.what.local")}</i>
        </span>
        <i className="axis__rule" aria-hidden="true" />
        <span className="axis__end axis__end--right">
          <b className="literal">{t("onboarding.what.axisTargets")}</b>
          <i>{t("onboarding.what.answering")}</i>
        </span>
      </div>

      {/* 眉批。不是三張等重的卡 —— 術語吊在頁邊，說明接在後面，
          長短不一，眼睛照著讀下去。三個並排的方塊沒有閱讀順序。 */}
      <dl className="marginalia">
        <dt>{t("onboarding.what.noConfigTitle")}</dt>
        <dd>
          {t("onboarding.what.noConfigBody")}
        </dd>

        <dt>{t("onboarding.what.noTruncateTitle")}</dt>
        <dd>
          {t("onboarding.what.noTruncateBody")}
        </dd>

        <dt>{t("onboarding.what.reviewTitle")}</dt>
        <dd>
          {t("onboarding.what.reviewBody")}
        </dd>
      </dl>

      {/* 第一個障礙不是功能，是信任 —— 一個攔下你全部對話的東西。
          所以這句話單獨放，用註腳那道筆畫收尾。 */}
      <p className="note">
        {t("onboarding.what.privacy")}
      </p>
    </>
  );
}

/* ============================================================
   第二幕 —— 接上一家
   ------------------------------------------------------------
   登記簿，不是卡片格。三家是同一種東西的三個條目，排成列才對得齊，
   也才裝得下「登入中要顯示一組代碼」這種會長高的狀態 ——
   卡片格會被最高的那張撐開，列不會。
   ============================================================ */

function ActConnect({
  connections,
  busy,
  onConnect,
  onCancelConnect,
  onAddCustom,
}: {
  connections: Record<ConnectKind, OnboardingConnection>;
  busy: ConnectKind | null;
  onConnect: (kind: ConnectKind) => void;
  onCancelConnect: (kind: ConnectKind) => void;
  onAddCustom: (draft: CustomEndpointDraft) => void;
}) {
  const { t } = useTranslation();
  const [customOpen, setCustomOpen] = useState(false);

  return (
    <>
      <ActHead
        title={t("onboarding.connect.title")}
        lead={t("onboarding.connect.lead")}
      />

      <div className="register">
        <Entry
          kind="chatgpt"
          name="ChatGPT"
          claim={t("onboarding.connect.chatgptClaim")}
          detail={t("onboarding.connect.chatgptDetail")}
          action={t("onboarding.ui.login")}
          conn={connections.chatgpt}
          busy={busy === "chatgpt"}
          disabledByOther={busy !== null && busy !== "chatgpt"}
          onConnect={() => onConnect("chatgpt")}
          onCancel={() => onCancelConnect("chatgpt")}
        />
        <Entry
          kind="grok"
          name="Grok"
          claim={t("onboarding.connect.grokClaim")}
          detail={t("onboarding.connect.grokDetail")}
          action={t("onboarding.ui.login")}
          conn={connections.grok}
          busy={busy === "grok"}
          disabledByOther={busy !== null && busy !== "grok"}
          onConnect={() => onConnect("grok")}
          onCancel={() => onCancelConnect("grok")}
        />
        <Entry
          kind="custom"
          name={t("onboarding.connect.customName")}
          claim={t("onboarding.connect.customClaim")}
          detail={t("onboarding.connect.customDetail")}
          action={customOpen ? t("onboarding.ui.collapse") : t("onboarding.ui.fillEndpoint")}
          conn={connections.custom}
          busy={busy === "custom"}
          disabledByOther={busy !== null && busy !== "custom"}
          onConnect={() => setCustomOpen((open) => !open)}
          onCancel={() => setCustomOpen(false)}
          expand={
            customOpen ? <CustomForm onSubmit={onAddCustom} busy={busy === "custom"} /> : null
          }
        />
      </div>

      <p className="note">
        {t("onboarding.connect.credentialNote")}
      </p>
    </>
  );
}

function Entry({
  kind,
  name,
  claim,
  detail,
  action,
  conn,
  busy,
  disabledByOther,
  onConnect,
  onCancel,
  expand,
}: {
  kind: ConnectKind;
  name: string;
  claim: string;
  detail: string;
  action: string;
  conn: OnboardingConnection;
  busy: boolean;
  disabledByOther: boolean;
  onConnect: () => void;
  onCancel: () => void;
  /** 就地展開的東西（自訂端點的表單）。不跳頁：中途把人送走，
      回來時他已經忘記自己在做什麼了。 */
  expand?: ReactNode;
}) {
  const { t } = useTranslation();
  const linked = conn.accounts.length > 0;
  const blocked = Boolean(conn.unavailable);

  /* 名字是登記簿的第一欄，所以它必須是**真的一欄**（subgrid），
     不能是「第一個 flex 項目 + 底下全部縮排 6.5rem」。後者在繁中剛好
     對得上（三個名字都是 3–4 個字），一換成英文的 "Custom endpoint"
     欄就撐寬、縮排卻是死的，整份登記簿的第二欄會歪掉。

     欄寬交給 max-content：它自己會長到四個語言裡最長的那個名字。 */
  return (
    <div className="entry" data-linked={linked || undefined}>
      <span className="entry__name">{name}</span>
      <div className="entry__body">
      <div className="entry__line">
        <span className="entry__claim">{claim}</span>

        {/* 狀態是這一列的註記，不是一顆按鈕 —— 沿用 State 的筆觸三態。
            登入流程跑起來之後不能還寫「未連線」：畫面上正擺著一組代碼
            要人去輸入，兩者互相打臉。 */}
        <span className="entry__state">
          {linked ? (
            <State tone="ok" label={t("onboarding.ui.connected")} />
          ) : blocked ? (
            <State tone="quiet" label={t("onboarding.ui.unavailable")} />
          ) : conn.pending ? (
            <State tone="warn" label={t("onboarding.ui.waitingAuth")} />
          ) : (
            <State tone="warn" label={t("onboarding.ui.notConnected")} />
          )}
        </span>

        <span className="entry__act">
          {conn.pending ? (
            <Btn soft mini onClick={onCancel}>
              {t("common.cancel")}
            </Btn>
          ) : (
            <Btn
              soft={linked}
              mini
              disabled={blocked || disabledByOther}
              onClick={onConnect}
            >
              {busy && kind !== "custom" ? t("onboarding.ui.waitingAuthEllipsis") : linked ? t("onboarding.ui.addAnother") : action}
            </Btn>
          )}
        </span>
      </div>

      <p className="entry__detail">{detail}</p>

      {linked ? (
        <ul className="entry__accts">
          {conn.accounts.map((account) => (
            <li key={account} className="literal">
              {account}
            </li>
          ))}
        </ul>
      ) : null}

      {blocked ? <p className="entry__blocked">{conn.unavailable}</p> : null}

      {/* 一次性代碼。整個初次設定只有這一個東西需要使用者照著輸入，
          所以它是唯一一塊刻意做大的等寬排版。 */}
      {conn.pending ? (
        <div className="plate">
          <span className="plate__cap">{t("onboarding.ui.enterCode")}</span>
          <strong className="plate__code">{conn.pending.code}</strong>
          <span className="plate__uri literal">{conn.pending.uri}</span>
        </div>
      ) : null}

      {expand}
      </div>
    </div>
  );
}

function CustomForm({
  onSubmit,
  busy,
}: {
  onSubmit: (draft: CustomEndpointDraft) => void;
  busy: boolean;
}) {
  const { t } = useTranslation();
  const [preset, setPreset] = useState<ProviderPreset>("custom");
  const [name, setName] = useState("");
  const [baseUrl, setBaseUrl] = useState("");
  const [apiKey, setApiKey] = useState("");
  const ready = preset === "opencodeZen"
    ? true
    : name.trim().length > 0 && baseUrl.trim().length > 0;

  function choosePreset(next: ProviderPreset) {
    const draft = providerPresetDraft(next);
    setPreset(next);
    setName(draft.name);
    setBaseUrl(draft.baseUrl);
  }

  return (
    <form
      className="entry__form"
      onSubmit={(event) => {
        event.preventDefault();
        if (ready && !busy) {
          onSubmit({
            name: preset === "opencodeZen" ? OPENCODE_ZEN_NAME : name.trim(),
            baseUrl: preset === "opencodeZen" ? OPENCODE_ZEN_BASE_URL : baseUrl.trim(),
            apiKey,
          });
        }
      }}
    >
      <label className="field">
        <span className="field__label">{t("onboarding.ui.providerType")}</span>
        <select
          className="input"
          value={preset}
          onChange={(event) => choosePreset(event.target.value as ProviderPreset)}
        >
          <option value="custom">{t("onboarding.ui.customEndpoint")}</option>
          <option value="opencodeZen">OpenCode Zen</option>
        </select>
      </label>
      {preset === "opencodeZen" ? (
        <p className="field__hint">{t("onboarding.ui.opencodeHint")}</p>
      ) : null}
      {preset === "custom" ? <label className="field">
        <span className="field__label">{t("onboarding.ui.displayName")}</span>
        <input
          className="input"
          value={name}
          placeholder={t("onboarding.ui.displayNamePlaceholder")}
          onChange={(event) => setName(event.target.value)}
        />
      </label> : null}
      {preset === "custom" ? <label className="field">
        <span className="field__label">{t("onboarding.ui.endpoint")}</span>
        <input
          className="input"
          value={baseUrl}
          placeholder="https://…/v1"
          onChange={(event) => setBaseUrl(event.target.value)}
        />
      </label> : null}
      <label className="field">
        <span className="field__label">
          {preset === "opencodeZen" ? t("onboarding.ui.opencodeApiKey") : "API key"}
        </span>
        <input
          className="input"
          type="password"
          value={apiKey}
          placeholder={
            preset === "opencodeZen"
              ? t("onboarding.ui.opencodeApiKeyPlaceholder")
              : t("onboarding.ui.optionalPlaceholder")
          }
          onChange={(event) => setApiKey(event.target.value)}
        />
      {preset === "custom" ? <p className="field__hint">{t("onboarding.ui.endpointHint")}</p> : null}
      </label>
      <div className="entry__formact">
        <Btn type="submit" disabled={!ready || busy}>
          {busy ? t("onboarding.ui.probing") : t("onboarding.ui.probeAndAdd")}
        </Btn>
      </div>
    </form>
  );
}

/* ============================================================
   第三幕 —— 這些已經開著
   ------------------------------------------------------------
   詞條，不是編號清單。01/02/03 假裝這四件事有先後，但它們沒有 ——
   它們是四個同時成立的事實。術語吊在頁邊，釋義接在後面，
   末尾用一個小記號標「之後在哪一頁調」，像手稿的出處註。
   ============================================================ */

const GLOSSARY = [
  {
    termKey: "onboarding.features.reviewTerm",
    whereKey: "onboarding.features.settingsPage",
    bodyKey: "onboarding.features.reviewBody",
  },
  {
    termKey: "onboarding.features.resetTerm",
    whereKey: "onboarding.features.modelsPage",
    bodyKey: "onboarding.features.resetBody",
  },
  {
    termKey: "onboarding.features.sessionTerm",
    whereKey: "onboarding.features.todayPage",
    bodyKey: "onboarding.features.sessionBody",
  },
] as const;

function ActFeatures() {
  const { t } = useTranslation();
  return (
    <>
      <ActHead
        title={t("onboarding.features.title")}
        lead={t("onboarding.features.lead")}
      />

      <dl className="glossary">
        {GLOSSARY.map((entry, index) => (
          <div className="glossary__row" style={{ ["--i" as string]: index }} key={entry.whereKey}>
            <dt className="glossary__term">
              {t(entry.termKey).split("\n").map((line) => (
                <span key={line}>{line}</span>
              ))}
            </dt>
            <dd className="glossary__def">
              {t(entry.bodyKey)}
              {/* 「之後在哪裡調」是這一幕真正的產出 —— 功能講得再清楚，
                  找不到入口就等於沒講。做成句末的出處註，不另起一行。 */}
              <span className="glossary__where">「{t(entry.whereKey)}」頁</span>
            </dd>
          </div>
        ))}
      </dl>
    </>
  );
}

/* ============================================================
   第四幕 —— 牌記
   ------------------------------------------------------------
   colophon：手稿末尾那一段，記這份東西是誰做的、什麼狀態。
   最後一幕該回答的正好就是這個 —— 不是慶祝，是「按下去之後呢」。
   ============================================================ */

function ActLaunch({
  proxyRunning,
  proxyBusy,
  linked,
  onStartProxy,
}: {
  proxyRunning: boolean;
  proxyBusy: boolean;
  /** 已接上的供應商名稱，攤平過。空陣列 = 一家都沒接。 */
  linked: string[];
  onStartProxy: () => void;
}) {
  const { t } = useTranslation();
  return (
    <>
      <ActHead
        title={proxyRunning ? t("onboarding.launch.runningTitle") : t("onboarding.launch.title")}
        lead={
          proxyRunning
            ? t("onboarding.launch.runningLead")
            : t("onboarding.launch.lead")
        }
      />

      {proxyRunning ? null : (
        <div className="launch">
          <Btn onClick={onStartProxy} disabled={proxyBusy}>
            {proxyBusy ? t("common.processing") : t("onboarding.launch.startProxy")}
          </Btn>
          {!linked.length ? (
            <span className="sheet__aside">
              {t("onboarding.launch.noProviderHint")}
            </span>
          ) : null}
        </div>
      )}

      {/* 牌記。「已開著的 Codex 視窗要重啟」是最常見的第一個卡關點，
          寫在這裡最省事 —— 這三列不是填充物。 */}
      <div className="colophon">
        <Rows>
          <Row label={t("onboarding.launch.connectedProviders")}>{linked.length ? linked.join(t("onboarding.ui.providerSeparator")) : t("onboarding.ui.notConnectedYet")}</Row>
          <Row label={t("onboarding.launch.howToStart")}>
            {t("onboarding.launch.howToStartValue")}
          </Row>
          <Row label={t("onboarding.launch.howToStop")}>{t("onboarding.launch.howToStopValue")}</Row>
          <Row label={t("onboarding.launch.reopenGuide")}>{t("onboarding.launch.reopenGuideValue")}</Row>
        </Rows>
      </div>
    </>
  );
}
