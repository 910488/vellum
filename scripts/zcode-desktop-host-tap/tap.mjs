#!/usr/bin/env node
/**
 * Sit between ZCode Desktop host and its app-server stdio.
 * Desktop still owns CAPTCHA / refreshCodingPlanApiKey; this process only
 * proxies JSONL and records method-level traffic.
 *
 * Spawned by Desktop when ZCODE_AGENT_SERVER_COMMAND points at node and
 * ZCODE_AGENT_SERVER_ARGS_JSON is ["<this-file>"]. Desktop also appends
 * `--surface desktop`, which this process ignores.
 *
 * Method: docs/zcode-desktop-host-tap.md
 */
import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { createInterface } from "node:readline";
import fs from "node:fs";
import net from "node:net";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const DEFAULT_ZCODE_ROOT = path.join(
  process.env.LOCALAPPDATA ?? "",
  "Programs",
  "ZCode",
);
const ZCODE_EXE =
  process.env.ZCODE_TAP_INNER_EXE ?? path.join(DEFAULT_ZCODE_ROOT, "ZCode.exe");
const ZCODE_CJS =
  process.env.ZCODE_TAP_INNER_CJS ??
  path.join(DEFAULT_ZCODE_ROOT, "resources", "glm", "zcode.cjs");
const LOG_DIR =
  process.env.ZCODE_TAP_LOG_DIR ??
  path.join(process.env.TEMP ?? path.resolve(HERE, "../../target"), "zcode-host-tap");

const SECRET_KEY =
  /captcha|apikey|api_key|authorization|token|cookie|secret|credential|passwd|password|runtimeProviderHeaders/i;
const injectPrompt = process.env.ZCODE_TAP_INJECT_PROMPT?.trim() || "";
const queuePath = path.join(LOG_DIR, "inject-queue.jsonl");

fs.mkdirSync(LOG_DIR, { recursive: true });
const logPath = path.join(LOG_DIR, `tap-${process.pid}.jsonl`);

function log(event) {
  try {
    fs.appendFileSync(
      logPath,
      `${JSON.stringify({ ts: new Date().toISOString(), pid: process.pid, ...event })}\n`,
    );
  } catch (error) {
    process.stderr.write(`tap-log-error ${error}\n`);
  }
}

function summarize(value) {
  if (value == null || typeof value !== "object") return { kind: typeof value };
  const keys = Object.keys(value);
  const out = { keys };
  const copy = (name) => {
    const v = value[name];
    if (typeof v === "string" || typeof v === "number" || typeof v === "boolean") out[name] = v;
  };
  for (const name of [
    "method",
    "reason",
    "providerId",
    "sessionId",
    "requestId",
    "turnId",
    "workspacePath",
    "headersApplied",
    "errorMessage",
    "providerRevision",
    "inputId",
    "type",
    "clientMode",
    "commandId",
    "kind",
    "status",
    "querySource",
    "modelId",
  ]) {
    copy(name);
  }
  if (value.modelRef && typeof value.modelRef === "object") {
    out.modelRef = {
      providerId: value.modelRef.providerId,
      modelId: value.modelRef.modelId,
    };
  }
  if (value.model && typeof value.model === "object") {
    out.model = {
      providerId: value.model.providerId,
      modelId: value.model.modelId,
    };
  }
  if (value.workspace && typeof value.workspace === "object") {
    out.workspacePath = value.workspace.workspacePath;
  }
  if (keys.some((key) => SECRET_KEY.test(key))) out.redactedSecretKeys = keys.filter((key) => SECRET_KEY.test(key));
  return out;
}

// Record only structural metadata for the private v4 conversation envelope.
// This is intentionally content-free: it lets qualification follow packaged
// ZCode wire changes without persisting prompts, responses, or runtime headers.
function summarizeWireShape(value, depth = 0) {
  if (value == null || depth > 6) return null;
  if (Array.isArray(value)) {
    return {
      arrayLength: value.length,
      first: value.length ? summarizeWireShape(value[0], depth + 1) : null,
      items: value.slice(0, 16).map((item) => summarizeWireShape(item, depth + 1)),
    };
  }
  if (typeof value !== "object") {
    return typeof value === "string"
      ? { type: "string", length: value.length }
      : { type: typeof value };
  }
  const result = { keys: Object.keys(value) };
  for (const key of ["kind", "type", "role", "status", "state", "phase", "op", "channel", "deliveryKind"]) {
    if (typeof value[key] === "string") result[key] = value[key];
  }
  for (const key of ["hasBackgroundWork", "sessionEnded"]) {
    if (typeof value[key] === "boolean") result[key] = value[key];
  }
  for (const key of Object.keys(value)) {
    if (SECRET_KEY.test(key)) continue;
    if (value[key] && typeof value[key] === "object") {
      result[key] = summarizeWireShape(value[key], depth + 1);
    }
  }
  return result;
}

function parseLine(line) {
  try {
    return JSON.parse(line);
  } catch {
    return null;
  }
}

const inner = spawn(
  ZCODE_EXE,
  [ZCODE_CJS, "app-server", "--stdio", "--surface", "desktop", "--no-color"],
  {
    cwd: process.cwd(),
    env: { ...process.env, ELECTRON_RUN_AS_NODE: "1" },
    stdio: ["pipe", "pipe", "pipe"],
    windowsHide: true,
  },
);

log({
  event: "inner-spawn",
  innerPid: inner.pid,
  cwd: process.cwd(),
  argv: process.argv.slice(2),
  inject: Boolean(injectPrompt),
});
process.stderr.write(`zcode-host-tap pid=${process.pid} inner=${inner.pid} log=${logPath}\n`);

let nextInjectId = 900001;
let seenSessionId;
let resumedSessionId;
let queueOffset = 0;
let seenEnvInject = false;

function noteSession(message, from) {
  const params = message?.params;
  const result = message?.result;
  const sessionId =
    params?.sessionId ||
    result?.session?.sessionId ||
    result?.sessionId ||
    params?.session?.sessionId;
  if (typeof sessionId === "string" && sessionId) {
    if (from === "host" && message.method === "session/resume") {
      resumedSessionId = sessionId;
    }
    const preferResume = from === "host" && message.method === "session/resume";
    const preferHostCommand = from === "host" && message.method === "v4/command";
    if (
      sessionId !== seenSessionId &&
      (preferResume || preferHostCommand || !seenSessionId || from === "host")
    ) {
      seenSessionId = sessionId;
      log({ event: "session-id", sessionId, from, method: message.method });
    }
  }
}

function injectSend(sessionId, content, meta = {}) {
  if (!inner.stdin.writable) {
    log({ event: "inject-skip", reason: "stdin-closed", sessionId });
    return null;
  }
  const nativeRequestId = nextInjectId++;
  const inputId = `vellum-tap-${Date.now()}-${nativeRequestId}`;
  const frame = {
    id: nativeRequestId,
    method: "session/send",
    params: {
      sessionId,
      inputId,
      content,
      toolDenylist: ["Bash", "Edit", "Write"],
    },
  };
  log({ event: "inject", method: frame.method, id: frame.id, sessionId, vellumTurnId: meta.vellumTurnId ?? null });
  inner.stdin.write(`${JSON.stringify(frame)}\n`);
  return { nativeRequestId: String(nativeRequestId), inputId, sessionId };
}

function drainQueue() {
  if (!fs.existsSync(queuePath)) return;
  const text = fs.readFileSync(queuePath, "utf8").replace(/^\uFEFF/, "");
  if (queueOffset > text.length) queueOffset = 0;
  const chunk = text.slice(queueOffset);
  const lines = chunk.split(/\n/);
  const complete = lines.slice(0, -1);
  queueOffset += complete.join("\n").length + (complete.length ? 1 : 0);
  for (const line of complete) {
    if (!line.trim()) continue;
    let job;
    try {
      job = JSON.parse(line);
    } catch (error) {
      log({ event: "inject-queue-error", detail: String(error) });
      continue;
    }
    const sessionId = job.sessionId || resumedSessionId || seenSessionId;
    const content = typeof job.content === "string" ? job.content.trim() : "";
    if (!sessionId || !content) {
      log({ event: "inject-skip", reason: "missing-session-or-content", sessionId: sessionId ?? null });
      continue;
    }
    injectSend(sessionId, content);
  }
}

function forward(line, from, dest) {
  const message = parseLine(line);
  if (message) {
    log({
      event: "frame",
      from,
      id: message.id ?? null,
      method: message.method ?? null,
      hasResult: Object.hasOwn(message, "result"),
      hasError: Object.hasOwn(message, "error"),
      errorCode: message.error?.code,
      errorMessage: typeof message.error?.message === "string" ? message.error.message.slice(0, 240) : undefined,
      summary: summarize(message.params ?? message.result ?? message.error ?? {}),
      wireShape:
        message.method === "v4/conversation/frame"
          ? summarizeWireShape(message.params)
          : undefined,
    });
    noteSession(message, from);
    if (from === "app-server") observeAppServer(message);
  } else {
    log({ event: "non-json", from, bytes: line.length });
  }
  dest.write(`${line}\n`);
}

createInterface({ input: process.stdin }).on("line", (line) => {
  if (inner.stdin.writable) forward(line, "host", inner.stdin);
});
createInterface({ input: inner.stdout }).on("line", (line) => {
  forward(line, "app-server", process.stdout);
  if (injectPrompt && resumedSessionId && !seenEnvInject) {
    seenEnvInject = true;
    setTimeout(() => injectSend(resumedSessionId, injectPrompt), 1500);
  }
});

const CONTROL_PROTOCOL_VERSION = 2;
const cjsSha256 = fs.existsSync(ZCODE_CJS)
  ? createHash("sha256").update(fs.readFileSync(ZCODE_CJS)).digest("hex")
  : "unknown";
const controlPipe =
  process.platform === "win32"
    ? `\\\\.\\pipe\\vellum-zcode-tap-${process.pid}`
    : path.join(LOG_DIR, `tap-${process.pid}.sock`);
const threadToSession = new Map();
const inflightByTurn = new Map();
const pendingBySession = new Map();
const liveSessions = new Set();
const seenConversationFrames = new Set();
let controlSocket = null;
let lifecycle = "ready";

function controlWrite(frame) {
  if (!controlSocket || controlSocket.destroyed) return;
  controlSocket.write(`${JSON.stringify(frame)}\n`);
}

function controlEvent(method, params) {
  log({ event: "control-event", method, summary: summarize(params) });
  controlWrite({ method, params });
}

function controlError(id, code, message) {
  controlWrite({ id, error: { code, message } });
}

function sessions() {
  const ids = [...liveSessions];
  if (seenSessionId && !ids.includes(seenSessionId)) ids.push(seenSessionId);
  if (resumedSessionId && !ids.includes(resumedSessionId)) ids.push(resumedSessionId);
  return ids;
}

function rememberLiveSession(sessionId) {
  if (!sessionId) return;
  if (!liveSessions.has(sessionId)) {
    liveSessions.add(sessionId);
    controlEvent("session/announced", { sessionId, source: "app-server" });
  }
}

function extractText(value, depth = 0) {
  if (value == null || depth > 5) return;
  if (typeof value === "string") return;
  if (typeof value !== "object") return;
  if (typeof value.text === "string" && value.text) return value.text;
  if (typeof value.delta === "string" && value.delta) return value.delta;
  for (const nested of Object.values(value)) {
    const text = extractText(nested, depth + 1);
    if (text) return text;
  }
}

function observeAppServer(message) {
  const params = message.params ?? {};
  const sessionId = params.sessionId;
  if (typeof sessionId === "string") rememberLiveSession(sessionId);
  if (message.method === "interaction/requestProviderRuntimeHeaders") {
    if (params.reason === "captcha-retry") {
      lifecycle = "captcha-waiting";
      controlEvent("lifecycle", { state: "captcha-waiting", detail: params.reason });
    }
    const queue = pendingBySession.get(sessionId) ?? [];
    const turn = queue[0] && inflightByTurn.get(queue[0]);
    if (turn) {
      if (params.turnId) turn.nativeTurnId = params.turnId;
      if (params.providerId) turn.providerId = params.providerId;
      if (params.modelRef?.modelId) turn.modelId = params.modelRef.modelId;
    }
  }
  if (message.method === "v4/telemetry/event") {
    const turnId = params.turnId;
    const match = [...inflightByTurn.values()].find(
      (item) => item.sessionId === sessionId && (item.nativeTurnId === turnId || !item.nativeTurnId),
    );
    if (!match) return;
    if (!match.nativeTurnId && turnId) match.nativeTurnId = turnId;
    if (params.errorMessage) {
      finishTurn(match, "failed", params.errorCode, params.errorMessage);
      return;
    }
    if (params.assistantMessageId || params.chunkLength || params.firstChunk) {
      controlEvent("turn/delta", {
        vellumTurnId: match.vellumTurnId,
        sessionId: match.sessionId,
        nativeTurnId: match.nativeTurnId,
        text: extractText(params),
      });
    }
    if (
      Number.isFinite(params.inputTokens) ||
      Number.isFinite(params.outputTokens) ||
      Number.isFinite(params.totalTokens)
    ) {
      controlEvent("turn/usage", {
        vellumTurnId: match.vellumTurnId,
        sessionId: match.sessionId,
        usage: {
          inputTokens: params.inputTokens,
          outputTokens: params.outputTokens,
          totalTokens: params.totalTokens,
          reasoningTokens: params.reasoningTokens,
          cacheReadTokens: params.cacheReadTokens,
          cacheWriteTokens: params.cacheWriteTokens,
        },
      });
    }
    if (params.resultType || (params.status && params.durationMs != null && params.toolCallCount != null)) {
      scheduleFinishTurn(match, params.status === "failed" ? "failed" : "completed", params.errorCode, params.errorMessage);
    }
  }
  if (message.method === "v4/conversation/frame") {
    const frameId = params.logicalFrameId;
    if (frameId && seenConversationFrames.has(frameId)) return;
    if (frameId) {
      seenConversationFrames.add(frameId);
      if (seenConversationFrames.size > 4096) {
        seenConversationFrames.delete(seenConversationFrames.values().next().value);
      }
    }
    const deltas = params.frame?.payload?.deltas;
    if (!Array.isArray(deltas)) return;
    let terminalSession = null;
    for (const delta of deltas) {
      if (delta?.session?.sessionId && /^completed/i.test(delta.session.phase ?? "")) {
        terminalSession = delta.session;
      }
      const row = delta?.row;
      if (row?.kind === "toolCall" && row.toolCallId) {
        const toolKey = String(row.toolCallId);
        let toolMatch = [...inflightByTurn.values()].find((item) => item.toolStates.has(toolKey));
        if (!toolMatch && row.status === "inputStreaming" && inflightByTurn.size === 1) {
          toolMatch = inflightByTurn.values().next().value;
          toolMatch.toolStates.set(toolKey, "");
        }
        if (toolMatch) {
          const signature = JSON.stringify({ status: row.status, input: row.input ?? null });
          const previous = toolMatch.toolStates.get(toolKey);
          if (signature !== previous) {
            toolMatch.toolStates.set(toolKey, signature);
            const common = {
              vellumTurnId: toolMatch.vellumTurnId,
              sessionId: toolMatch.sessionId,
              toolCallId: toolKey,
              name: row.toolName || "tool",
              arguments: row.input,
            };
            if (row.status === "inputStreaming") {
              controlEvent(previous === "" ? "tool/started" : "tool/updated", common);
            } else if (row.status === "running") {
              controlEvent("tool/updated", common);
            } else if (["success", "failed", "error", "cancelled"].includes(row.status)) {
              controlEvent("tool/completed", {
                ...common,
                result: row.output ?? null,
                isError: row.status !== "success",
              });
            }
          }
        }
      }
      if (row?.kind !== "assistantText" || typeof row.text !== "string") continue;
      const rowKey = String(row.rowId || row.entityId || row.assistantResponseId || "assistant");
      let match = [...inflightByTurn.values()].find((item) => item.assistantRowIds.has(rowKey));
      if (!match && row.state === "streaming" && inflightByTurn.size === 1) {
        match = inflightByTurn.values().next().value;
        match.assistantRowIds.add(rowKey);
      }
      log({
        event: "assistant-row",
        rowId: row.rowId,
        rowTurnId: row.turnId,
        productTurnId: row.productTurnId,
        state: row.state,
        textLength: row.text.length,
        matchedVellumTurnId: match?.vellumTurnId ?? null,
      });
      if (!match) continue;
      const previous = match.assistantTextByRow.get(rowKey) ?? "";
      const text = row.text.startsWith(previous) ? row.text.slice(previous.length) : row.text;
      match.assistantTextByRow.set(rowKey, row.text);
      if (text) {
        controlEvent("turn/delta", {
          vellumTurnId: match.vellumTurnId,
          sessionId: match.sessionId,
          nativeTurnId: match.nativeTurnId,
          text,
        });
      }
      if (row.state === "complete" && !match.completedAssistantRowIds.has(rowKey)) {
        match.completedAssistantRowIds.add(rowKey);
        controlEvent("assistant/completed", {
          vellumTurnId: match.vellumTurnId,
          sessionId: match.sessionId,
          messageId: rowKey,
          text: row.text,
        });
      }
      if (row.state === "complete" && match.pendingCompletion) {
        const pending = match.pendingCompletion;
        finishTurn(match, pending.outcome, pending.errorCode, pending.errorMessage);
      }
    }
    if (terminalSession) {
      const match = [...inflightByTurn.values()].find(
        (item) => item.sessionId === terminalSession.sessionId,
      );
      if (match) {
        scheduleFinishTurn(
          match,
          terminalSession.phase === "completedSuccess" ? "completed" : "failed",
          terminalSession.phase === "completedSuccess" ? undefined : terminalSession.phase,
        );
      }
    }
  }
}

function scheduleFinishTurn(turn, outcome, errorCode, errorMessage) {
  turn.pendingCompletion = { outcome, errorCode, errorMessage };
  if (turn.terminalTimer) clearTimeout(turn.terminalTimer);
  turn.terminalTimer = setTimeout(
    () => finishTurn(turn, outcome, errorCode, errorMessage),
    300,
  );
}

function finishTurn(turn, outcome, errorCode, errorMessage) {
  if (!inflightByTurn.has(turn.vellumTurnId)) return;
  inflightByTurn.delete(turn.vellumTurnId);
  const queue = pendingBySession.get(turn.sessionId) ?? [];
  pendingBySession.set(
    turn.sessionId,
    queue.filter((id) => id !== turn.vellumTurnId),
  );
  if (turn.timer) clearTimeout(turn.timer);
  if (turn.terminalTimer) clearTimeout(turn.terminalTimer);
  controlEvent("turn/completed", {
    vellumTurnId: turn.vellumTurnId,
    sessionId: turn.sessionId,
    outcome,
    providerId: turn.providerId ?? undefined,
    modelId: turn.modelId ?? undefined,
    errorCode: errorCode ?? undefined,
    errorMessage: errorMessage ?? undefined,
  });
}

function handleControl(line) {
  let frame;
  try {
    frame = JSON.parse(line);
  } catch {
    return;
  }
  const id = frame.id;
  const method = frame.method;
  const params = frame.params ?? {};
  if (method === "hello") {
    if (params.protocolVersion !== CONTROL_PROTOCOL_VERSION) {
      controlError(id, "PROTOCOL_MISMATCH", String(params.protocolVersion ?? ""));
      return;
    }
    if (params.expectedCjsSha256 && params.expectedCjsSha256 !== cjsSha256) {
      controlError(id, "ARTIFACT_MISMATCH", cjsSha256);
      return;
    }
    controlWrite({
      id,
      result: {
        protocolVersion: CONTROL_PROTOCOL_VERSION,
        artifact: {
          cjsSha256,
          productVersion: process.env.ZCODE_APP_VERSION ?? null,
        },
        pid: process.pid,
        innerPid: inner.pid,
        sessions: sessions(),
        lifecycle,
      },
    });
    return;
  }
  if (method === "bind") {
    const sessionId = params.sessionId || resumedSessionId || seenSessionId || sessions()[0];
    if (!sessionId) {
      controlError(id, "SESSION_NOT_FOUND", "no live session");
      return;
    }
    const existing = threadToSession.get(params.vellumThreadId);
    if (existing && params.sessionId && existing !== params.sessionId) {
      controlError(id, "THREAD_ALREADY_BOUND", existing);
      return;
    }
    threadToSession.set(params.vellumThreadId, sessionId);
    rememberLiveSession(sessionId);
    controlWrite({
      id,
      result: { vellumThreadId: params.vellumThreadId, sessionId },
    });
    return;
  }
  if (method === "turn/start") {
    const sessionId =
      threadToSession.get(params.vellumThreadId) || params.sessionId || seenSessionId;
    if (!sessionId) {
      controlError(id, "SESSION_NOT_FOUND", params.vellumThreadId ?? "");
      return;
    }
    if (!params.vellumTurnId || !params.content?.trim()) {
      controlError(id, "BAD_PARAMS", "vellumTurnId and content are required");
      return;
    }
    if (inflightByTurn.has(params.vellumTurnId)) {
      controlError(id, "DUPLICATE_TURN", params.vellumTurnId);
      return;
    }
    const injected = injectSend(sessionId, params.content, { vellumTurnId: params.vellumTurnId });
    if (!injected) {
      controlError(id, "TAP_UNAVAILABLE", "app-server stdin closed");
      return;
    }
    const turn = {
      vellumTurnId: params.vellumTurnId,
      vellumThreadId: params.vellumThreadId,
      sessionId,
      nativeRequestId: injected.nativeRequestId,
      inputId: injected.inputId,
      nativeTurnId: null,
      providerId: null,
      modelId: null,
      assistantRowIds: new Set(),
      completedAssistantRowIds: new Set(),
      assistantTextByRow: new Map(),
      toolStates: new Map(),
      pendingCompletion: null,
      terminalTimer: null,
      timer: null,
    };
    if (params.timeoutMs) {
      turn.timer = setTimeout(() => finishTurn(turn, "timeout", "TIMEOUT", "turn timed out"), params.timeoutMs);
    }
    inflightByTurn.set(params.vellumTurnId, turn);
    const queue = pendingBySession.get(sessionId) ?? [];
    queue.push(params.vellumTurnId);
    pendingBySession.set(sessionId, queue);
    controlWrite({
      id,
      result: {
        vellumTurnId: params.vellumTurnId,
        sessionId,
        nativeRequestId: injected.nativeRequestId,
        inputId: injected.inputId,
      },
    });
    controlEvent("turn/started", {
      vellumTurnId: turn.vellumTurnId,
      sessionId: turn.sessionId,
      nativeTurnId: turn.nativeTurnId ?? undefined,
    });
    return;
  }
  if (method === "turn/cancel") {
    const turn = inflightByTurn.get(params.vellumTurnId);
    if (!turn) {
      controlError(id, "TURN_NOT_FOUND", params.vellumTurnId ?? "");
      return;
    }
    if (inner.stdin.writable) {
      inner.stdin.write(`${JSON.stringify({ id: nextInjectId++, method: "session/stop", params: { sessionId: turn.sessionId } })}\n`);
    }
    finishTurn(turn, "cancelled");
    controlWrite({ id, result: { cancelled: true } });
    return;
  }
  if (method === "status") {
    controlWrite({
      id,
      result: {
        lifecycle,
        sessions: sessions(),
        inflightTurns: [...inflightByTurn.keys()],
      },
    });
    return;
  }
  controlError(id, "UNKNOWN_METHOD", method ?? "");
}

function startControl() {
  const server = net.createServer((socket) => {
    if (controlSocket && !controlSocket.destroyed) {
      socket.end(`${JSON.stringify({ error: { code: "BUSY", message: "control client already connected" } })}\n`);
      return;
    }
    controlSocket = socket;
    createInterface({ input: socket }).on("line", handleControl);
    socket.on("close", () => {
      if (controlSocket === socket) controlSocket = null;
    });
  });
  server.on("error", (error) => log({ event: "control-error", detail: String(error) }));
  server.listen(controlPipe, () => {
    const listenPath = path.join(LOG_DIR, `tap-${process.pid}.listen.json`);
    fs.writeFileSync(
      listenPath,
      `${JSON.stringify({
        schemaVersion: 1,
        protocolVersion: CONTROL_PROTOCOL_VERSION,
        pid: process.pid,
        innerPid: inner.pid,
        pipe: controlPipe,
        artifact: { cjsSha256, productVersion: process.env.ZCODE_APP_VERSION ?? null },
        startedAt: new Date().toISOString(),
      })}\n`,
    );
    log({ event: "control-listen", pipe: controlPipe });
  });
}

startControl();
setInterval(drainQueue, 400);
process.on("uncaughtException", (error) => {
  log({ event: "uncaught", detail: String(error) });
});
inner.stderr.on("data", (chunk) => {
  const text = chunk.toString("utf8");
  const preview = SECRET_KEY.test(text)
    ? undefined
    : text.replace(/\s+/g, " ").slice(0, 160);
  log({ event: "inner-stderr", bytes: chunk.length, redacted: !preview, preview });
  process.stderr.write(text);
});

function shutdown(reason) {
  log({ event: "shutdown", reason, innerPid: inner.pid });
  lifecycle = "exited";
  controlEvent("lifecycle", { state: "exited", detail: reason });
  try {
    fs.unlinkSync(path.join(LOG_DIR, `tap-${process.pid}.listen.json`));
  } catch {
    // ignore
  }
  try {
    inner.kill();
  } catch {
    // ignore
  }
}

inner.on("exit", (code, signal) => {
  log({ event: "inner-exit", code, signal });
  lifecycle = "app-server-restart";
  controlEvent("lifecycle", { state: "app-server-restart", detail: String(code ?? signal ?? "") });
  process.exit(code ?? 1);
});
process.on("SIGINT", () => shutdown("sigint"));
process.on("SIGTERM", () => shutdown("sigterm"));
process.stdin.on("close", () => shutdown("stdin-close"));
process.stdin.on("end", () => shutdown("stdin-end"));
