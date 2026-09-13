import { describe, expect, it } from "vitest";
import desktopToml from "../src-tauri/Cargo.toml?raw";
import workspaceToml from "../Cargo.toml?raw";

describe("daily Desktop package", () => {
  it("does not declare eval, app-server, or gate-child bins", () => {
    expect(desktopToml).not.toMatch(/name = "vellum-eval"/);
    expect(desktopToml).not.toMatch(/name = "vellum-codex-app-server"/);
    expect(desktopToml).not.toMatch(/name = "vellum-codex-gate-child"/);
    expect(desktopToml).toMatch(/name = "vellum-proxy-desktop"/);
  });

  it("keeps those CLIs as workspace packages outside default-members", () => {
    const defaults = workspaceToml.slice(
      workspaceToml.indexOf("default-members"),
      workspaceToml.indexOf("\nmembers = ["),
    );
    expect(defaults).not.toMatch("vellum-eval");
    expect(defaults).not.toMatch("vellum-codex-app-server");
    expect(defaults).not.toMatch("vellum-codex-gate-child");
    expect(workspaceToml).toMatch("crates/vellum-eval");
    expect(workspaceToml).toMatch("crates/vellum-codex-app-server");
    expect(workspaceToml).toMatch("crates/vellum-codex-gate-child");
  });
});
