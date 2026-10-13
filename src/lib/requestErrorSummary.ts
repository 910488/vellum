import type { RequestLogEntry } from "@/types";
import { humanDuration } from "./humanDuration";

type Translate = (key: string, options?: Record<string, unknown>) => string;

/** Only allow-listed reasons reach the row. Arbitrary upstream text, ids,
 * URLs and provider names remain in the closed technical disclosure. */
function rejectionReason(raw: string): string | null {
  if (/unsupported parameter/i.test(raw)) return "unsupported";
  if (/reserved.*schema|must match.*schema|invalid.*tools|tool.*schema/i.test(raw)) return "tools";
  if (/previous_response_id|previous response|response.*not found/i.test(raw)) return "continuation";
  if (/context.*(?:length|window)|too many tokens|maximum.*tokens/i.test(raw)) return "context";
  if (/content.*(?:policy|flagged)|cybersecurity risk/i.test(raw)) return "policy";
  return null;
}

export function requestErrorSummary(entry: Pick<RequestLogEntry, "status" | "error" | "errorCategory" | "retryAfter">, t: Translate): string {
  const category = entry.errorCategory;
  const raw = entry.error ?? "";
  let key: string;
  if (category === "provider_quota" || entry.status === 429) key = "quota";
  else if (category === "provider_unauthorized" || category === "authentication_failed" || [401, 403].includes(entry.status)) key = "auth";
  else if (["client_cancel", "client_disconnect"].includes(category ?? "") || entry.status === 499) key = "cancelled";
  else if (category === "provider_protocol") key = entry.status >= 500 ? "protocol" : "rejected";
  else if (["invalid_request", "provider_request_rejected", "unsupported_mode"].includes(category ?? "") || entry.status === 400 || entry.status === 422) key = "rejected";
  else if (category === "provider_unavailable" || entry.status >= 500) key = "unavailable";
  else key = "other";
  let summary = t(`requestErrors.${key}`);
  if (key === "rejected") {
    const reason = rejectionReason(raw);
    if (reason) summary += ` ${t(`requestErrors.reasons.${reason}`)}`;
  }
  // Retry-After is an upstream minimum, not a scheduled Vellum retry.
  const retryAfter = entry.retryAfter ?? Number(raw.match(/retry[_ -]after\s*[:=]\s*(\d+)/i)?.[1]);
  if (Number.isFinite(retryAfter) && retryAfter >= 0 && ["quota", "unavailable"].includes(key)) {
    summary += ` ${t("requestErrors.retryAfter", { time: humanDuration(retryAfter * 1000, t) })}`;
  }
  const retries = raw.match(/(?:after\s+)?(\d+)\s+retr(?:y|ies)\b/i)?.[1];
  if (retries) summary += ` ${t("requestErrors.retries", { count: Number(retries) })}`;
  return summary;
}
