import { describe, expect, it } from "vitest";
import i18n from "i18next";
import "@/i18n";
import { requestErrorSummary } from "@/lib/requestErrorSummary";

describe("request error summaries", () => {
  for (const locale of ["zh-TW", "zh-CN", "en", "ja"]) {
    it(`explains categories and real retry evidence in ${locale}`, () => {
      const t = i18n.getFixedT(locale);
      expect(requestErrorSummary({ status: 429, error: null, retryAfter: 60 }, t))
        .toBe(`${t("requestErrors.quota")} ${t("requestErrors.retryAfter", { time: t("tabPolish.time.minutesSeconds", {minutes:1,seconds:0}) })}`);
      expect(requestErrorSummary({status:502, errorCategory:"provider_unavailable",error:"upstream failed after 2 retries"},t))
        .toBe(`${t("requestErrors.unavailable")} ${t("requestErrors.retries",{count:2})}`);
      expect(requestErrorSummary({status:400,errorCategory:"provider_protocol",error:"Invalid tools: reserved function must match configured schema"},t))
        .toBe(`${t("requestErrors.rejected")} ${t("requestErrors.reasons.tools")}`);
      expect(requestErrorSummary({status:502,errorCategory:"provider_protocol",error:"invalid frame"},t)).toBe(t("requestErrors.protocol"));
      expect(requestErrorSummary({status:401,error:null},t)).toBe(t("requestErrors.auth"));
      expect(requestErrorSummary({status:499,error:null},t)).toBe(t("requestErrors.cancelled"));
      expect(requestErrorSummary({status:400,error:"unsupported parameter: private_secret_id"},t)).not.toContain("private_secret_id");
      expect(requestErrorSummary({status:400,error:"opaque-id @email https://private.example"},t)).toBe(t("requestErrors.rejected"));
      expect(requestErrorSummary({status:429,error:null},t)).toBe(t("requestErrors.quota"));
      expect(t("quotaActions.resetCount",{count:2})).not.toBe("quotaActions.resetCount");
      expect(t("tabPolish.core.missing")).not.toContain("Enhanced");
    });
  }
});
