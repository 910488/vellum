import { pathToFileURL } from 'node:url';
import path from 'node:path';
import { readFile } from 'node:fs/promises';
import { lockPath, qwenRoot } from './paths.mjs';

const TOOL_MAP = new Map([
  ['exec_command', 'run_shell_command'],
  ['write_stdin', 'write_stdin'],
  ['apply_patch', 'replace'],
  ['view_image', 'read_many_files'],
  ['web_search', 'web_search'],
]);

function configStub(skipHeuristics) {
  return new Proxy({}, {
    get(_target, key) {
      if (key === 'getSkipLoopDetection') return () => skipHeuristics;
      if (key === 'getMaxToolCallsPerTurn') return () => Infinity;
      if (key === 'isMaxToolCallsPerTurnExplicit') return () => false;
      if (key === 'getModel') return () => 'vellum-harness-lab';
      if (key === 'getContentGenerator') return () => undefined;
      if (key === 'getTelemetryLogPromptsEnabled') return () => false;
      if (key === 'getTelemetryEnabled') return () => false;
      return () => undefined;
    },
  });
}

export function mapToolName(name) {
  return TOOL_MAP.get(name) ?? name;
}

export async function createQwenDetector({ skipHeuristics = false } = {}) {
  const lock = JSON.parse(await readFile(lockPath, 'utf8'));
  const modulePath = path.join(qwenRoot, ...lock.qwenCode.module.split('/'));
  const { LoopDetectionService } = await import(pathToFileURL(modulePath));
  const detector = new LoopDetectionService(configStub(skipHeuristics));
  detector.reset('vellum-harness-lab');
  return {
    addToolCall(call) {
      return detector.addAndCheck({
        type: 'tool_call_request',
        value: {
          callId: call.callId,
          providerCallId: call.providerCallId ?? call.callId,
          name: mapToolName(call.name),
          args: call.args ?? {},
          isClientInitiated: false,
          prompt_id: 'vellum-harness-lab',
        },
      });
    },
    addFinished() {
      return detector.addAndCheck({ type: 'finished', value: { reason: 'STOP' } });
    },
    lastLoopType() { return detector.getLastLoopType?.() ?? null; },
    reset(id) { detector.reset(id); },
  };
}
