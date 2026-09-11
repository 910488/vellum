#!/usr/bin/env node
import fs from "node:fs";
import net from "node:net";
import path from "node:path";
import process from "node:process";
import { createInterface } from "node:readline";

function arg(name) {
  const prefix = `--${name}`;
  const idx = process.argv.indexOf(prefix);
  if (idx >= 0 && process.argv[idx + 1]) return process.argv[idx + 1];
  const inline = process.argv.find((value) => value.startsWith(`${prefix}=`));
  return inline ? inline.slice(prefix.length + 1) : undefined;
}

const logDir = process.env.ZCODE_TAP_LOG_DIR ?? path.join(process.env.TEMP ?? "/tmp", "zcode-host-tap");
const content = arg("content");
const threadId = arg("thread-id") ?? "adhoc";
if (!content?.trim()) {
  process.stderr.write(
    "usage: inject-send.mjs --content <text> [--session-id sess_…] [--thread-id ui-…]\n",
  );
  process.exit(2);
}

function newestListen() {
  if (!fs.existsSync(logDir)) return null;
  const files = fs
    .readdirSync(logDir)
    .filter((name) => name.endsWith(".listen.json"))
    .map((name) => ({ name, mtime: fs.statSync(path.join(logDir, name)).mtimeMs }))
    .sort((a, b) => b.mtime - a.mtime);
  if (files.length === 0) return null;
  return JSON.parse(fs.readFileSync(path.join(logDir, files[0].name), "utf8"));
}

const listen = newestListen();
if (!listen?.pipe) {
  process.stderr.write(`no tap-*.listen.json under ${logDir}; is Desktop running with the tap?\n`);
  process.exit(2);
}

const sessionId = arg("session-id");
const socket = net.connect(listen.pipe);
let nextId = 1;
const pending = new Map();

function request(method, params) {
  const id = String(nextId++);
  socket.write(`${JSON.stringify({ id, method, params })}\n`);
  return new Promise((resolve, reject) => pending.set(id, { resolve, reject }));
}

socket.on("error", (error) => {
  process.stderr.write(`${error.message}\n`);
  process.exit(1);
});

createInterface({ input: socket }).on("line", (line) => {
  let message;
  try {
    message = JSON.parse(line);
  } catch {
    return;
  }
  if (message.id && pending.has(String(message.id))) {
    const entry = pending.get(String(message.id));
    pending.delete(String(message.id));
    if (message.error) entry.reject(new Error(`${message.error.code}: ${message.error.message}`));
    else entry.resolve(message.result);
  } else if (message.method === "turn/completed") {
    process.stdout.write(`${JSON.stringify(message.params)}\n`);
    socket.end();
  } else if (message.method === "turn/delta" && message.params?.text) {
    process.stdout.write(`${message.params.text}\n`);
  } else if (message.method === "assistant/completed") {
    process.stdout.write(
      `${JSON.stringify({
        method: message.method,
        messageId: message.params?.messageId,
        textLength: message.params?.text?.length ?? 0,
      })}\n`,
    );
  } else if (message.method?.startsWith("tool/")) {
    process.stdout.write(
      `${JSON.stringify({
        method: message.method,
        toolCallId: message.params?.toolCallId,
        name: message.params?.name,
        isError: message.params?.isError,
      })}\n`,
    );
  }
});

socket.on("connect", async () => {
  try {
    await request("hello", { protocolVersion: 2, client: "inject-send" });
    await request("bind", {
      vellumThreadId: threadId,
      sessionId: sessionId ?? undefined,
    });
    const started = await request("turn/start", {
      vellumThreadId: threadId,
      vellumTurnId: `adhoc-${Date.now()}`,
      content: content.trim(),
      timeoutMs: 120000,
    });
    process.stdout.write(`started ${JSON.stringify(started)}\n`);
  } catch (error) {
    process.stderr.write(`${error.message}\n`);
    process.exit(1);
  }
});
