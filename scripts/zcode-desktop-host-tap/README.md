# ZCode Desktop host stdio tap

Host tap plus control pipe for Vellum. Desktop keeps CAPTCHA and credential
refresh. Control API: `hello`, `bind`, `turn/start`, `turn/cancel`, `status`.

```powershell
pwsh -File scripts/zcode-desktop-host-tap/launch-zcode-with-tap.ps1
node scripts/zcode-desktop-host-tap/inject-send.mjs --content "Reply with exactly PONG" --thread-id ui-1
```

Rust: `vellum-zcode-desktop` (`ZcodeDesktopHost`) and harness driver
`ZcodeDesktopDriver`. Details: [`docs/zcode-desktop-host-tap.md`](../../docs/zcode-desktop-host-tap.md).
