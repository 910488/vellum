import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import i18n from "i18next";
import "@/i18n";
import { api } from "@/lib/api";
import { RemoteHostAdd, RemoteHostLoginGuide } from "@/components/RemoteHostAdd";
import type { AddedRemoteHost } from "@/types";
import { assertResourceParity } from "@/i18n/resources";

const added: AddedRemoteHost = {
  candidate: { codexHostId: null, vellumHostId: "added-123", displayName: "dev@host.test:2222", sshAlias: "vellum-added-123", hostname: "host.test", user: "dev", port: 2222, source: "vellumAdded", validated: true, validationError: null },
  loginCommand: "ssh -F '/data/remote-ssh/config' -- vellum-added-123",
  keyInstallCommand: "ssh-copy-id -F '/data/remote-ssh/config' vellum-added-123",
};
beforeEach(async () => { await i18n.changeLanguage("en"); });
afterEach(() => { cleanup(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });

describe("private SSH host registration", () => {
  it("keeps the pending operation single and never asks for a password", async () => {
    let finish!: (host: AddedRemoteHost) => void;
    const add = vi.spyOn(api, "addRemoteSshHost").mockReturnValue(new Promise((resolve) => { finish = resolve; }));
    const onAdded = vi.fn();
    const { container } = render(<RemoteHostAdd onAdded={onAdded} />);
    fireEvent.change(screen.getByLabelText("SSH destination"), { target: { value: "  dev@host.test:2222  " } });
    fireEvent.submit(container.querySelector("form")!);
    fireEvent.submit(container.querySelector("form")!);
    expect(screen.getByRole("status")).toBeTruthy();
    expect(add).toHaveBeenCalledTimes(1);
    expect(add).toHaveBeenCalledWith("dev@host.test:2222");
    expect(container.querySelector('input[type="password"]')).toBeNull();
    expect(onAdded).not.toHaveBeenCalled();
    finish(added);
    await waitFor(() => expect(onAdded).toHaveBeenCalledWith(added));
  });
  it("contains registration errors in the form and permits retry", async () => {
    const add = vi.spyOn(api, "addRemoteSshHost").mockRejectedValueOnce(new Error("SshDestinationInvalid")).mockResolvedValueOnce(added);
    const onAdded = vi.fn();
    render(<RemoteHostAdd onAdded={onAdded} />);
    fireEvent.change(screen.getByLabelText("SSH destination"), { target: { value: "dev@host.test:2222" } });
    fireEvent.click(screen.getByRole("button", { name: "Add host" }));
    expect(await screen.findByText(i18n.t("remoteAdd.failed"))).toBeTruthy();
    expect(onAdded).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Add host" }));
    await waitFor(() => expect(add).toHaveBeenCalledTimes(2));
    await waitFor(() => expect(onAdded).toHaveBeenCalledWith(added));
  });
  it("only copies external terminal instructions and includes all four locales", async () => {
    const copy = vi.fn().mockResolvedValue(undefined);
    vi.stubGlobal("navigator", { clipboard: { writeText: copy } });
    render(<RemoteHostLoginGuide host={added} />);
    expect(screen.getByText(added.keyInstallCommand)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Copy key setup command" }));
    await waitFor(() => expect(copy).toHaveBeenCalledWith(added.keyInstallCommand));
    fireEvent.click(screen.getByRole("button", { name: "Copy login command" }));
    await waitFor(() => expect(copy).toHaveBeenCalledWith(added.loginCommand));
    expect(() => assertResourceParity()).not.toThrow();
  });
});
