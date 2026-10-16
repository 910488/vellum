import { Btn, Cap, Card, Row, Rows, State, Tray } from "@/components/ui";
import { formatBytes } from './remoteHelpers';
import type { RemoteController } from './useRemoteController';
export function RemoteHostDetails({ model }: {
  model: RemoteController;
}) {
  const {
    t,
    selectedHostId,
    busy,
    probeErrors,
    release,
    desktopCodex,
    setPending,
    host,
    status,
    native,
    proxy,
    configurationKey,
    inventory,
    unknown,
    operationRunning,
    exportBundle,
    workSurface,
  } = model;
  return <>{/* ---------- 主機細節 ----------
                            原本是十九行同權重的狀態，常駐在主卡上，其中 Docker 還出現兩次
                            （一次來自 agent capabilities，一次來自 inventory）。
                            這是「很少問但問起來要問得完整」的資料，所以收起來、分四組。 */}
    <Card>
      <Cap>{t("remote.detail.title")}</Cap>
      <Tray label={t("remote.detail.host")}>
        <Rows>
          <Row label="SSH">{host?.validated ? t("remote.detail.sshResolved") : host?.validationError ?? t("remote.detail.sshUnresolved")}</Row>
          <Row label="Agent">{status?.agent ? `${status.agent.agentVersion} · protocol ${status.agent.agentProtocol}` : probeErrors[selectedHostId ?? ""] ?? status?.agentError ?? t("remote.detail.agentAbsent")}</Row>
          <Row label="OS">{status?.agent ? `${status.agent.capabilities.os} · ${status.agent.capabilities.arch}` : unknown}</Row>
          <Row label={t("remote.detail.platform")}>{inventory?.platform ?? status?.agent?.capabilities.platform ?? unknown}</Row>
          <Row label={t("remote.detail.proxyBackend")}>{inventory?.proxyBackend ?? status?.agent?.capabilities.proxyBackend ?? unknown}</Row>
          <Row label={t("remote.detail.persistence")}>{(inventory?.persistenceScope ?? status?.agent?.capabilities.persistenceScope) === "login"
            ? t("remote.detail.loginResident")
            : (inventory?.persistenceScope ?? status?.agent?.capabilities.persistenceScope) === "linger"
              ? t("remote.detail.lingerResident")
              : (inventory?.persistenceScope ?? status?.agent?.capabilities.persistenceScope) === "onDemand"
                ? t("remote.detail.onDemandResident")
                : unknown}</Row>
          <Row label={t("remote.detail.managedHome")}>{inventory?.managedCodexHome ?? status?.agent?.capabilities.managedCodexHome ?? native?.codexHome ?? unknown}</Row>
          {(inventory?.platform === "darwin-arm64" || status?.agent?.capabilities.platform === "darwin-arm64") && (<Row label={t("remote.detail.isolationLabel")}>{t("remote.detail.isolation")}</Row>)}
          {(status?.agent?.capabilities.os === "darwin" || status?.agent?.capabilities.os === "macos") && status?.agent?.capabilities.arch === "x86_64" && (<Row label={t("remote.detail.platform")}>{t("remote.blocker.intelMacUnsupported")}</Row>)}
          <Row label="CPU">{inventory?.system.cpuCores != null ? t("remote.detail.cores", { count: inventory.system.cpuCores }) : unknown}</Row>
          <Row label="Memory">{formatBytes(inventory?.system.memoryBytes, unknown)}</Row>
          <Row label="Disk">{inventory?.system.diskTotalBytes != null ? t("remote.detail.diskFree", { free: formatBytes(inventory.system.diskFreeBytes, unknown), total: formatBytes(inventory.system.diskTotalBytes, unknown) }) : unknown}</Row>
          {/* Docker 原本出現兩次：一次來自 agent capabilities，一次來自
  inventory，兩個來源在同一張卡上並列。留 inventory 那一份，
  agent 只當備援。 */}
          <Row label="Docker">{inventory?.docker.available
            ? `${inventory.docker.mode}${inventory.docker.serverVersion ? ` · v${inventory.docker.serverVersion}` : ""}${inventory.docker.context ? ` · ${inventory.docker.context}` : ""}`
            : status?.agent?.capabilities.dockerAvailable
              ? status.agent.capabilities.dockerMode
              : t("remote.detail.dockerAbsent")}</Row>
        </Rows>
      </Tray>
      <Tray label={t("remote.detail.runtime")}>
        <Rows>
          <Row label="Proxy">{proxy?.ready ? t("remote.detail.proxyReady", { image: proxy.image ?? "managed" }) : proxy?.running ? t("remote.detail.proxyNotReady") : t("remote.detail.proxyStopped")}</Row>
          {configurationKey && (<Row label={t("remote.detail.config")}>{t(configurationKey)}</Row>)}
          <Row label="Codex CLI">{native?.codexVersion ?? status?.agent?.capabilities.codexVersion ?? unknown}</Row>
          <Row label="Codex App CLI">{native?.cliLauncher?.ready ? native.cliLauncher.path ?? t("remote.detail.launcherLoginShell") : t("remote.detail.launcherBroken")}</Row>
          <Row label="Daemon">{native?.daemonRunning ? t("remote.detail.daemonRunning", { pid: native.daemonPid ?? "?" }) : t("remote.detail.daemonStopped")}</Row>
          <Row label={t("remote.detail.daemonOwner")}>{native ? `${native.daemonOwner} · ${native.durable ? t("remote.detail.durable") : t("remote.detail.notDurable")}` : unknown}</Row>
          {/* 沒值就是沒值。原本這裡填「由 Agent 探測，不接受 renderer 路徑」
  —— 那是給工程師的實作註記，不是給使用者的訊息。 */}
          <Row label="CODEX_HOME">{native?.codexHome ?? unknown}</Row>
          <Row label={t("remote.detail.codexSource")}>{inventory ? `${inventory.codex.source}${inventory.codex.version ? ` · ${inventory.codex.version}` : ""}` : unknown}</Row>
        </Rows>
      </Tray>
      <Tray label={t("remote.detail.version")}>
        <Rows>
          <Row label={t("remote.desktopCodex.desktop")}>{desktopCodex?.desktopVersion ?? unknown}</Row>
          <Row label={t("remote.desktopCodex.remote")}>{desktopCodex?.remoteVersion ?? unknown}</Row>
          <Row label={t("remote.desktopCodex.status")}>{desktopCodex ? t(`remote.desktopCodex.states.${desktopCodex.state}`) : t("common.loading")}</Row>
          <Row label={t("remote.detail.releaseTrust")}>{release?.ready ? `${release.releaseVersion ?? t("remote.detail.verified")} · ${release.trust}` : t("remote.detail.releaseUnverified", { trust: release?.trust ?? unknown })}</Row>
          <Row label={t("remote.detail.pinned")}>{release?.ready ? `Codex ${release.codexVersion} · Agent ${release.agentVersion}` : unknown}</Row>
        </Rows>
      </Tray>
      {inventory?.blockers.length ? (<div className="remote-maintenance-list" aria-label={t("remote.detail.inventoryBlockers")}>
        {inventory.blockers.map((blocker) => (<div key={blocker.code}>
          <State tone={blocker.repairable ? "warn" : "quiet"} label={blocker.code} />
          <span>{blocker.message}</span>
        </div>))}
      </div>) : null}
    </Card>

    {/* 進階只留重新部署與診斷包；安裝、更新與修復由 converge 處理。 */}
    <Card>
      <div className="remote-actions">
        <Btn soft onClick={() => setPending("bootstrap")} disabled={busy !== null || operationRunning || workSurface.primaryDisabled}>{t("remoteWork.redeploy")}</Btn>
        <Btn soft onClick={exportBundle} disabled={busy !== null}>{t("remote.act.bundle")}</Btn>
      </div>
    </Card>

  </>;
}
