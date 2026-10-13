export function operationalErrorKey(raw: string): string {
  if (/refresh[_ ]token.*(?:reject|reus|expir)|refresh_token_reused|login.*expir|sign.?in.*expir/i.test(raw)) return "tabPolish.loginExpired";
  if (/grok.*(?:429|rate.?limit)|(?:429|rate.?limit).*grok/i.test(raw)) return "tabPolish.rateLimited";
  return "tabPolish.genericError";
}
