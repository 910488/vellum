export class ResponsesSseParser {
  #buffer = '';

  push(chunk) {
    this.#buffer += chunk;
    const calls = [];
    for (;;) {
      const match = /\r?\n\r?\n/.exec(this.#buffer);
      if (!match) break;
      const block = this.#buffer.slice(0, match.index);
      this.#buffer = this.#buffer.slice(match.index + match[0].length);
      const data = block.split(/\r?\n/)
        .filter(line => line.startsWith('data:'))
        .map(line => line.slice(5).trimStart())
        .join('\n');
      if (!data || data === '[DONE]') continue;
      try {
        const event = JSON.parse(data);
        if (event.type !== 'response.output_item.done') continue;
        const item = event.item;
        if (!item || (item.type !== 'function_call' && item.type !== 'custom_tool_call')) continue;
        let args = item.arguments ?? item.input ?? {};
        if (typeof args === 'string') {
          try { args = JSON.parse(args); } catch { args = { input: args }; }
        }
        calls.push({ callId: item.call_id, providerCallId: item.call_id, name: item.name, args });
      } catch {}
    }
    return calls;
  }
}
