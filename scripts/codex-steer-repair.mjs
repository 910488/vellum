// Opt-in Windows Desktop workaround. Never modifies the installed MSIX.
import fs from "node:fs";
import path from "node:path";
import crypto from "node:crypto";
import { fileURLToPath } from "node:url";

export const ASSET = "app-primary-84ad97f06929.js";
export const SUPPORTED_HASH = "533eaa44435adf88ae2c7f5e61aac747db1ad1deafd2d02bdaf2606a37e80f7b";
export const REPAIRED_HASH = "a0453190941b2bd418e281fd327b9e0e2146062bb92dfb99785b882b3b026bf4";
export const ORIGINAL_HEADER_HASH = "d7da3129304d1f77bb8e4825709942bc7573786b67a21da1e9ee96f3370777a8";
export const REPAIRED_HEADER_HASH = "1d753bc87addb8a1d8420517393869bbfaafa5faa1f81ddd090bbf56cd4e4468";
export const ORIGINAL_GATE = "fn=q(rP)&&bt===`local`";
// Retain the atom subscription / hook call. Leave banners and usage untouched.
export const REPAIRED_GATE = "fn=(q(rP),!1)".padEnd(ORIGINAL_GATE.length, " ");
export const ORIGINAL_DISABLE = "Gn=je||St||rt&&it||vt||Ot||rn?.isLoading===!0||wt||fn";
// wt is the Luna Reserve hardBlocked atom. Keep its hook, omit this UI gate.
export const REPAIRED_DISABLE = ORIGINAL_DISABLE.replace("||wt", "    ");
export const sha256 = (bytes) => crypto.createHash("sha256").update(bytes).digest("hex");

export function patchExecutableIntegrity(bytes) {
  const original = Buffer.from(ORIGINAL_HEADER_HASH);
  const offset = bytes.indexOf(original);
  if (offset < 0 || bytes.indexOf(original, offset + 1) !== -1) {
    throw new Error("Unsupported executable ASAR integrity metadata.");
  }
  const result = Buffer.from(bytes);
  Buffer.from(REPAIRED_HEADER_HASH).copy(result, offset);
  return result;
}

export function patchRenderer(bytes, expectedHash = SUPPORTED_HASH) {
  if (sha256(bytes) !== expectedHash) throw new Error("Unsupported Desktop renderer; refusing to patch.");
  const source = bytes.toString("utf8");
  if (source.split(ORIGINAL_GATE).length !== 2 || source.split(ORIGINAL_DISABLE).length !== 2) {
    throw new Error("Composer gates must occur exactly once.");
  }
  const result = Buffer.from(source.replace(ORIGINAL_GATE, REPAIRED_GATE)
    .replace(ORIGINAL_DISABLE, REPAIRED_DISABLE), "utf8");
  if (result.length !== bytes.length) throw new Error("Patch must preserve ASAR offsets.");
  return result;
}

export function inspectArchive(archive) {
  const fd = fs.openSync(archive, "r");
  try {
    const pre = Buffer.alloc(16);
    if (fs.readSync(fd, pre, 0, 16, 0) !== 16) throw new Error("Truncated ASAR.");
    const headerSize = pre.readUInt32LE(12);
    if (headerSize > 32 * 1024 * 1024) throw new Error("Invalid ASAR header size.");
    const rawHeader = Buffer.alloc(headerSize);
    fs.readSync(fd, rawHeader, 0, headerSize, 16);
    const header = JSON.parse(rawHeader.toString("utf8"));
    const entry = header.files?.webview?.files?.assets?.files?.[ASSET];
    if (!entry || entry.unpacked || entry.integrity?.algorithm !== "SHA256") {
      throw new Error("Unsupported Desktop ASAR layout.");
    }
    const offset = 8 + pre.readUInt32LE(4) + Number(entry.offset);
    const renderer = Buffer.alloc(entry.size);
    if (fs.readSync(fd, renderer, 0, renderer.length, offset) !== renderer.length) {
      throw new Error("Truncated renderer.");
    }
    return { header, rawHeader, entry, offset, renderer };
  } finally { fs.closeSync(fd); }
}

export function patchArchiveCopy(source, destination) {
  if (path.resolve(source) === path.resolve(destination)) throw new Error("Cannot modify installed archive.");
  if (fs.existsSync(destination)) throw new Error("Destination already exists.");
  const { header, rawHeader, entry, offset, renderer } = inspectArchive(source);
  const patched = patchRenderer(renderer);
  const hash = sha256(patched);
  if (!Number.isSafeInteger(entry.integrity.blockSize) || entry.integrity.blockSize <= 0) {
    throw new Error("Invalid integrity block size.");
  }
  entry.integrity.hash = hash;
  entry.integrity.blocks = [];
  for (let start = 0; start < patched.length; start += entry.integrity.blockSize) {
    entry.integrity.blocks.push(sha256(patched.subarray(start, start + entry.integrity.blockSize)));
  }
  const patchedHeader = Buffer.from(JSON.stringify(header), "utf8");
  if (patchedHeader.length !== rawHeader.length) throw new Error("Header size changed; refusing to repack.");
  fs.copyFileSync(source, destination, fs.constants.COPYFILE_EXCL);
  const fd = fs.openSync(destination, "r+");
  try {
    fs.writeSync(fd, patchedHeader, 0, patchedHeader.length, 16);
    fs.writeSync(fd, patched, 0, patched.length, offset);
  } finally { fs.closeSync(fd); }
  const verified = inspectArchive(destination);
  if (sha256(verified.renderer) !== hash || verified.entry.integrity.hash !== hash) {
    throw new Error("Patched archive verification failed.");
  }
  return hash;
}

export function prepareCopy(sourceApp, destination) {
  const source = path.resolve(sourceApp);
  const target = path.resolve(destination);
  if (target === source || target.startsWith(source + path.sep) || source.startsWith(target + path.sep)) {
    throw new Error("Repair directory must be separate from installed app.");
  }
  if (fs.existsSync(target)) throw new Error("Repair directory already exists; use a fresh directory.");
  const archive = path.join(source, "resources", "app.asar");
  // Validate compatibility before copying hundreds of MB.
  const originalArchive = inspectArchive(archive);
  patchRenderer(originalArchive.renderer);
  if (sha256(originalArchive.rawHeader) !== ORIGINAL_HEADER_HASH) throw new Error("Unsupported archive header.");
  const originalExecutable = fs.readFileSync(path.join(source, "ChatGPT.exe"));
  const patchedExecutable = patchExecutableIntegrity(originalExecutable);
  fs.mkdirSync(target, { recursive: true });
  fs.cpSync(source, target, { recursive: true, filter: (item) => item !== archive });
  const rendererSha256 = patchArchiveCopy(archive, path.join(target, "resources", "app.asar"));
  // Windows native shell embeds the ASAR header hash. Updating it invalidates
  // the copied executable's Authenticode signature, not the installed package.
  fs.writeFileSync(path.join(target, "ChatGPT.exe"), patchedExecutable);
  const manifest = { sourceApp: source, rendererSha256, originalRendererSha256: SUPPORTED_HASH,
    executableSha256: sha256(patchedExecutable), originalExecutableSha256: sha256(originalExecutable),
    originalSignaturePreserved: false,
    asset: ASSET, createdAt: new Date().toISOString(), scope: "composer-quota-disable-only" };
  fs.writeFileSync(path.join(target, "vellum-steer-repair.json"), JSON.stringify(manifest, null, 2) + "\n", "utf8");
  return manifest;
}

export function verifyCopy(directory) {
  const { renderer, entry, rawHeader } = inspectArchive(path.join(directory, "resources", "app.asar"));
  if (sha256(renderer) !== REPAIRED_HASH || entry.integrity.hash !== REPAIRED_HASH ||
      sha256(rawHeader) !== REPAIRED_HEADER_HASH) {
    throw new Error("Repair renderer integrity does not match the supported patch.");
  }
  const metadata = JSON.parse(fs.readFileSync(path.join(directory, "vellum-steer-repair.json"), "utf8"));
  const originalExecutable = fs.readFileSync(path.join(metadata.sourceApp, "ChatGPT.exe"));
  const expectedExecutable = patchExecutableIntegrity(originalExecutable);
  if (sha256(fs.readFileSync(path.join(directory, "ChatGPT.exe"))) !== sha256(expectedExecutable)) {
    throw new Error("Repair executable differs from the supported integrity update.");
  }
  return REPAIRED_HASH;
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const [source, destination] = process.argv.slice(2);
  if (source === "--verify" && destination) {
    console.log(verifyCopy(destination));
    process.exit(0);
  }
  if (!source || !destination) throw new Error("Usage: node scripts/codex-steer-repair.mjs <installed-app-directory> <new-copy-directory>");
  console.log(JSON.stringify(prepareCopy(source, destination), null, 2));
}
