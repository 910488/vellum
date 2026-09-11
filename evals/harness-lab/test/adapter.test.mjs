import test from 'node:test';
import assert from 'node:assert/strict';
import { mapToolName } from '../src/qwen-adapter.mjs';
import { readRollout } from '../src/rollout.mjs';
import { mkdtemp, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { ResponsesSseParser } from '../src/sse.mjs';

test('maps Codex tools without collapsing patches into shell', () => {
  assert.equal(mapToolName('exec_command'), 'run_shell_command');
  assert.equal(mapToolName('apply_patch'), 'replace');
  assert.equal(mapToolName('custom'), 'custom');
});

test('SSE parser waits for done and tolerates arbitrary chunks', () => {
  const parser = new ResponsesSseParser();
  const added = 'data: {"type":"response.output_item.added","item":{"type":"function_call","call_id":"a","name":"exec_command","arguments":"{}"}}\n\n';
  const done = 'data: {"type":"response.output_item.done","item":{"type":"function_call","call_id":"a","name":"exec_command","arguments":"{\\"cmd\\":\\"x\\"}"}}\n\n';
  assert.deepEqual(parser.push(added + done.slice(0, 25)), []);
  assert.deepEqual(parser.push(done.slice(25)), [{ callId: 'a', providerCallId: 'a', name: 'exec_command', args: { cmd: 'x' } }]);
});

test('rollout adapter counts complete calls once and preserves ids', async () => {
  const dir = await mkdtemp(path.join(tmpdir(), 'vellum-lab-'));
  const file = path.join(dir, 'rollout.jsonl');
  await writeFile(file, [
    JSON.stringify({ type: 'response_item', payload: { type: 'function_call', call_id: 'a', name: 'exec_command', arguments: '{"cmd":"x"}' } }),
    JSON.stringify({ type: 'response_item', payload: { type: 'function_call_output', call_id: 'a', output: 'ok' } }),
  ].join('\n'));
  assert.deepEqual(await readRollout(file), [{ callId: 'a', providerCallId: 'a', name: 'exec_command', args: { cmd: 'x' } }]);
});
