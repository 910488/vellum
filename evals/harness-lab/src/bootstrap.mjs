import { mkdir, readFile, rm } from 'node:fs/promises';
import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import path from 'node:path';
import { lockPath, qwenRoot, upstreamRoot } from './paths.mjs';

const lock = JSON.parse(await readFile(lockPath, 'utf8'));
const expected = lock.qwenCode.commit;

function run(command, args, cwd) {
  const result = spawnSync(command, args, {
    cwd,
    stdio: 'inherit',
    shell: process.platform === 'win32' && command === 'pnpm',
  });
  if (result.status !== 0) throw new Error(`${command} failed with ${result.status}`);
}

await mkdir(upstreamRoot, { recursive: true });
let actual = '';
try {
  actual = spawnSync('git', ['rev-parse', 'HEAD'], { cwd: qwenRoot, encoding: 'utf8' }).stdout.trim();
} catch {}
if (actual && actual !== expected) {
  throw new Error(`refusing mutable upstream cache: expected ${expected}, found ${actual}`);
}
if (!actual) {
  await rm(qwenRoot, { recursive: true, force: true });
  run('git', ['clone', '--filter=blob:none', '--no-checkout', lock.qwenCode.repository, qwenRoot], upstreamRoot);
  run('git', ['config', 'core.longpaths', 'true'], qwenRoot);
  run('git', ['fetch', '--depth=1', 'origin', expected], qwenRoot);
  run('git', ['checkout', '--detach', expected], qwenRoot);
} else {
  run('git', ['config', 'core.longpaths', 'true'], qwenRoot);
  run('git', ['checkout', '--force', '--detach', expected], qwenRoot);
}
actual = spawnSync('git', ['rev-parse', 'HEAD'], { cwd: qwenRoot, encoding: 'utf8' }).stdout.trim();
if (actual !== expected) throw new Error(`Qwen source identity mismatch: ${actual}`);
const source = await readFile(path.join(qwenRoot, 'packages', 'core', 'src', 'services', 'loopDetectionService.ts'));
const sourceSha256 = `sha256:${createHash('sha256').update(source).digest('hex')}`;
if (sourceSha256 !== lock.qwenCode.sourceSha256) {
  throw new Error(`Qwen detector source mismatch: ${sourceSha256}`);
}

const modulePath = path.join(qwenRoot, ...lock.qwenCode.module.split('/'));
try {
  await readFile(modulePath);
} catch {
  // The monorepo prepare script builds every UI/channel package. The lab only
  // needs core, so keep the one-time setup bounded to its locked dependencies.
  run('pnpm', ['install', '--frozen-lockfile', '--ignore-scripts'], qwenRoot);
  run('pnpm', ['--filter', '@qwen-code/qwen-code-core', 'build'], qwenRoot);
}
console.log(JSON.stringify({ status: 'ready', commit: actual, sourceSha256, modulePath }));
