import { readFile } from 'node:fs/promises';

export async function readRollout(path) {
  const rows = (await readFile(path, 'utf8')).split(/\r?\n/).filter(Boolean).map(JSON.parse);
  const events = [];
  for (const row of rows) {
    if (row.type !== 'response_item') continue;
    const item = row.payload ?? {};
    if (item.type !== 'function_call' && item.type !== 'custom_tool_call') continue;
    let args = item.arguments ?? item.input ?? {};
    if (typeof args === 'string') {
      try { args = JSON.parse(args); } catch { args = { input: args }; }
    }
    events.push({ callId: item.call_id, providerCallId: item.call_id, name: item.name, args });
  }
  return events;
}
