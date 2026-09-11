import { createCipheriv, createDecipheriv, randomBytes } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import path from "node:path";

const TTL_MS = 30 * 60 * 1000;

function envelopePath(dir) {
  return path.join(dir, "credential-envelope.bin");
}

function metaPath(dir) {
  return path.join(dir, "credential-envelope.meta.json");
}

/**
 * Host-side: encrypt provider secrets for a single run.
 * The envelope lives in a dedicated secrets directory — never the public exchange,
 * never argv, never the HTML/JSON report.
 */
export function sealCredentials(secretsDir, payload, { now = Date.now(), ttlMs = TTL_MS } = {}) {
  mkdirSync(secretsDir, { recursive: true });
  const key = randomBytes(32);
  const iv = randomBytes(12);
  const cipher = createCipheriv("aes-256-gcm", key, iv);
  const plain = Buffer.from(JSON.stringify(payload), "utf8");
  const encrypted = Buffer.concat([cipher.update(plain), cipher.final()]);
  const tag = cipher.getAuthTag();
  const blob = Buffer.concat([iv, tag, encrypted]);
  writeFileSync(envelopePath(secretsDir), blob);
  writeFileSync(
    metaPath(secretsDir),
    JSON.stringify(
      {
        alg: "aes-256-gcm",
        createdAt: new Date(now).toISOString(),
        expiresAt: new Date(now + ttlMs).toISOString(),
        once: true,
        fields: Object.keys(payload ?? {}),
      },
      null,
      2,
    ),
  );
  return { key: key.toString("base64"), expiresAt: now + ttlMs };
}

export function openCredentials(secretsDir, keyB64, { now = Date.now() } = {}) {
  const meta = JSON.parse(readFileSync(metaPath(secretsDir), "utf8"));
  if (new Date(meta.expiresAt).getTime() < now) {
    const error = new Error("credential envelope expired");
    error.code = "CREDENTIAL_EXPIRED";
    throw error;
  }
  const blob = readFileSync(envelopePath(secretsDir));
  const iv = blob.subarray(0, 12);
  const tag = blob.subarray(12, 28);
  const encrypted = blob.subarray(28);
  const decipher = createDecipheriv("aes-256-gcm", Buffer.from(keyB64, "base64"), iv);
  decipher.setAuthTag(tag);
  const plain = Buffer.concat([decipher.update(encrypted), decipher.final()]);
  return JSON.parse(plain.toString("utf8"));
}

export function destroyCredentialChannel(secretsDir) {
  if (!secretsDir || !existsSync(secretsDir)) return { ok: true, skipped: true };
  try {
    rmSync(secretsDir, { recursive: true, force: true, maxRetries: 8, retryDelay: 250 });
    return { ok: true };
  } catch (error) {
    return { ok: false, error: error.message };
  }
}

/** Host resolver: env keys only. Never copies DPAPI files. */
export function resolveHostSecrets(env = process.env) {
  const secrets = {};
  if (env.VELLUM_QA_OPENCODE_KEY || env.OPENCODE_API_KEY) {
    secrets.opencode = env.VELLUM_QA_OPENCODE_KEY || env.OPENCODE_API_KEY;
  }
  if (env.VELLUM_QA_GROK_KEY || env.XAI_API_KEY) {
    secrets.grok = env.VELLUM_QA_GROK_KEY || env.XAI_API_KEY;
  }
  if (env.VELLUM_QA_QWEN_KEY) {
    secrets.qwen = env.VELLUM_QA_QWEN_KEY;
  }
  return secrets;
}

export function credentialFieldsPresent(secrets) {
  return Object.keys(secrets ?? {});
}
