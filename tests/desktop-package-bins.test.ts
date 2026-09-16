import { describe, expect, it } from "vitest";
import desktopToml from "../src-tauri/Cargo.toml?raw";
import buildSidecar from "../scripts/build-sidecar.mjs?raw";
import buildLocalRelease from "../scripts/build-local-release.sh?raw";
import desktopWorkflow from "../.github/workflows/desktop-build.yml?raw";
import hotUpdateConfig from "../src-tauri/tauri.hot-update.conf.json";
import releaseWorkflow from "../.github/workflows/release.yml?raw";
import finalizeDarwinManifest from "../scripts/finalize-remote-darwin-manifest.mjs?raw";
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

  it("keeps full installers bundled and hot updates component-only", () => {
    expect(tauriConfig.bundle.resources).toEqual([
      "resources/remote/**/*",
      "binaries/*",
    ]);
    expect(desktopWorkflow).toMatch(/Embedded Remote Manager payload/);
    expect(desktopWorkflow).toMatch(/build-local-release\.sh stage-only/);

    expect(hotUpdateConfig.bundle.resources).toEqual([
      "binaries/vellum-codex-app-server*",
      "binaries/vellum-codex-relay*",
    ]);
    expect(buildSidecar).toMatch(/VELLUM_DESKTOP_HOT_UPDATE/);
    expect(buildSidecar).toMatch(/delete cargoEnvironment\.TAURI_CONFIG/);

    const windowsHotUpdate = releaseWorkflow.slice(
      releaseWorkflow.indexOf("  windows-updater:"),
      releaseWorkflow.indexOf("  macos-updater:"),
    );
    const macHotUpdate = releaseWorkflow.slice(
      releaseWorkflow.indexOf("  macos-updater:"),
      releaseWorkflow.indexOf("  remote-linux:"),
    );
    for (const job of [windowsHotUpdate, macHotUpdate]) {
      expect(job).toMatch(/VELLUM_DESKTOP_HOT_UPDATE/);
      expect(job).toMatch(/tauri\.hot-update\.conf\.json/);
      expect(job).not.toMatch(/build-only-remote-payload|build-local-release/);
    }
  });

  it("finalizes one complete Darwin-aware manifest before building either installer", () => {
    const darwinJob = desktopWorkflow.slice(
      desktopWorkflow.indexOf("  remote-payload-darwin:"),
      desktopWorkflow.indexOf("  macos-dmg:"),
    );
    expect(darwinJob).toMatch(/needs: remote-payload/);
    expect(darwinJob).toMatch(/codex-aarch64-apple-darwin\.tar\.gz/);
    expect(darwinJob).toMatch(/finalize-remote-darwin-manifest\.mjs/);
    expect(buildLocalRelease).toMatch(/aarch64-apple-darwin/);
    expect(buildLocalRelease).toMatch(/finalize-remote-darwin-manifest\.mjs/);
    expect(finalizeDarwinManifest).toMatch(/schemaVersion = 4/);
    expect(finalizeDarwinManifest).toMatch(/darwin-arm64\/codex/);
    expect(finalizeDarwinManifest).toMatch(/darwin-arm64\/vellum-remote-agent/);
    expect(finalizeDarwinManifest).toMatch(/darwin-arm64\/vellum-proxy-daemon/);

    for (const jobName of ["  macos-dmg:", "  windows-installer:"]) {
      const start = desktopWorkflow.indexOf(jobName);
      const rest = desktopWorkflow.slice(start + jobName.length);
      const nextJob = rest.search(/\n  [a-z][a-z-]+:/);
      const job =
        nextJob < 0
          ? desktopWorkflow.slice(start)
          : desktopWorkflow.slice(start, start + jobName.length + nextJob);
      expect(job).toMatch(/needs: remote-payload-darwin/);
      expect(job).toMatch(/Download complete remote payload/);
    }

    const signJob = releaseWorkflow.slice(releaseWorkflow.indexOf("  sign-and-release:"));
    expect(signJob).toMatch(/- remote-darwin/);
  });
});
