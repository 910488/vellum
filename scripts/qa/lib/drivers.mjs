import { TAB_CONTROL } from "../../../qa/matrix/v1.mjs";
import { assertionAfterAction } from "./assert-after-action.mjs";

export { assertionAfterAction };

/**
 * A desktop/live/remote case may PASS only after its named control is driven
 * AND a case-specific assertion succeeds. Clicking the tab because `control`
 * is missing is forbidden.
 */
export function resolveActionControl(matrixCase) {
  const fromDriver = matrixCase.driver?.action?.control;
  if (fromDriver) return fromDriver;
  const surfaceTab = TAB_CONTROL[matrixCase.surface];
  const isNavigate = /\.navigate$/.test(matrixCase.id) || matrixCase.driver?.action?.kind === "navigate";
  if (isNavigate) return matrixCase.control || surfaceTab;
  if (matrixCase.control && matrixCase.control !== surfaceTab) return matrixCase.control;
  return null;
}

export function controlFallbackRejected(matrixCase) {
  const surfaceTab = TAB_CONTROL[matrixCase.surface];
  if (!matrixCase.control) return true;
  if (/\.navigate$/.test(matrixCase.id)) return false;
  return matrixCase.control === surfaceTab && matrixCase.driver?.action?.kind !== "navigate";
}

export function candidateGateIsNotLiveTask(source) {
  return source === "enhanced-integration-gate" || source === "candidate-provider-gate";
}
