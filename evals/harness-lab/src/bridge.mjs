import http from 'node:http';
import { appendFile } from 'node:fs/promises';
import { createQwenDetector } from './qwen-adapter.mjs';
import { ResponsesSseParser } from './sse.mjs';

const upstream = new URL(process.env.VELLUM_LAB_UPSTREAM ?? 'http://127.0.0.1:15721');
const port = Number(process.env.VELLUM_LAB_PORT ?? 15722);
const mode = process.env.VELLUM_LAB_MODE ?? 'S0';
const sessions = new Map();
const eventLog = process.env.VELLUM_LAB_EVENT_LOG;
let recordChain = Promise.resolve();
const intervention = '最近的工具操作可能陷入重複。請依原始需求重新核對自行建立的測試預期，區分實作、測試預期與環境問題，選擇一個能區分原因的最小檢查，再決定下一步。只使用目前提供的工具。';

function record(name, fields = {}) {
  if (!eventLog) return;
  recordChain = recordChain.then(() => appendFile(eventLog, JSON.stringify({ timestamp: new Date().toISOString(), name, fields }) + '\n'));
}

async function session(id) {
  if (!sessions.has(id)) sessions.set(id, {
    detector: await createQwenDetector(), triggered: false, injected: false,
  });
  return sessions.get(id);
}

function inject(body, state) {
  if (mode !== 'S2' || !state.triggered || state.injected || !Array.isArray(body.input)) return body;
  state.injected = true;
  record('lab.recovery_prompt.injected');
  return { ...body, input: [...body.input, { role: 'user', content: [{ type: 'input_text', text: intervention }] }] };
}

function captureSse(chunk, state, parser) {
  for (const call of parser.push(chunk)) {
    if (state.detector.addToolCall(call) && !state.triggered) {
      state.triggered = true;
      record('lab.qwen_loop.detected', { tool: call.name, loopType: state.detector.lastLoopType() });
    }
    state.detector.addFinished();
  }
}

const server = http.createServer(async (request, response) => {
  if (request.url === '/healthz') {
    response.writeHead(200, { 'content-type': 'application/json' });
    response.end(JSON.stringify({ ready: true, mode }));
    return;
  }
  const chunks = [];
  for await (const chunk of request) chunks.push(chunk);
  const headerSession = request.headers['x-vellum-lab-session'];
  const sessionId = typeof headerSession === 'string' && headerSession
    ? headerSession
    : process.env.VELLUM_LAB_SESSION ?? 'single-process-session';
  const state = await session(sessionId);
  if (mode === 'S1' && state.triggered) {
    record('lab.qwen_loop.stopped');
    response.writeHead(409, { 'content-type': 'application/json', 'x-vellum-lab-stop': 'qwen-loop' });
    response.end(JSON.stringify({ error: { type: 'lab_loop_stop' } }));
    return;
  }
  let raw = Buffer.concat(chunks);
  if ((request.headers['content-type'] ?? '').includes('application/json')) {
    raw = Buffer.from(JSON.stringify(inject(JSON.parse(raw.toString('utf8')), state)));
  }
  const headers = { ...request.headers, host: upstream.host, 'content-length': String(raw.length) };
  const proxy = http.request(new URL(request.url, upstream), { method: request.method, headers }, upstreamResponse => {
    const parser = new ResponsesSseParser();
    response.writeHead(upstreamResponse.statusCode ?? 502, upstreamResponse.headers);
    upstreamResponse.setEncoding('utf8');
    upstreamResponse.on('data', chunk => { captureSse(chunk, state, parser); response.write(chunk); });
    upstreamResponse.on('end', () => response.end());
  });
  request.on('aborted', () => proxy.destroy());
  proxy.on('error', error => { if (!response.headersSent) response.writeHead(502); response.end(error.message); });
  proxy.end(raw);
});
server.listen(port, '127.0.0.1', () => console.log(JSON.stringify({ status: 'listening', port, upstream: upstream.href, mode })));
process.on('SIGTERM', () => server.close(async () => { await recordChain; process.exit(0); }));
process.on('SIGINT', () => server.close(async () => { await recordChain; process.exit(0); }));
