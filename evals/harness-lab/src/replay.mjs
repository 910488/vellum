import { createHash } from 'node:crypto';
import { mkdir, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { createQwenDetector } from './qwen-adapter.mjs';
import { readRollout } from './rollout.mjs';
import { outputRoot } from './paths.mjs';

const args = process.argv.slice(2);
const input = args[args.indexOf('--input') + 1];
if (!input || args.includes('--help')) {
  console.log('Usage: node src/replay.mjs --input <rollout.jsonl> [--upstream-default]');
  process.exit(input ? 0 : 2);
}
const detector = await createQwenDetector({ skipHeuristics: args.includes('--upstream-default') });
const calls = await readRollout(input);
const triggers = [];
for (const [index, call] of calls.entries()) {
  if (detector.addToolCall(call) && triggers.length === 0) {
    triggers.push({ index: index + 1, callId: call.callId, tool: call.name, loopType: detector.lastLoopType() });
  }
  detector.addFinished();
}
const report = {
  schemaVersion: 1,
  adapterVersion: 'qwen-loop-v1',
  input: path.resolve(input),
  inputSha256: `sha256:${createHash('sha256').update(await import('node:fs/promises').then(m => m.readFile(input))).digest('hex')}`,
  calls: calls.length,
  triggers,
};
await mkdir(path.join(outputRoot, 'reports'), { recursive: true });
const output = path.join(outputRoot, 'reports', `${path.basename(input, '.jsonl')}-qwen-loop.json`);
await writeFile(output, JSON.stringify(report, null, 2) + '\n');
console.log(JSON.stringify({ ...report, output }, null, 2));
