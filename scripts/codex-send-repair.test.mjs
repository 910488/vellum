import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import vm from "node:vm";
import { JSDOM } from "jsdom";

// Run against renderer bytes extracted from the installed app and the native
// Rust integration's verified copy, rather than an invented composer condition.
const originalPath = process.env.VELLUM_SEND_ORIGINAL_RENDERER;
const repairedPath = process.env.VELLUM_SEND_REPAIRED_RENDERER;
test("ordinary new-message Send remains clickable when login or Reserve quota is exhausted", {
  skip: !originalPath || !repairedPath,
}, () => {
  const original = fs.readFileSync(originalPath, "utf8");
  const repaired = fs.readFileSync(repairedPath, "utf8");
  function submitDisabled(source, exhausted, reserveBlocked, callerDisabled = false) {
    if (source.includes("Wn=Ae||St||rt&&it||vt||Ot||rn?.isLoading===!0")) {
      const gate = source.match(/fn=(?:X\(mP\)&&bt===`local`|\(X\(mP\),!1\))/)?.[0];
      const disable = source.match(/Wn=Ae\|\|St\|\|rt&&it\|\|vt\|\|Ot\|\|rn\?\.isLoading===!0(?:\|\|wt| {4})\|\|fn/)?.[0];
      assert(gate && disable, "audited macOS general-send gate must be found");
      let subscriptions = 0;
      const context = { X: () => { subscriptions++; return exhausted; }, mP: {}, bt: "local",
        Ae: callerDisabled, St: false, rt: false, it: false, vt: false, Ot: false, rn: null, wt: reserveBlocked };
      const disabled = vm.runInNewContext(`let ${gate}; let ${disable}; Wn`, context);
      assert.equal(subscriptions, 1, "macOS quota subscription must remain intact");
      return disabled;
    }
    const gate = source.match(/dn=(?:Y\(lP\)&&yt===`local`|\(Y\(lP\),!1\))/)?.[0];
    const disable = source.match(/Un=je\|\|xt\|\|nt&&rt\|\|_t\|\|Dt\|\|tn\?\.isLoading===!0(?:\|\|Ct| {4})\|\|dn/)?.[0];
    assert(gate && disable, "current native general-send gate must be found");
    let subscriptions = 0;
    const context = { Y: () => { subscriptions++; return exhausted; }, lP: {}, yt: "local",
      je: callerDisabled, xt: false, nt: false, rt: false, _t: false, Dt: false, tn: null, Ct: reserveBlocked };
    const disabled = vm.runInNewContext(`let ${gate}; let ${disable}; Un`, context);
    assert.equal(subscriptions, 1, "quota subscription must remain intact");
    return disabled;
  }
  for (const [exhausted, reserveBlocked] of [[true, false], [false, true], [true, true]]) {
    assert.equal(submitDisabled(original, exhausted, reserveBlocked), true);
    const dom = new JSDOM('<input aria-label="New message"><button>Send</button>');
    const { document } = dom.window;
    const input = document.querySelector("input");
    input.value = "Please continue with the selected Vellum account";
    const sent = [];
    const button = document.querySelector("button");
    button.disabled = submitDisabled(repaired, exhausted, reserveBlocked);
    button.addEventListener("click", () => sent.push(input.value));
    button.click();
    assert.deepEqual(sent, [input.value], "an idle composer sends a new message without any running turn");
    dom.window.close();
  }
  assert.equal(submitDisabled(repaired, true, true, true), true);
});
