# Vellum onboarding sandbox checklist

Everything inside this Windows Sandbox is disposable. Closing it deletes the installed
apps, login state, DPAPI keys, Codex sessions, and Vellum data. Only files copied into
`C:\Exchange` come back to the host.

## Setup

1. Wait for the bootstrap window to report WebView2, the Vellum installer, and the
   ChatGPT/Codex Store package. `C:\Exchange\sandbox-*.log` has the full trace.
2. Vellum launches automatically at the end of bootstrap. If it did not, the resolved
   path is in `C:\Exchange\vellum-path.txt`.
3. Confirm this really is a first run: the palimpsest onboarding screen appears rather
   than the main window.

## Act 1 — what

1. Read the opening act in each of the four locales via the language switcher
   (en / ja / zh-CN / zh-TW).
2. Check the CJK typography rules hold: no fixed-width fields clipping Han characters,
   no `ch`-based sizing collapsing, no mixed-script font fallback in one line.
3. Resize the window down to the 940x620 minimum and confirm nothing overflows.

## Act 2 — connect

1. **chatgpt** — the Codex app is installed in this Sandbox, so the connection should
   detect it. Sign in with a test account. Verify the state chip moves
   not connected -> waiting for auth -> connected.
2. **grok** — leave it unconnected on the first pass, then connect it, and confirm the
   summary in Act 4 updates.
3. **custom** — enter a display name, endpoint, and a throwaway API key. Verify a bad
   endpoint surfaces an error instead of silently marking connected.
4. Confirm Codex config resolves under `C:\IsolatedHome\.codex`, not the Sandbox user
   profile.
5. Skip every connection and confirm you can still reach Act 4.

## Act 3 — features

1. Step through the feature act and confirm nothing depends on a connection made in
   Act 2 (it must render with zero providers linked).

## Act 4 — launch

1. Verify "connected providers" lists exactly what was linked, and reads as
   "not connected yet" when nothing was.
2. Verify the how-to-start / how-to-stop / reopen-guide rows are accurate.
3. Finish onboarding and confirm the app lands on the main screen.
4. Restart Vellum and confirm onboarding does **not** replay.

## Replaying onboarding without restarting the Sandbox

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File C:\SandboxKit\Reset-Onboarding.ps1
```

Add `-IncludeCodexHome` to also force a fresh Codex sign-in. The reset script refuses to
run outside Sandbox.

## Returning evidence to the host

Copy only non-secret evidence into `C:\Exchange`: screenshots, sanitized logs, the
onboarding state at each act, and exact reproduction steps.

Do not copy `auth.json`, access tokens, DPAPI blobs, provider keys, or raw conversation
databases back to the host.
