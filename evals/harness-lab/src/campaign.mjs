import { createHash, randomUUID } from 'node:crypto';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { createWriteStream } from 'node:fs';
import { spawn, spawnSync } from 'node:child_process';
import path from 'node:path';
import { outputRoot, repoRoot } from './paths.mjs';

const args = process.argv.slice(2);
const manifest = args[args.indexOf('--manifest') + 1];
if (!manifest || args.includes('--help')) {
  console.log('Usage: node src/campaign.mjs --manifest <campaign.json>');
  process.exit(manifest ? 0 : 2);
}
const spec = JSON.parse(await readFile(manifest, 'utf8'));
const runId = new Date().toISOString().replace(/\D/g, '').slice(0, 14) + '-' + randomUUID().slice(0, 8);
const runRoot = path.join(outputRoot, 'campaigns', runId);
await mkdir(runRoot, { recursive: true });
const statePath = path.join(runRoot, 'state.json');
const state = {
  schemaVersion: 1, runId, status: 'running', pid: process.pid,
  manifest: path.resolve(manifest),
  manifestSha256: `sha256:${createHash('sha256').update(await readFile(manifest)).digest('hex')}`,
  startedAt: new Date().toISOString(), cases: [], artifacts: {},
  gitCommit: spawnSync('git', ['rev-parse', 'HEAD'], { cwd: repoRoot, encoding: 'utf8' }).stdout.trim(),
};
for (const [name, file] of Object.entries(spec.artifacts ?? {})) {
  const resolved = String(file).replaceAll('${REPO_ROOT}', repoRoot);
  state.artifacts[name] = {
    path: resolved,
    sha256: `sha256:${createHash('sha256').update(await readFile(resolved)).digest('hex')}`,
  };
}
await writeFile(statePath, JSON.stringify(state, null, 2) + '\n');
const expand = (value, variables) => String(value)
  .replaceAll('${REPO_ROOT}', repoRoot)
  .replaceAll('${VELLUM_LAB_BASE_URL}', variables.baseUrl ?? '')
  .replaceAll('${VELLUM_LAB_SESSION}', variables.sessionId);

async function startBridge(item, sessionId) {
  if (!spec.bridge) return null;
  const eventLog = path.join(runRoot, `${item.id}-bridge-events.jsonl`);
  const bridge = spawn(process.execPath, ['src/bridge.mjs'], {
    cwd: path.join(repoRoot, 'evals', 'harness-lab'),
    env: {
      ...process.env,
      VELLUM_LAB_UPSTREAM: spec.bridge.upstream,
      VELLUM_LAB_PORT: String(spec.bridge.port),
      VELLUM_LAB_MODE: item.mode ?? spec.bridge.mode,
      VELLUM_LAB_SESSION: sessionId,
      VELLUM_LAB_EVENT_LOG: eventLog,
    },
    stdio: ['ignore', 'pipe', 'pipe'], shell: false,
  });
  const deadline = Date.now() + 15000;
  while (Date.now() < deadline) {
    try {
      const response = await fetch(`http://127.0.0.1:${spec.bridge.port}/healthz`);
      if (response.ok) return { process: bridge, eventLog };
    } catch {}
    await new Promise(resolve => setTimeout(resolve, 100));
  }
  bridge.kill();
  throw new Error('bridge did not become ready');
}

try {
  for (const item of spec.commands ?? []) {
    const sessionId = randomUUID();
    const activeBridge = await startBridge(item, sessionId);
    const baseUrl = spec.bridge ? `http://127.0.0.1:${spec.bridge.port}` : '';
    const variables = { baseUrl, sessionId };
    const env = {
      ...process.env,
      ...Object.fromEntries(Object.entries(item.env ?? {}).map(([key, value]) => [key, expand(value, variables)])),
      VELLUM_LAB_SESSION: sessionId,
      ...(spec.bridge ? { VELLUM_LAB_BASE_URL: baseUrl } : {}),
    };
    const command = item.command.map(value => expand(value, variables));
    const stdoutPath = path.join(runRoot, `${item.id}.stdout.log`);
    const stderrPath = path.join(runRoot, `${item.id}.stderr.log`);
    const stdout = createWriteStream(stdoutPath, { encoding: 'utf8' });
    const stderr = createWriteStream(stderrPath, { encoding: 'utf8' });
    const code = await new Promise(resolve => {
      const child = spawn(command[0], command.slice(1), { cwd: expand(item.cwd ?? '${REPO_ROOT}', variables), env, stdio: ['ignore', 'pipe', 'pipe'], shell: false });
      child.stdout.pipe(stdout); child.stdout.pipe(process.stdout);
      child.stderr.pipe(stderr); child.stderr.pipe(process.stderr);
      child.on('exit', value => resolve(value ?? 1));
    });
    stdout.end(); stderr.end();
    activeBridge?.process.kill();
    if (activeBridge) await new Promise(resolve => activeBridge.process.once('exit', resolve));
    state.cases.push({ id: item.id, mode: item.mode ?? spec.bridge?.mode, sessionId, exitCode: code, eventLog: activeBridge?.eventLog, stdoutPath, stderrPath });
    await writeFile(statePath, JSON.stringify(state, null, 2) + '\n');
    if (code !== 0 && !item.allowFailure) throw new Error(`${item.id} failed with ${code}`);
  }
  state.status = 'complete'; state.exitCode = 0;
} catch (error) {
  state.status = 'failed'; state.exitCode = 1; state.error = String(error);
} finally {
  state.finishedAt = new Date().toISOString();
  await writeFile(statePath, JSON.stringify(state, null, 2) + '\n');
}
console.log(statePath);
process.exitCode = state.exitCode;
