import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import i18n from "i18next";
import "@/i18n";
import { RemoteModels } from "@/components/RemoteModels";
const api = vi.hoisted(() => ({ listRemoteHostModels: vi.fn(), updateRemoteHostModels: vi.fn() }));
vi.mock("@/lib/api", () => ({ api }));
const models = [
  { catalogId: "official", displayName: "Official", upstreamModel: "official", routeId: "official", selected: true, mandatory: true },
  { catalogId: "other", displayName: "Other", upstreamModel: "other", routeId: "other", selected: true, mandatory: false },
];
const operation = { operationId: "models", hostId: "host", kind: "oneClickBootstrap", phase: "waitingForIdle", percent: 15, state: "running" as const, message: null, result: null, updatedAt: "2026-10-16T00:00:00Z" };
beforeEach(async () => { vi.resetAllMocks(); await i18n.changeLanguage("en"); api.listRemoteHostModels.mockResolvedValue(models); api.updateRemoteHostModels.mockResolvedValue(operation); });
afterEach(cleanup);
describe("host model catalogue", () => {
  it("only reads on mount, enforces mandatory controls and submits one controlled converge", async () => {
    const onOperation = vi.fn(); const onCatalog = vi.fn();
    render(<RemoteModels hostId="host" disabled={false} revision={1} onOperation={onOperation} onCatalog={onCatalog} />);
    const official = await screen.findByRole<HTMLInputElement>("checkbox", { name: /Official/ });
    expect(official.disabled).toBe(true); expect(official.checked).toBe(true);
    expect(api.updateRemoteHostModels).not.toHaveBeenCalled();
    expect(onCatalog).toHaveBeenCalledWith({ hostId: "host", models });
    fireEvent.click(screen.getByRole("checkbox", { name: /Other/ }));
    // Ticking edits a draft; nothing deploys until Apply.
    expect(api.updateRemoteHostModels).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Apply" }));
    await waitFor(() => expect(api.updateRemoteHostModels).toHaveBeenCalledWith("host", ["official"]));
    expect(api.updateRemoteHostModels).toHaveBeenCalledOnce();
    expect(onOperation).toHaveBeenCalledWith(operation);
  });
  it("retains confirmed checkboxes and limits a busy deployment error to this section", async () => {
    api.updateRemoteHostModels.mockRejectedValue(new Error("DeploymentAlreadyRunning"));
    const onOperation = vi.fn();
    render(<RemoteModels hostId="host" disabled={false} revision={1} onOperation={onOperation} onCatalog={() => {}} />);
    const other = await screen.findByRole<HTMLInputElement>("checkbox", { name: /Other/ });
    fireEvent.click(other);
    fireEvent.click(screen.getByRole("button", { name: "Apply" }));
    expect(await screen.findByText(i18n.t("remoteModels.failed"))).toBeTruthy();
    expect(other.checked).toBe(true);
    expect(onOperation).not.toHaveBeenCalled();
    expect(screen.getByText(/DeploymentAlreadyRunning/)).toBeTruthy();
  });
  it("batches several ticks into one converge and can revert the draft", async () => {
    const many = [...models, { catalogId: "third", displayName: "Third", upstreamModel: "third", routeId: "third", selected: false, mandatory: false }];
    api.listRemoteHostModels.mockResolvedValue(many);
    render(<RemoteModels hostId="host" disabled={false} revision={1} onOperation={() => {}} onCatalog={() => {}} />);
    fireEvent.click(await screen.findByRole("checkbox", { name: /Third/ }));
    fireEvent.click(screen.getByRole("checkbox", { name: /Third/ }));
    // Back to the confirmed selection: nothing to apply.
    expect(screen.queryByRole("button", { name: "Apply" })).toBeNull();
    fireEvent.click(screen.getByRole("checkbox", { name: /Other/ }));
    fireEvent.click(screen.getByRole("button", { name: "Revert" }));
    expect(screen.getByRole<HTMLInputElement>("checkbox", { name: /Other/ }).checked).toBe(true);
    fireEvent.click(screen.getByRole("checkbox", { name: /Other/ }));
    fireEvent.click(screen.getByRole("checkbox", { name: /Third/ }));
    fireEvent.click(screen.getByRole("button", { name: "Apply" }));
    await waitFor(() => expect(api.updateRemoteHostModels).toHaveBeenCalledWith("host", ["official", "third"]));
    expect(api.updateRemoteHostModels).toHaveBeenCalledOnce();
  });
  it("disables changes during an existing operation and refetches read-only on refresh", async () => {
    const props = { hostId: "host", disabled: true, revision: 1, onOperation: vi.fn(), onCatalog: vi.fn() };
    const view = render(<RemoteModels {...props} />);
    const other = await screen.findByRole<HTMLInputElement>("checkbox", { name: /Other/ });
    expect(other.disabled).toBe(true); fireEvent.click(other);
    expect(api.updateRemoteHostModels).not.toHaveBeenCalled();
    view.rerender(<RemoteModels {...props} revision={2} />);
    await waitFor(() => expect(api.listRemoteHostModels).toHaveBeenCalledTimes(2));
    expect(api.updateRemoteHostModels).not.toHaveBeenCalled();
  });
});
