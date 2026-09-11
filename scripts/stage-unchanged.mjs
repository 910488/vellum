import { chmodSync, existsSync, readFileSync, renameSync, statSync, writeFileSync } from "node:fs";

/**
 * Copy `source` to `destination` only when bytes differ. Unchanged files keep their mtime.
 *
 * The permission bits travel with the bytes. What gets staged here are
 * executables the bundler copies as-is into the macOS app; a bare
 * `writeFileSync` made them 0644, so 0.2.7 found its bridge and core and could
 * not run either. A destination an older copy already staged has the right
 * bytes and the wrong mode, so the mode is fixed on the skip path too — chmod
 * does not touch mtime.
 */
export function stageUnchangedSkip(source, destination) {
  const incoming = readFileSync(source);
  const mode = statSync(source).mode & 0o777;
  if (existsSync(destination) && readFileSync(destination).equals(incoming)) {
    syncMode(destination, mode);
    return false;
  }
  const temporary = `${destination}.staging`;
  writeFileSync(temporary, incoming);
  syncMode(temporary, mode);
  renameSync(temporary, destination);
  return true;
}

function syncMode(path, mode) {
  // Windows has no execute bit; its mode only mirrors the read-only flag.
  if (process.platform === "win32") {
    return;
  }
  if ((statSync(path).mode & 0o777) !== mode) {
    chmodSync(path, mode);
  }
}
