const SECRET_KEYS = [
  "api_key",
  "apikey",
  "authorization",
  "bearer",
  "token",
  "secret",
  "password",
  "credential",
  "openai",
  "xai-key",
  "brave",
];

const KEY_VALUE = /((?:api[_-]?key|authorization|bearer|token|secret|password)\s*[:=]\s*)([^\s,;]+)/gi;
const BEARER = /Bearer\s+[A-Za-z0-9._\-]+/gi;
const WINDOWS_USER_HOME = /\b[A-Za-z]:[\\/]Users[\\/][^\\/\s"'<>]+/gi;
const POSIX_USER_HOME = /\/(?:Users|home)\/[^/\s"'<>]+/g;

export function redactText(value) {
  if (value == null) return value;
  let text = String(value);
  text = text.replace(KEY_VALUE, "$1[REDACTED]");
  text = text.replace(BEARER, "Bearer [REDACTED]");
  text = text.replace(WINDOWS_USER_HOME, "<USER_HOME>");
  text = text.replace(POSIX_USER_HOME, "<USER_HOME>");
  return text;
}

export function redactDeep(value, key = "") {
  if (typeof value === "string") {
    if (SECRET_KEYS.some((part) => key.toLowerCase().includes(part))) return "[REDACTED]";
    return redactText(value);
  }
  if (Array.isArray(value)) return value.map((item) => redactDeep(item, key));
  if (value && typeof value === "object") {
    const out = {};
    for (const [nextKey, nextValue] of Object.entries(value)) {
      out[nextKey] = redactDeep(nextValue, nextKey);
    }
    return out;
  }
  return value;
}
