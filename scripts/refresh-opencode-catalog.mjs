// Refreshes the bundled OpenCode Zen / Go model metadata.
//
// Zen's own `/models` response carries no capability fields at all -- just
// `id`, `object`, `created`, `owned_by` -- so Vellum has to bundle the context
// window, reasoning support and retirement status from somewhere.
// That somewhere is models.dev, the registry OpenCode itself publishes its
// catalog through, under the `opencode` and `opencode-go` providers. Running
// this is how those tables get their numbers; they are not guessed from ids.
//
// Ids are the union of what the two endpoints serve right now and what the
// tables already carried. Nothing is dropped: a model OpenCode has withdrawn
// keeps its row so a route that still names it keeps its context window, and
// the registry's `status` marks it deprecated so it sorts behind live ones.
// The one exception is OpenCode's free tier (`-free` ids and `big-pickle`):
// OpenCode serves it only to its own app, so Vellum never lists it.
//
//   node scripts/refresh-opencode-catalog.mjs [--check]
//
// `--check` reports drift and exits non-zero instead of writing.

import { execFileSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const probePath = join(root, "src-tauri", "src", "probe.rs");
const checkOnly = process.argv.includes("--check");

const CATALOGS = [
  {
    label: "Zen",
    constant: "OPENCODE_ZEN_MODEL_METADATA",
    provider: "opencode",
    endpoint: "https://opencode.ai/zen/v1/models",
  },
  {
    label: "Go",
    constant: "OPENCODE_GO_MODEL_METADATA",
    provider: "opencode-go",
    endpoint: "https://opencode.ai/zen/go/v1/models",
  },
];

async function fetchJson(url) {
  const response = await fetch(url, { redirect: "follow" });
  if (!response.ok) {
    throw new Error(`GET ${url} failed: HTTP ${response.status} ${response.statusText}`);
  }
  return response.json();
}

function readSource(path) {
  const text = readFileSync(path, "utf8");
  return { crlf: text.includes("\r\n"), text: text.replaceAll("\r\n", "\n") };
}

function writeSource(path, source, text) {
  writeFileSync(path, source.crlf ? text.replaceAll("\n", "\r\n") : text);
}

function block(text, header, constant) {
  const opening = `const ${constant}: ${header} = &[\n`;
  const start = text.indexOf(opening);
  if (start < 0) {
    throw new Error(`${constant} is not declared the way this script edits it`);
  }
  const end = text.indexOf("\n];", start + opening.length);
  if (end < 0) {
    throw new Error(`${constant} has no terminator`);
  }
  return { start: start + opening.length, end: end + 1, body: text.slice(start + opening.length, end + 1) };
}

function existingIds(body) {
  return [...body.matchAll(/^\s*\("([^"]+)"/gm)].map((match) => match[1]);
}

// Rows are generated one per line and then handed to rustfmt, which wraps the
// ones whose ids push the tuple past `fn_call_width`. So the file on disk never
// matches the generated text verbatim, even immediately after a write --
// comparing the two raw strings made `--check` report drift forever. Drift is a
// question about content, so collapse the whitespace rustfmt owns before
// asking it.
function sameRows(a, b) {
  // Two things rustfmt owns and this script does not: it breaks a wrapped row
  // after the opening paren, and it adds a trailing comma before the closing
  // one. Neither changes a single value, so strip both before comparing --
  // otherwise `--check` reports drift against the file it just wrote.
  const flatten = (text) => text.replace(/\s+/g, "").replaceAll(",)", ")");
  return flatten(a) === flatten(b);
}

// Mirrors `vellum_proxy_runtime::opencode::is_free_tier_model`.
function isFreeTier(id) {
  const model = id.trim().toLowerCase();
  return model.endsWith("-free") || model === "big-pickle";
}

function row(id, entry) {
  const context = entry.limit?.context;
  return `    ("${id}", ${context ? `Some(${context})` : "None"}, ${Boolean(entry.reasoning)}, ${entry.status === "deprecated"}),\n`;
}

const registry = await fetchJson("https://models.dev/api.json");
let probe = readSource(probePath);
let drifted = false;
const unknown = [];

for (const catalog of CATALOGS) {
  const models = registry[catalog.provider]?.models;
  if (!models) {
    throw new Error(`models.dev has no ${catalog.provider} provider`);
  }
  const live = (await fetchJson(catalog.endpoint)).data.map((model) => model.id);
  const current = block(probe.text, "&[OpenCodeZenModelRow]", catalog.constant);
  const ids = [...new Set([...existingIds(current.body), ...live])]
    .filter((id) => !isFreeTier(id))
    .sort();

  let body = "";
  for (const id of ids) {
    const entry = models[id];
    if (!entry) {
      unknown.push(`${catalog.label}: ${id}`);
      const kept = current.body
        .split("\n")
        .find((line) => line.trimStart().startsWith(`("${id}",`));
      if (kept) {
        body += `${kept}\n`;
      }
      continue;
    }
    body += row(id, entry);
  }

  if (!sameRows(body, current.body)) {
    drifted = true;
    console.log(`${catalog.label}: ${existingIds(current.body).length} rows -> ${existingIds(body).length}`);
  }
  probe.text = probe.text.slice(0, current.start) + body + probe.text.slice(current.end);
}

if (unknown.length) {
  console.log(`not in models.dev (row left as-is, or absent): ${unknown.join(", ")}`);
}
if (checkOnly) {
  console.log(drifted ? "bundled OpenCode catalog is stale" : "bundled OpenCode catalog is current");
  process.exit(drifted ? 1 : 0);
}
writeSource(probePath, probe, probe.text);
// Rows are emitted on one line; rustfmt splits the ones whose ids push the
// tuple past `fn_call_width`.
execFileSync("rustfmt", ["--edition", "2021", probePath], { stdio: "inherit" });
console.log(drifted ? "rewrote the bundled OpenCode catalog" : "bundled OpenCode catalog was already current");
