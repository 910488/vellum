import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const version = process.argv[2];
if (!version || !/^\d+\.\d+\.\d+(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$/.test(version)) {
  throw new Error("usage: set-release-version.mjs <SemVer without build metadata>");
}

const root = process.env.VELLUM_RELEASE_ROOT ?? join(fileURLToPath(new URL("..", import.meta.url)));
for (const relative of ["package.json", "src-tauri/tauri.conf.json"]) {
  const path = join(root, relative);
  const value = JSON.parse(readFileSync(path, "utf8"));
  value.version = version;
  writeFileSync(path, `${JSON.stringify(value, null, 2)}\n`, "utf8");
}

const cargoTomlPath = join(root, "src-tauri", "Cargo.toml");
const cargoToml = readFileSync(cargoTomlPath, "utf8").replace(
  /(\[package\][\s\S]*?\nversion\s*=\s*")[^"]+("\s*\n)/,
  `$1${version}$2`,
);
writeFileSync(cargoTomlPath, cargoToml, "utf8");

const cargoLockPath = join(root, "Cargo.lock");
const cargoLock = readFileSync(cargoLockPath, "utf8").replace(
  /(\[\[package\]\]\r?\nname = "vellum"\r?\nversion = ")[^"]+("\r?\n)/,
  `$1${version}$2`,
);
writeFileSync(cargoLockPath, cargoLock, "utf8");
console.log(`build version set to ${version}`);
