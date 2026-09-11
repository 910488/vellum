import { createHash } from 'node:crypto';
import { mkdir, readFile, readdir, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { createQwenDetector } from './qwen-adapter.mjs';
import { readRollout } from './rollout.mjs';
import { labRoot, outputRoot, repoRoot } from './paths.mjs';

const specPath = path.join(labRoot, 'corpus.json');
const spec = JSON.parse(await readFile(specPath, 'utf8'));
const cases = [];

async function walk(root) {
  const found = [];
  for (const entry of await readdir(root, { withFileTypes: true })) {
    const full = path.join(root, entry.name);
    if (entry.isDirectory()) found.push(...await walk(full));
    else if (entry.name.endsWith('.jsonl')) found.push(full);
  }
  return found;
}

for (const run of spec.runs) {
  const runRoot = path.join(repoRoot, 'target', 'vellum-evals', run.id);
  const traces = (await readdir(path.join(runRoot, 'traces')))
    .filter(name => name.endsWith('.phase-0.jsonl'));
  const rollouts = (await walk(path.join(runRoot, 'cases')))
    .filter(name => name.includes(`${path.sep}sessions${path.sep}`));
  const rolloutByThread = new Map();
  for (const file of rollouts) {
    const first = JSON.parse((await readFile(file, 'utf8')).split(/\r?\n/, 1)[0]);
    rolloutByThread.set(first.payload?.id, file);
  }
  for (const traceName of traces) {
    const tracePath = path.join(runRoot, 'traces', traceName);
    const first = JSON.parse((await readFile(tracePath, 'utf8')).split(/\r?\n/, 1)[0]);
    const rollout = rolloutByThread.get(first.thread_id);
    if (!rollout) throw new Error(`rollout missing for ${run.id}/${traceName}`);
    const calls = await readRollout(rollout);
    const detector = await createQwenDetector();
    let trigger = null;
    for (const [index, call] of calls.entries()) {
      if (!trigger && detector.addToolCall(call)) trigger = { index: index + 1, tool: call.name, loopType: detector.lastLoopType() };
      detector.addFinished();
    }
    const task = traceName.replace('.phase-0.jsonl', '');
    cases.push({ runId: run.id, arm: run.arm, round: run.round, task, label: spec.labels[`${run.id}/${task}`] ?? 'unreviewed', calls: calls.length, trigger });
  }
}
const reviewed = cases.filter(item => item.label !== 'unreviewed');
const report = {
  schemaVersion: 1,
  adapterVersion: 'qwen-loop-v1',
  corpusSha256: `sha256:${createHash('sha256').update(await readFile(specPath)).digest('hex')}`,
  caseCount: cases.length,
  reviewedCount: reviewed.length,
  reviewedStalledDetected: reviewed.filter(item => item.label === 'stalled' && item.trigger).length,
  reviewedRecoveredFalseStops: reviewed.filter(item => item.label === 'recovered' && item.trigger).length,
  cases,
};
await mkdir(path.join(outputRoot, 'reports'), { recursive: true });
const output = path.join(outputRoot, 'reports', 'e1-corpus-qwen-loop.json');
await writeFile(output, JSON.stringify(report, null, 2) + '\n');
console.log(JSON.stringify({ output, ...report }, null, 2));
