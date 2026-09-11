// Provider display-name derivation for the Connect-a-Provider flow.
//
// The guessed name is only a prefill: the user may still edit it before the
// route is created. The route id is derived from the final name, so a better
// guess here avoids surprising ids derived from a top-level domain.

const SERVICE_PREFIXES = new Set(["api", "www", "gateway"]);

/**
 * Derive a short provider display name from an endpoint URL.
 *
 * - `https://api.provider.example/v1` → `provider`
 * - `https://api.openrouter.ai/v1` → `openrouter`
 * - `https://openrouter.ai/v1` → `openrouter`
 * - `localhost`, IP literals and invalid URLs fall back to `fallback`.
 *
 * Common service prefixes (`api`, `www`, `gateway`) are skipped before the
 * first meaningful label is taken.
 */
export function providerNameFromEndpoint(endpoint: string, fallback: string): string {
  let hostname: string;
  try {
    hostname = new URL(endpoint).hostname;
  } catch {
    return fallback;
  }
  if (!hostname || hostname === "localhost") return fallback;
  // IPv6 literals keep brackets in hostname ("[::1]").
  if (hostname.includes(":")) return fallback;
  const labels = hostname.split(".").filter(Boolean);
  if (!labels.length) return fallback;
  // IPv4 literals ("127.0.0.1") carry no provider identity.
  if (labels.every((label) => /^\d+$/.test(label))) return fallback;
  const meaningful = labels.filter((label) => !SERVICE_PREFIXES.has(label));
  return meaningful[0] ?? fallback;
}
