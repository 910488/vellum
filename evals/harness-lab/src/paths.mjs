import { fileURLToPath } from 'node:url';
import path from 'node:path';

export const labRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
export const repoRoot = path.resolve(labRoot, '..', '..');
export const outputRoot = path.join(repoRoot, 'target', 'harness-lab');
export const upstreamRoot = path.join(outputRoot, 'upstream');
export const qwenRoot = path.join(upstreamRoot, 'qwen-code');
export const lockPath = path.join(labRoot, 'upstream-lock.json');
