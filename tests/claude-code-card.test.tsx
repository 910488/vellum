import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "i18next";
import "@/i18n";
import { ClaudeCodeCard } from "@/components/ClaudeCodeCard";
import { claudeCodeApi, type ClaudeCodeStatus } from "@/lib/claudeCode";

vi.mock("@/lib/claudeCode", () => ({ claudeCodeApi: { status: vi.fn(), recentProjects: vi.fn(), start: vi.fn(), stop: vi.fn(), select: vi.fn(), login: vi.fn(), cancelLogin: vi.fn(), remove: vi.fn(), usage: vi.fn(), prepareCertificate: vi.fn(), trustCertificate: vi.fn(), restoreTrust: vi.fn() } }));

function status(): ClaudeCodeStatus {
  return { running: false, project: null, port: null, settingsMatch: false, recoveryRequired: false, notice: null,
    trust: { supported: false, sha256: null, trusted: false, managed: false, recoveryRequired: false },
    routing: { activeRequests: 0, desktopRequests: 0, records: [] },
    accounts: { selected: "B", accounts: [{ id: "B", label: "Account B", expiresAt: 9999999999, reauthorizeRequired: false }], loginPending: false, loginError: null } };
}
beforeEach(async () => { await i18n.changeLanguage("zh-TW"); vi.clearAllMocks(); vi.mocked(claudeCodeApi.status).mockResolvedValue(status()); vi.mocked(claudeCodeApi.recentProjects).mockResolvedValue([]);
  vi.spyOn(HTMLDialogElement.prototype, "showModal").mockImplementation(function (this: HTMLDialogElement) { this.setAttribute("open", ""); });
  vi.spyOn(HTMLDialogElement.prototype, "close").mockImplementation(function (this: HTMLDialogElement) { this.removeAttribute("open"); });
});
afterEach(cleanup);
describe("Claude Desktop Code", () => {
  it("does not report inference connected for a rejected request or a token estimate", async () => {
    const value = status(); value.running = true; value.routing.desktopRequests = 2;
    value.routing.records = [
      { id: "r", sessionHash: "s", accountId: "B", operation: "messages", state: "upstream_rejected", upstreamStatus: 401 },
      { id: "c", sessionHash: "s", accountId: "B", operation: "count_tokens", state: "completed", upstreamStatus: 200 },
    ];
    vi.mocked(claudeCodeApi.status).mockResolvedValue(value);
    render(<ClaudeCodeCard active />);
    await screen.findByText(i18n.t("claudeCode.waiting"));
    expect(screen.queryByText(i18n.t("claudeCode.connected"))).toBeNull();
  });
  it("starts browser login and exposes cancellation without collecting credentials", async () => {
    const value = status();
    vi.mocked(claudeCodeApi.login).mockImplementation(async () => { value.accounts.loginPending = true; });
    vi.mocked(claudeCodeApi.status).mockImplementation(async () => value);
    render(<ClaudeCodeCard active />);
    const login = await screen.findByText(i18n.t("claudeCode.login"));
    fireEvent.click(login);
    await screen.findByText(i18n.t("claudeCode.loginPending"));
    expect(claudeCodeApi.login).toHaveBeenCalledOnce();
    expect(screen.queryByLabelText(/token|password|密碼/i)).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: i18n.t("common.cancel") }));
    await waitFor(() => expect(claudeCodeApi.cancelLogin).toHaveBeenCalledOnce());
  });
  it("shows both remaining quota windows and selects the next inference account", async () => {
    const value = status(); value.accounts.selected = null;
    value.accounts.accounts[0]!.usage = { observedAt: 1, fiveHour: { utilization: 62, resetsAt: null }, sevenDay: { utilization: 27, resetsAt: null } };
    vi.mocked(claudeCodeApi.status).mockResolvedValue(value);
    render(<ClaudeCodeCard active />);
    await screen.findByText("38%");
    expect(screen.getByText("73%")).toBeTruthy();
    expect(screen.getAllByRole("progressbar").map((bar) => bar.getAttribute("aria-valuenow"))).toEqual(["38", "73"]);
    fireEvent.click(screen.getByText(i18n.t("models.ui.account.useThis")));
    await waitFor(() => expect(claudeCodeApi.select).toHaveBeenCalledWith("B"));
    expect(claudeCodeApi.start).not.toHaveBeenCalled();
  });
  it("asks for confirmation before removing an unselected account", async () => {
    const value = status(); value.accounts.selected = null;
    vi.mocked(claudeCodeApi.status).mockResolvedValue(value);
    render(<ClaudeCodeCard active />);
    fireEvent.click(await screen.findByRole("button", { name: i18n.t("common.remove") }));
    expect(claudeCodeApi.remove).not.toHaveBeenCalled();
    const dialog = await screen.findByRole("dialog");
    fireEvent.click(within(dialog).getByRole("button", { name: i18n.t("common.remove") }));
    await waitFor(() => expect(claudeCodeApi.remove).toHaveBeenCalledWith("B"));
  });
  it("distinguishes actual inference from token estimates in routing evidence", async () => {
    const value=status(); value.routing.records=[
      {id:"m",sessionHash:"same-session",accountId:"B",operation:"messages",state:"completed",upstreamStatus:200},
      {id:"c",sessionHash:"same-session",accountId:"B",operation:"count_tokens",state:"completed",upstreamStatus:200},
    ];
    vi.mocked(claudeCodeApi.status).mockResolvedValue(value);
    render(<ClaudeCodeCard active />);
    await screen.findByText(/Account B · 推論 ·/);
    expect(screen.getByText(/Account B · Token 估算 ·/)).toBeTruthy();
  });
  it("requires explicit fingerprint-bound consent before Windows trust and project routing", async () => {
    const value = status(); value.trust = { supported: true, sha256: "reviewed-fingerprint", trusted: false, managed: false, recoveryRequired: false };
    vi.mocked(claudeCodeApi.status).mockResolvedValue(value);
    render(<ClaudeCodeCard active />);
    await screen.findByText("reviewed-fingerprint");
    fireEvent.change(screen.getByLabelText("專案資料夾"), { target: { value: "E:\\project" } });
    expect((screen.getByText("接入此專案") as HTMLButtonElement).disabled).toBe(true);
    expect((screen.getByText("暫時信任此 CA") as HTMLButtonElement).disabled).toBe(true);
    fireEvent.click(screen.getByLabelText("我同意為目前 Windows 使用者暫時信任此 CA"));
    fireEvent.click(screen.getByText("暫時信任此 CA"));
    await waitFor(() => expect(claudeCodeApi.trustCertificate).toHaveBeenCalledWith("reviewed-fingerprint"));
  });
  it("offers recovery when trust remains without any project lease", async () => {
    const value = status(); value.trust = { supported: true, sha256: "owned", trusted: true, managed: true, recoveryRequired: true };
    vi.mocked(claudeCodeApi.status).mockResolvedValue(value);
    render(<ClaudeCodeCard active />);
    await screen.findByText("移除本次臨時信任");
    fireEvent.click(screen.getByText("移除本次臨時信任"));
    await waitFor(() => expect(claudeCodeApi.restoreTrust).toHaveBeenCalledOnce());
  });
  it("limits scope and sends the explicitly selected project to the backend", async () => {
    render(<ClaudeCodeCard active />);
    await screen.findByText("已停用");
    expect(screen.getByText(/本機 Code 工作階段/)).toBeTruthy();
    fireEvent.change(screen.getByLabelText("專案資料夾"), { target: { value: "E:\\project" } });
    fireEvent.click(screen.getByText("接入此專案"));
    await waitFor(() => expect(claudeCodeApi.start).toHaveBeenCalledWith("E:\\project"));
  });
  it("shows waiting instead of claiming that a running listener proves Desktop integration", async () => {
    const value = status(); value.running = true; value.project = "E:\\project"; value.settingsMatch = true;
    vi.mocked(claudeCodeApi.status).mockResolvedValue(value);
    render(<ClaudeCodeCard active />);
    await screen.findByText("等待本機 Desktop Code");
    expect(screen.queryByText("接入此專案")).toBeNull();
    expect(screen.getByText("E:\\project")).toBeTruthy();
  });
  it("requires an explicit immediate stop while inference is active and reports errors", async () => {
    const value = status(); value.running = true; value.project = "E:\\project"; value.routing.activeRequests = 1;
    vi.mocked(claudeCodeApi.status).mockResolvedValue(value);
    vi.mocked(claudeCodeApi.stop).mockRejectedValue(new Error("settings conflict"));
    render(<ClaudeCodeCard active />);
    await screen.findByText("取消推論並還原");
    expect((screen.getByText("停用並還原設定") as HTMLButtonElement).disabled).toBe(true);
    await act(async () => { fireEvent.click(screen.getByText("取消推論並還原")); });
    expect(claudeCodeApi.stop).toHaveBeenCalledWith(true);
    await screen.findByText(/settings conflict/);
  });
  it("distinguishes TLS rejection before the first HTTP inference from a waiting listener", async () => {
    const value = status(); value.running = true; value.settingsMatch = true; value.routing.connectRequests = 1; value.routing.tlsFailures = 1; value.routing.lastError = "client_ca_untrusted";
    vi.mocked(claudeCodeApi.status).mockResolvedValue(value);
    render(<ClaudeCodeCard active />);
    await screen.findByText("Code 尚未信任代理憑證");
    expect(screen.queryByText("已觀察到 Code 請求")).toBeNull();
  });
  it("routes back to the Desktop account from its own row", async () => {
    render(<ClaudeCodeCard active />);
    await screen.findByText("Account B");
    fireEvent.click(screen.getByText(i18n.t("models.ui.account.useThis")));
    await waitFor(() => expect(claudeCodeApi.select).toHaveBeenCalledWith(null));
  });
  it("reads usage only from the page refresh and always reports completion", async () => {
    const value = status(); value.accounts.accounts.push({ id: "C", label: "Account C", expiresAt: 1, reauthorizeRequired: true });
    vi.mocked(claudeCodeApi.status).mockResolvedValue(value);
    vi.mocked(claudeCodeApi.usage).mockRejectedValue(new Error("usage unavailable"));
    const onRefreshed = vi.fn();
    const { rerender } = render(<ClaudeCodeCard active refreshVersion={0} onRefreshed={onRefreshed} />);
    await screen.findByText("Account B");
    expect(claudeCodeApi.usage).not.toHaveBeenCalled();
    expect(screen.queryByText(/更新訂閱用量/)).toBeNull();
    rerender(<ClaudeCodeCard active refreshVersion={1} onRefreshed={onRefreshed} />);
    await waitFor(() => expect(onRefreshed).toHaveBeenCalledOnce());
    expect(claudeCodeApi.usage).toHaveBeenCalledTimes(1);
    expect(claudeCodeApi.usage).toHaveBeenCalledWith("B");
    await screen.findByText(/usage unavailable/);
  });
  it("offers recent Desktop Code folders and still accepts another folder", async () => {
    vi.mocked(claudeCodeApi.recentProjects).mockResolvedValue([
      { path: "E:\\alpha", sessions: 3, lastActivityAt: 2 },
      { path: "E:\\beta", sessions: 1, lastActivityAt: 1 },
    ]);
    render(<ClaudeCodeCard active />);
    const picker = await screen.findByLabelText("Desktop 最近的 Code 專案");
    await waitFor(() => expect((picker as HTMLSelectElement).value).toBe("E:\\alpha"));
    expect(screen.queryByLabelText("專案資料夾")).toBeNull();
    fireEvent.change(picker, { target: { value: "E:\\beta" } });
    fireEvent.click(screen.getByText("接入此專案"));
    await waitFor(() => expect(claudeCodeApi.start).toHaveBeenCalledWith("E:\\beta"));
    fireEvent.change(picker, { target: { value: "<other>" } });
    fireEvent.change(screen.getByLabelText("專案資料夾"), { target: { value: "D:\\other" } });
    fireEvent.click(screen.getByText("接入此專案"));
    await waitFor(() => expect(claudeCodeApi.start).toHaveBeenLastCalledWith("D:\\other"));
  });
});
