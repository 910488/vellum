import { describe, expect, it } from "vitest";
import desktopToml from "../src-tauri/Cargo.toml?raw";
import buildSidecar from "../scripts/build-sidecar.mjs?raw";
import buildScript from "../src-tauri/build.rs?raw";
import desktopWorkflow from "../.github/workflows/desktop-build.yml?raw";
import releaseWorkflow from "../.github/workflows/release.yml?raw";
import tauriConfig from "../src-tauri/tauri.conf.json";
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

  it("bundles only the Desktop bridge resource", () => {
    expect(tauriConfig.bundle.resources).toEqual([
      "binaries/vellum-codex-app-server*",
    ]);
  });

  it("does not stage Remote or Enhanced Core inside Desktop builds", () => {
    expect(buildSidecar).not.toMatch(/stageEnhancedRuntime|downloadPinnedRuntime/);
    expect(buildScript).not.toMatch(/VELLUM_BUNDLED_MANIFEST_SHA256/);
    expect(desktopWorkflow).not.toMatch(/remote-payload|build-local-release/);

    const windowsDesktop = releaseWorkflow.slice(
      releaseWorkflow.indexOf("  windows-updater:"),
      releaseWorkflow.indexOf("  macos-updater:"),
    );
    const macDesktop = releaseWorkflow.slice(
      releaseWorkflow.indexOf("  macos-updater:"),
      releaseWorkflow.indexOf("  remote-linux:"),
    );
    expect(windowsDesktop).not.toMatch(/remote-payload|build-local-release/);
    expect(macDesktop).not.toMatch(/remote-payload|build-local-release/);
  });
});
