export function buildReport(input: Record<string, unknown>): Record<string, unknown>;
export function renderHtml(report: Record<string, unknown>): string;
export function writeReports(
  outDir: string,
  report: Record<string, unknown>,
): { jsonPath: string; htmlPath: string };
