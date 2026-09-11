export function probeLiveCredentials(env?: Record<string, string | undefined>): {
  ok: boolean;
  reason: string;
  detail?: string;
  present: string[];
};
