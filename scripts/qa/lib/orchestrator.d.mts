export function remoteProofPassed(fullLog: string, testName: string): boolean;
export function desktopDrivenOutcome(control: string): {
  verdict: string;
  reason: string;
  detail: string;
};
export function candidateLiveOutcome(caseId: string, route: string): {
  verdict: string;
  reason: string;
  detail: string;
};
