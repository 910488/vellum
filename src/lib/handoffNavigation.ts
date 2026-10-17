let target: { host: string; id: string } | null = null;
export const handoffReviewTarget = () => target;
export function requestHandoffReview(host: string, id: string) {
  target = { host, id };
  window.dispatchEvent(new CustomEvent("vellum:handoff-review", { detail: target }));
}
