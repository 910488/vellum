# Stage-1 replay report

Re-run after namespace canonicalization. Full write-up: implementer scratch `replay-report.md`.

- Label: 案例衍生重現
- Fixture `cases.json`: `sha256:741c99d42b659eec1da5729326ce3f60b10fbcdeca4e1f8ec30ed8f99c0a9b6a`
- Grok: customInputPresent=true, formatFailures=4, falseBlocks=0, notices=0
- Omen: executed=25, 409=`tool_loop_limit` vs 502=`provider_protocol`, falseBlocks=0
- Flags off: no model-visible notice, no extra text continuation
- Scripted provider (`enhanced_agent_loop`): 6 passed — tools ran, no duplicate side effect, cancel starts no new request
- Scripted-provider pass ≠ live-model improvement
- macOS CI not observed; not marked pass
- Live Omen/Grok counts and cost remain undecided
