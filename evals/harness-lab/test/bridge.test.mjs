import test from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';
import { spawn } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const labRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

async function freePort() {
  const server = http.createServer();
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const port = server.address().port;
  await new Promise(resolve => server.close(resolve));
  return port;
}

test('S2 injects once after upstream Qwen detector fires', async t => {
  const bodies = [];
  let requestCount = 0;
  const upstream = http.createServer(async (request, response) => {
    const chunks = [];
    for await (const chunk of request) chunks.push(chunk);
    bodies.push(JSON.parse(Buffer.concat(chunks).toString('utf8')));
    requestCount++;
    response.writeHead(200, { 'content-type': 'text/event-stream' });
    if (requestCount === 1) {
      for (let index = 0; index < 8; index++) {
        const event = JSON.stringify({
          type: 'response.output_item.done',
          item: { type: 'function_call', call_id: `c${index}`, name: 'exec_command', arguments: JSON.stringify({ cmd: `python -c ${index}` }) },
        });
        response.write(`data: ${event}\n\n`.slice(0, 31));
        response.write(`data: ${event}\n\n`.slice(31));
      }
    }
    response.end('data: [DONE]\n\n');
  });
  await new Promise(resolve => upstream.listen(0, '127.0.0.1', resolve));
  t.after(() => upstream.close());
  const bridgePort = await freePort();
  const child = spawn(process.execPath, ['src/bridge.mjs'], {
    cwd: labRoot,
    env: {
      ...process.env,
      VELLUM_LAB_UPSTREAM: `http://127.0.0.1:${upstream.address().port}`,
      VELLUM_LAB_PORT: String(bridgePort),
      VELLUM_LAB_MODE: 'S2',
    },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  t.after(() => child.kill());
  await new Promise((resolve, reject) => {
    child.stdout.on('data', chunk => { if (chunk.toString().includes('listening')) resolve(); });
    child.once('exit', code => reject(new Error(`bridge exited ${code}`)));
  });
  const send = input => fetch(`http://127.0.0.1:${bridgePort}/v1/responses`, {
    method: 'POST', headers: { 'content-type': 'application/json', 'x-vellum-lab-session': 'test' },
    body: JSON.stringify({ model: 'test', input }),
  }).then(response => response.text());
  await send([{ role: 'user', content: [{ type: 'input_text', text: 'task' }] }]);
  await send([{ role: 'user', content: [{ type: 'input_text', text: 'next' }] }]);
  await send([{ role: 'user', content: [{ type: 'input_text', text: 'again' }] }]);
  assert.equal(bodies[1].input.length, 2);
  assert.match(bodies[1].input[1].content[0].text, /重新核對/);
  assert.equal(bodies[2].input.length, 1);
});
