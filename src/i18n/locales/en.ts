import type { ResourceTree } from "../resources";

const en: ResourceTree = {
  "enhanced": {"heading":"Enhanced runtime status","lead":"Shows the execution owner for each route, the result of the compatibility check, and the mobile connection status.","title":"Enhanced Core","subtitle":"Execution owners, compatibility evidence and mobile connections","serving":"Enhanced Core is serving","notServing":"Enhanced Core is not serving","sessions":"Observed sessions","running":"Running turns","freshness":"Observation","restartRequired":"A new launch is prepared. Restart safely after active turns finish.","launchCoreDrift":"Codex Desktop updated its core. Vellum will restart the Proxy when turns are idle; restart Codex afterwards.","now": "Right now", "runningTurns_one": "{{count}} turn running", "runningTurns_other": "{{count}} turns running", "sessionCount_one": "{{count}} session", "sessionCount_other": "{{count}} sessions", "verdict": {"serving": "Codex Desktop is going through the Enhanced Core.", "servingOtherLaunch": "Enhanced is serving, but on the previous launch's configuration — restart Codex to move it onto this one.", "stoppedAt": "Startup stopped at the “{{link}}” stage.", "disabled": "Enhanced Core is off; Codex Desktop is running its own core."}, "chain": {"title": "Startup sequence", "artifact": "Component verification", "armed": "Path takeover", "adopted": "Bridge adoption", "inPlace": "Runtime confirmed"}, "mark": {"blocked": "Failed", "waiting": "Waiting", "inPlace": "Active"}, "whyStopped": "Reason the startup stopped at “{{link}}”", "whyNoted": "Running with items to note", "protocol": {"verified": "Verified", "unverified": "Unverified", "incompatible": "Incompatible"}, "protocolUnrun": "The comparison did not run", "unverifiedNote": "This Codex Desktop was never qualified against. Every routed method passes; the differences are real and unproven — usable, correctness not guaranteed.", "missingHelpers": "Helpers the official install has and the Enhanced core does not: {{list}}. Chat and routing are unaffected; sandboxed command execution fails.", "routed": "routed", "delta": {"methodUnservable": "this method cannot be served", "methodUnknownToDesktop": "Desktop does not know this method", "fieldNewlyRequired": "a field became required", "fieldRemoved": "a field was removed"}, "environment": {"leased": "Held", "released": "Handed back", "orphanedBridge": "Orphaned bridge", "foreignValue": "Held by something else", "unreadable": "Unreadable"}, "bridgeProcess": "Bridge process", "bridgeState": "Bridge state", "children": "Children", "launch": "Launch", "relayNote": "A phone connects only after handshake, task list, message load, live events and task control all complete; whichever one did not is the reason it cannot connect.", "staleWarning":"The last observation may be out of date.","compatibility":"Versions and compatibility","activeRuntime":"Serving runtime","candidateRuntime":"Next launch runtime","structure":"Schema comparison","qualification":"Latest qualification","notObserved":"Not observed","observed":"Observed","evidenceNote":"Schema equality is structural evidence. It does not certify Desktop or mobile behavior.","differences":"Differences and blockers","relay":"Relay state","clients":"Connected client versions","handshake":"Handshake","list":"Task list","history":"Message loading","stream":"Live events","control":"Task control","failureStage":"Last failure stage","plane":"Execution owner","all":"All","empty":"No sessions observed in this launch. Historical bindings are not live connections.","session":"Session","state":"State","lastActivity":"Last activity","parent":"Parent","diagnostics":"Diagnostics","recheck":"Recheck compatibility","export":"Export diagnostics","exported":"Saved","states":{"starting":"Starting", "ready":"Ready", "degraded":"Degraded", "stopped":"Stopped", "transportReady":"Transport ready", "unknown":"Unknown","current":"Current","stale":"Stale","offline":"Offline","unavailable":"Unavailable","running":"Running","idle":"Idle","approval":"Awaiting approval","unloaded":"Unloaded","observed":"Observed","attached":"Attached","failed":"Failed"}},
  "common": { "expiresAt": "Expires {{month}}/{{day}} {{time}} ({{zone}})", "expiredAlready": "Expired", "expiresInMinutes": "{{value}} min left", "expiresInHours": "{{value}} h left", "expiresInDays": "{{value}} d left","emDash": "—", "none": "—", "listSeparator": "; ", "itemSeparator": ", ", "loading": "Loading…", "processing": "Processing…", "refreshing": "Refreshing…", "refresh": "Refresh", "reorganize": "Refresh", "save": "Save", "cancel": "Cancel", "confirm": "Confirm", "close": "Close", "remove": "Remove", "enabled": "Enabled", "disabled": "Disabled", "yes": "Yes", "no": "No", "on": "On", "off": "Off", "unknown": "Unknown", "provider": "Provider", "model": "Model", "proxy": "Proxy", "codexCatalog": "Codex model catalog", "justNow": "Just now", "minutesAgo": "{{count}} min ago", "hoursAgo": "{{count}} h ago", "daysAgo": "{{count}} d ago", "resetAt": "Resets {{month}}/{{day}} {{time}}", "previousPage": "Previous page", "nextPage": "Next page", "pageOf": "Page {{page}} / {{total}}", "shortcutTitle": "{{label}} (Ctrl+{{n}})", "statusUpdateFailed": "Status update failed: {{detail}}", "errorWithDetail": "{{message}}: {{detail}}", "connectionFailed": "Connection failed", "operationFailed": "Operation failed", "loadFailed": "Failed to load", "saveFailed": "Failed to save", "visible": "Visible", "hidden": "Hidden", "totalItems_one": "{{count}} item", "totalItems_other": "{{count}} items", "itemsNeedAttention_one": "item needs attention", "itemsNeedAttention_other": "items need attention", "notice": {"info": "Note", "warn": "Heads up", "error": "Failed"}, "dismiss": "Dismiss"},
  "navigation": {
    "mainAria": "Main navigation",
    "today": {
      "label": "Today",
      "title": "Today",
      "blurb": "Proxy, Provider, and quota status"
    },
    "models": {
      "label": "Models",
      "title": "Models",
      "blurb": "Providers and Codex model catalog"
    },
    "context": {
      "label": "Context",
      "title": "Context",
      "blurb": "Context usage, compaction preview and transcript"
    },
    "enhanced": {
      "label": "Enhanced",
      "title": "Enhanced",
      "blurb": "Execution owner and compatibility status"
    },
    "remote": {
      "label": "Remote",
      "title": "Remote Manager",
      "blurb": "Codex App SSH hosts and native daemon"
    },
    "log": {
      "label": "Log",
      "title": "Log",
      "blurb": "Token stats and request log"
    },
    "settings": {
      "label": "Settings",
      "title": "Settings",
      "blurb": "App and Codex integration settings"
    }
  },
  "status": {
    "live": "Live",
    "pending": "Pending",
    "off": "Off",
    "reading": "Loading…",
    "enhancedLoaded": "Loaded",
    "enhancedNotLoaded": "Not loaded",
    "enhancedUnverified": "Loaded, unverified",
    "proxyStopped": "Proxy is stopped",
    "codexNotManaged": "Codex is not pointed at Vellum",
    "lastSuccessfulRequest": "Last successful request",
    "defaultRoute": "Default route",
    "noProviderYet": "No provider yet",
    "quotaRemaining": "Quota left {{percent}}%",
    "restartCodexRequired": "Codex restart required",
    "updateAvailable": "Update available",
    "updateWaitingIdle": "Update downloaded, waiting for idle",
    "updateWaitingRestart": "Update downloaded, apply on next start",
    "updateFailed": "Update failed or rolled back",
    "remedy": {
      "startProxy": "Takes effect after starting Proxy",
      "restartProxy": "Takes effect after restarting Proxy",
      "restartCodex": "Appears in the model catalog after restarting Codex"
    }
  },
  "vocabulary": {
    "quotaUnavailable": "This endpoint does not provide quota lookup",
    "source": {
      "override": "Your manual value",
      "modelCache": "Provider model cache",
      "catalog": "Codex model catalog",
      "fallback": "Fallback value"
    }
  },
  "quota": {
    "period": {
      "week": "Weekly quota",
      "month": "Monthly quota",
      "hours_one": "{{count}}-hour quota",
      "hours_other": "{{count}}-hour quota",
      "days_one": "{{count}}-day quota",
      "days_other": "{{count}}-day quota",
      "unspecified": "Usage quota"
    },
    "remainingLabel": "{{period}} remaining {{remaining}}%"
  },
  "review": {
    "policy": {
      "always": {
        "label": "Fixed model",
        "blurb": "Always use the selected model. Auto review stops when that Provider quota is exhausted."
      },
      "failover": {
        "label": "Prefer selected, fail over if needed",
        "blurb": "Use the selected model first, then fall back if quota or connectivity fails so review continues."
      }
    }
  },
  "heatmap": {
    "tokens": "{{value}} tokens",
    "gridAria": "Token usage heatmap · {{mode}}",
    "mode": {
      "daily": "Daily",
      "weekly": "Weekly",
      "cumulative": "Cumulative"
    },
    "monthLabel": "{{month}}",
    "aria": "Usage heatmap",
    "tooltip": "{{from}} – {{to}} · {{requests}} requests · {{tokens}} tokens",
    "range": "{{from}} to {{to}}",
    "requestsTokens_one": "{{count}} request · {{tokens}} tokens",
    "requestsTokens_other": "{{count}} requests · {{tokens}} tokens"
  },
  "runtime": {"notice": {"codexConfigStillPointedAtVellum": "Codex is still pointed at Vellum from the last session. Start the Proxy, then restart the Codex App to carry on.", "codexRunning": "Codex is running; restart it to load the Proxy and the model catalog", "proxyStoppedCodexRestartRequired": "The Proxy stopped and restored its settings; restart Codex to leave the old Proxy/Bridge connection", "codexRestartDetected": "Codex restart detected; the Proxy and model catalog are loaded", "routesAndCatalogUpdated": "Provider settings and the model catalog changed; restart the Vellum Proxy and Codex to apply them", "catalogRestored": "The model catalog was rolled back; Codex needs a restart", "catalogUpdated": "The model catalog was updated", "enhancedDesktopRuntimeChanged": "Enhanced Codex Runtime settings changed; restart Codex to apply them", "enhancedLaunchCoreRepaired": "Codex Desktop updated its core. Vellum restarted the Proxy and prepared the new launch; restart Codex (previous launch {{launchId}}).", "enhancedLaunchCoreRepairFailed": "Vellum could not restart the Proxy after Codex Desktop updated: {{reason}}. Restart the Proxy, then restart Codex.", "remoteOfficialAccountSwitchNotFollowed": "The remote host {{host}} did not follow the account switch. Open Remote Manager to pair or align it. Reason: {{detail}}", "officialAccountSwitchNativePlane": "Official GPT is on the unmodified Codex core; Vellum account selection does not replace the Codex Desktop login while the Enhanced bridge is live", "enhancedDesktopBridgeReady": "Codex restarted and is running through the Enhanced bridge (launch {{launchId}})", "enhancedDesktopBridgeFailed": "Codex restarted, but the Enhanced bridge failed: {{reason}}", "enhancedDesktopRuntimeNotArmed": "The Proxy started, but the Enhanced Codex runtime was not armed: third-party threads run on native Codex. Reason: {{detail}}", "enhancedDesktopBridgeNotObserved": "Codex was relaunched, but no Enhanced bridge reported for launch {{launchId}}; Enhanced is not running", "routeHotSwapped": "The route was hot-swapped", "slotSwitched": "{{slot}} switched to {{model}}", "restartSucceeded": "Codex restarted safely; the new runtime settings are loaded", "restartNotDetected": "The launch request was sent, but Codex was not detected within 5 seconds; open it manually", "restartProcessStillRunning": "Codex has not fully exited (stop exit {{exitCode}}); the restart was cancelled", "restartBlockedByActiveRequests_one": "{{count}} request is still running", "restartBlockedByActiveRequests_other": "{{count}} requests are still running", "restartBlockedByCodexTurn_one": "Codex Desktop has {{count}} conversation still running; restarting now loses whatever it has not written to disk yet", "restartBlockedByCodexTurn_other": "Codex Desktop has {{count}} conversations still running; restarting now loses whatever they have not written to disk yet", "restartExecutableMissing": "No verifiable Codex App executable was found", "restartExecutableInvalid": "The Codex App executable is missing or is not an absolute path", "restartUnavailableInPreview": "The browser preview does not restart Codex", "usageOAuthNotConfigured": "OpenAI OAuth is not configured in Vellum; OpenAI totals are estimated from the Proxy log", "usageProfileUnavailable_one": "Codex token stats for {{count}} OpenAI OAuth account are temporarily unavailable", "usageProfileUnavailable_other": "Codex token stats for {{count}} OpenAI OAuth accounts are temporarily unavailable", "webSearchDisabledMissingBraveKey": "Third-party web search was turned off because no Brave Search API key is configured. Add a key in Settings to turn it back on."}, "alerts": {"label": "Runtime alerts"}},
  "settings": {
    "language": {
      "title": "Language",
      "blurb": "Choose the UI language. Follow system tracks the OS language.",
      "option": {
        "system": "Follow system",
        "zh-TW": "繁體中文",
        "zh-CN": "简体中文",
        "en": "English",
        "ja": "日本語"
      },
      "applied": "Language applied"
    },
    "title": "Settings",
    "page": {
      "subagent": { "title": "Sub-agent defaults", "description": "By default a sub-agent runs on the model and reasoning effort of the conversation that spawned it. Pick a fixed default here instead; an explicit model or effort on the task still takes priority.", "desktopReady": "Codex Desktop is ready", "desktopUnavailable": "Codex Desktop cannot apply this setting", "desktopVersion": "Desktop version {{version}} · native sub-agent defaults available", "defaultsTitle": "Model used when unspecified", "mode": { "inherit": "Keep Desktop settings", "custom": "Set default with Vellum" }, "effort": "Reasoning effort", "autoEffort": "Automatic (model default)", "effortNotProbed": "This model's reasoning efforts have not been verified; probe its capabilities from the Models page first.", "modelEmpty": "This Provider has no catalog models to pick from.", "hint": "These are only defaults: an agent may explicitly name another enabled model and effort for a task.", "unavailable": "The selected Provider or model is no longer available. Sub-agent spawns that rely on this default will fail explicitly until the selection is updated.", "unsupported": "Could not verify native sub-agent settings in Codex Desktop: {{detail}}" },
      "loading": "Loading settings…", "heading": "Review, search, display, and recovery", "unspecified": "Not specified", "saved": "Saved",
      "review": { "title": "Auto review", "toggle": "Review operations that require approval", "description": "Operations that Codex would normally ask you to approve are assessed by the selected model. This does not change sandbox, network, or file permissions; it changes only who evaluates the approval decision.", "billingHelp": "How is quota charged?", "billingHelpTitle": "Auto review and ChatGPT quota", "billingHelpFacts": { "enabled": { "title": "Auto review on", "body": "Official reviews use the billing account selected here. They follow normal ChatGPT account routing only when Follow the account in use is selected." }, "disabled": { "title": "Auto review off", "body": "Vellum no longer names a review-specific account. Official requests use normal ChatGPT account routing and usually count against the account selected on the Models page." }, "pool": { "title": "Quota pool exception", "body": "When the quota pool is enabled and has members, it selects the account for ordinary Official requests, so usage may not count against the manually selected account." }, "native": { "title": "No Vellum-managed account", "body": "If Vellum manages no ChatGPT account, requests preserve the native Codex login. Third-party Providers use their own quota instead." } }, "strategyTitle": "Model selection strategy", "currentLabel": "Currently used", "fallbackActive": "Fallback active", "selectedUnavailable": "Selected model unavailable", "provider": "Selected Provider", "model": "Selected model", "fallbackModel": "Fallback model", "billingAccount": "Billed to", "billingFollowsDefault": "Follow the account in use", "billingAccountMissing": "Account {{account}} is not signed in on this machine. Auto review will fail rather than bill a different account — pick one from the list, or sign that account back in.", "savedRemotePending": "Saved. Applied to the local proxy now; {{count}} remote host(s) need reapply in Remote Manager.", "rulesTitle": "Auto review rules", "willReview": "Reviewed", "willReviewValue": "Operations leaving the sandbox, using restricted network access, or touching protected paths", "willNotReview": "Not reviewed", "willNotReviewValue": "Operations already allowed inside the sandbox", "whenRejected": "When rejected", "whenRejectedValue": "Codex must choose a safer approach instead of executing directly" },
      "stats": { "title": "Review runs by Provider", "summary": "Fallback {{fallback}} / {{total}} runs ({{percent}}%)", "primary": "Selected", "fallback": "Fallback", "failed": "Failed", "lastUsed": "Last used", "neverUsed": "Never used", "empty": "No auto review has run yet. The next approval request will record which Provider answered." },
      "webSearch": { "title": "Web search", "toggle": "Enable web search for third-party Providers", "description": "When enabled, non-official Providers can use the Codex web search tool and queries are answered by Brave Search. When disabled, Vellum removes the tool before forwarding each request. OpenAI official Providers continue to use native search and are unaffected by this setting.", "braveKey": "Brave Search API key", "braveKeySaved": "Configured; enter a new key to update it", "braveKeyPlaceholder": "Enter a Brave Search API key", "braveKeyHint": "The API key is stored locally in encrypted form, is not written to the general settings file, and is never provided to the model.", "reach": "Web access scope", "reachOption": { "indexed": "Indexed results only", "live": "Allow public page retrieval" }, "reachHint": { "indexed": "The model receives only the titles and snippets returned by the search backend; source pages are not retrieved.", "live": "The model may retrieve public HTTP(S) pages from search results. Localhost, private-network, and cloud-metadata addresses are blocked." }, "probe": "Search connection test", "probeAction": "Run test", "probing": "Testing…", "probeHint": "Sends a minimal query to verify Brave Search connectivity and response status.", "probeOk": "Brave Search returned {{count}} results", "probeEmpty": "Brave Search is reachable but returned no search results", "probeFailed": "Connection test failed: {{detail}}", "probedAt": "tested {{when}}", "needsBraveKey": "Web search is enabled but no Brave Search API key is configured; queries will fail until you add one.", "domainTray": "Domain access rules", "domainAllow": "Allow list", "domainBlock": "Block list", "domainHint": "Enter one domain per line. Leave both lists empty to allow all domains. When an allow list is configured, only matching domains are retained in search results." },
      "dashboard": { "title": "Dashboard Provider display", "note": "{{count}} selected. This affects only the Today page and does not enable or disable Providers.", "disabledVisible": "Show (disabled)" },
      "restore": { "title": "Restore original Codex settings", "description": "Remove Vellum's proxy URL, boundary key, and model catalog entries, and restore the original default Provider. A proxy-free Provider that connects directly to OpenAI remains so existing Vellum chats can still open in native Codex. Login credentials, chats, project groups, and workspace data are untouched.", "action": "Restore original Codex settings", "cleared": "Restored", "result": "Result", "nothingToClear": "Codex has no Vellum-written settings to restore", "preserved": "Unchanged: {{items}}." },
      "enhancedRuntime": {"title": "Enhanced Codex Runtime", "description": "New conversations on third-party Providers run on Enhanced Codex; OpenAI's own models stay on the native Codex Runtime. Changing execution plane requires a new conversation.", "desiredState": "Requested state", "desiredEnabled": "Enable requested", "desiredDisabled": "Disable requested", "activation": {"disabled": "Off", "artifactBlocked": "Executable verification failed", "awaitingDesktopRestart": "Waiting for a Codex Desktop restart", "active": "Live", "disablePendingRestart": "Disable requested, restart pending", "environmentDrift": "Handover state is inconsistent", "failed": "Bridge failed to start"}, "environment": {"leased": "Held by Vellum under a lease", "released": "Not taken over", "orphanedBridge": "An older Vellum bridge was left behind", "foreignValue": "Set by another tool; Vellum did not overwrite it", "unreadable": "Cannot read the per-user environment"}, "environmentValue": "Current CODEX_CLI_PATH", "environmentUnset": "Not set", "staleBridge": "This path is a bridge left behind by an older Vellum, not the one this build ships. Press Disable and release to clear it, then enable again.", "launchDetail": "{{launchId}} · {{state}} · bridge {{bridgePid}} · Official {{officialPid}} · Enhanced {{enhancedPid}}", "officialBinary": "Official Codex core", "officialBinaryHint": "Detected from the installed Codex Desktop. It has to be the executable Desktop actually runs, so it is not editable.", "enhancedBinary": "Enhanced Codex core", "bridgeBinary": "App Server bridge", "bridgeBinaryHint": "Ships with this Vellum build with its hash compiled in; it cannot be substituted.", "protocol": {"label": "Protocol check", "unavailable": "Could not be checked", "details": "Differences and diagnostics ({{count}})", "routed": "routed", "verdict": {"verified": "Matches the pinned release", "unverified": "Usable, correctness not guaranteed", "incompatible": "Incompatible, fell back"}, "mean": {"verified": "This is the pairing this Vellum release pinned; the two protocols are byte-identical.", "unverified": "Nobody has qualified this Codex Desktop version. Every method the bridge demultiplexes on is still there, so routing is sound; the differences below are real and untested.", "incompatible": "Codex Desktop changed a method the bridge has to demultiplex on. There is no degraded mode to fall back to."}, "delta": {"shapeIncompatible": "{{subject}} carries a wire shape the other side does not accept: {{fields}}", "shapeUnverified": "{{subject}} carries a wire shape this check cannot verify either way: {{fields}}", "methodUnservable": "Desktop may call {{subject}}, which the Enhanced core does not implement", "methodUnknownToDesktop": "The Enhanced core may send {{subject}}, which Desktop no longer declares", "fieldNewlyRequired": "Desktop now requires {{fields}} on {{subject}}, which the Enhanced core does not emit", "fieldRemoved": "Desktop no longer sends {{fields}} on {{subject}}, which the Enhanced core reads"}, "fallback": "Back on native Codex. Codex itself works exactly as it should; none of the Enhanced behaviour is present."}, "protocolHash": "App Server protocol", "observedBridge": "Bridge process actually running", "unverified": "Not verified", "details": "Technical details and diagnostics", "runInstalledGate": "Re-run the installed-build qualification", "installedGateHint": "A deeper check than one restart: restarts Codex Desktop, runs the full installed gate, and leaves a report.", "lastQualification": "Last qualification", "qualificationPassed": "{{mode}} passed and is ready to promote", "qualificationComponentOnly": "{{mode}} passed its component tests but not the installed promotion bar", "qualificationFailed": "{{mode}} failed — {{detail}}", "qualificationReport": "Report", "planeTitle": "Which plane each route runs on now", "contextNote": "Compaction belongs to whichever runtime executes the conversation: the Official path uses Official Codex's native compaction, the third-party path uses Enhanced Codex's local compaction and context recovery. Vellum applies no compaction policy of its own.", "planeOfficial": "Official Codex · native compaction", "planeEnhanced": "Enhanced Codex · local compaction and context recovery", "planeUnbound": "Not bound to Enhanced Codex", "activationLabel": "Activation", "environmentLabel": "Handover", "inject": {"on": "Injected", "off": "Not injected", "working": "Injecting", "switchLabel": "Inject the Enhanced Codex Runtime", "mean": {"staleLaunch": "Codex Desktop is going through Enhanced right now, but on an earlier launch’s configuration. Restart Codex Desktop for the current one to take effect.", "on": "Codex Desktop is running on Vellum's bridge. New threads on third-party providers run on Enhanced Codex; OpenAI's own models stay on native Codex.", "off": "Codex Desktop is running on native Codex. Vellum has not touched CODEX_CLI_PATH.", "wanted": "Vellum's side is set up, but Codex Desktop has not adopted it, so what is running is still native Codex.", "working": "Verifying the executable, taking over CODEX_CLI_PATH, and restarting Codex Desktop.", "armed": "Takes effect the next time Codex Desktop starts."}, "blocked": {"proxyStopped": "The Proxy is not running. Vellum does not take over the launch path while it is stopped — Codex Desktop would otherwise start on a bridge with no data plane behind it. Start the Proxy first.", "stuck": "Injection was requested, but the launch path was never taken over. The terminal below says where it stopped; throwing the switch again retries.", "noCore": "This Vellum build does not contain the Enhanced Codex core. That is not a setting you skipped — the core ships with the build, so a missing one means an incomplete install. Reinstall Vellum."}}, "term": {"idle": "Nothing has run yet. Throw the switch above and every step is printed here.", "verify": "Check the Enhanced Codex executable", "verifyOk": "The file's contents are the ones this version expects", "verifyFailed": "The check did not pass; nothing was changed", "restart": "Take over CODEX_CLI_PATH and restart Codex Desktop", "restartBack": "Restart Codex Desktop back onto the native runtime", "restartRefused": "Did not restart", "adopt": "Confirm Codex Desktop adopted the bridge", "injected": "Injected · {{profile}}", "notInjected": "Codex Desktop did not adopt the bridge", "release": "Release CODEX_CLI_PATH", "releasedOk": "Released. Codex Desktop is back on the native runtime", "envUnset": "(unset)", "preflight": "Check the preconditions", "proxyStopped": "The Proxy is not running, so the takeover would be released. Start the Proxy, then inject.", "armed": "The launch path is taken over. It takes effect the next time Codex Desktop starts.", "alreadyInjected": "Codex Desktop is already on this bridge; no restart needed · {{profile}}", "missingHelper": "Helper {{name}} is missing"}, "missingHelpers": "Helper executables the official Codex install has are missing beside this Enhanced core: {{names}}. Codex looks for them beside itself at run time, and a miss becomes a Windows “cannot find” dialog. Threads and routing are unaffected; only the capability that uses each helper breaks — sandboxed shell commands need codex-windows-sandbox-setup.exe and codex-command-runner.exe, and code mode needs codex-code-mode-host.exe, which is off by default and costs nothing while it stays off.", "enhancedBinaryHint": "Ships with this Vellum build and is matched byte for byte against the hash in enhanced-runtime.lock.json. Any other file could only fail verification, so it is not offered as a choice.", "coreMissing": "not in this build"},
      "updates": { "title": "Software updates", "description": "Desktop, Remote, and Enhanced core update independently. Downloaded updates stay staged until their apply condition is met.", "liveDisabled": "Automatic updates stay off until release signing is configured.", "actionFailed": "Update action failed: {{error}}", "autoCheck": "Check for updates automatically", "autoDownload": "Download updates automatically", "channel": "Channel", "stable": "Stable", "preview": "Preview", "current": "Current version", "available": "Available version", "none": "None", "progress": "Download progress", "applyWhen": "Apply condition", "notes": "Release notes", "failure": "Failure reason", "check": "Check for updates", "download": "Download", "apply": "Apply", "cancel": "Cancel download", "rollback": "Roll back", "hosts": "Hosts", "idleAuto": "Update automatically when idle", "idleHandoff": "Preview: apply Enhanced core while idle (off by default)", "layerDisabled": "Updates for this layer are not enabled in this build.", "layer": { "desktop": "Vellum", "remote": "Remote package", "core": "Enhanced Codex core" }, "condition": { "restartVellum": "Ready. Restart Vellum to apply.", "hostIdle": "Downloaded. Waiting until the host is idle.", "nextCoreStart": "Downloaded. Will apply on the next core start.", "download": "Download to stage.", "failed": "See the failure reason.", "idle": "Up to date.", "checking": "Checking…", "available": "An update is available.", "downloading": "Downloading…", "verifying": "Verifying…", "staged": "Staged.", "waitingForIdle": "Waiting for idle.", "waitingForRestart": "Waiting for restart.", "applying": "Applying…", "validating": "Validating…", "applied": "Applied.", "blocked": "Blocked.", "rolledBack": "Rolled back." }, "phase": { "idle": "Idle", "checking": "Checking", "available": "Available", "downloading": "Downloading", "verifying": "Verifying", "staged": "Staged", "waitingForIdle": "Waiting for idle", "waitingForRestart": "Apply on next start", "applying": "Applying", "validating": "Validating", "applied": "Applied", "blocked": "Blocked", "failed": "Failed", "rolledBack": "Rolled back" } },
      "advanced": { "title": "Advanced settings", "description": "Request draining, safe Codex restart, and model catalog rollback for troubleshooting and maintenance only.", "activeRequests": "Active requests", "drainTitle": "Stop accepting new requests", "draining": "New requests stopped; waiting for existing requests to finish", "accepting": "Accepting requests normally", "drainHint": "Existing requests are not interrupted. While enabled, Vellum rejects new requests until you resume it manually.", "resume": "Resume accepting requests", "stop": "Stop accepting new requests", "catalogVersion": "Current catalog version", "notCreated": "Not created", "restartTitle": "Restart Codex", "restartHint": "Safely finish existing requests, then restart Codex.", "restartAction": "Restart Codex", "restartAnyway": "Restart anyway", "guideTitle": "Setup guide", "guideHint": "Review the first-run guide or connect another Provider without resetting existing settings.", "guideAction": "Open setup guide", "catalogHistory": "Model catalog history", "rollback": "Rollback", "noVersions": "No rollback versions yet. Each model catalog update creates one.", "restartRequired": "Effective after restart", "applied": "Applied" },
      "logs": { "title": "Diagnostic logs", "description": "Export the retained Vellum, Proxy, and Enhanced Runtime text logs as a ZIP. Keys, tokens, and user paths are redacted again; credentials, settings, chat history, and databases are excluded.", "export": "Export all logs as ZIP", "exporting": "Exporting…", "exported": "Exported to {{path}}", "hint": "The ZIP is saved to Downloads and includes a file manifest and truncation record." },
      "app": { "title": "Application controls", "exit": "Exit Vellum", "exitHint": "Exiting stops the Proxy and restores the original Codex connection settings; chats and projects are not affected." },
      "errors": { "partialRefresh": "Some data failed to refresh: {{detail}}", "reviewSave": "Could not save auto review settings: {{detail}}", "reviewNoModelAvailable": "No enabled review model is configured. Add or enable a Provider before switching to a fixed model.", "reviewNoFallbackAvailable": "Failover needs a second enabled review model on a different Provider. Add or enable one first.", "restore": "Restore failed: {{detail}}", "drain": "Could not update request acceptance: {{detail}}", "restart": "Could not restart Codex: {{detail}}", "rollback": "Could not roll back the model catalog: {{detail}}", "webSearchSave": "Could not save web search settings: {{detail}}", "subagentSave": "Could not save sub-agent settings: {{detail}}", "logExport": "Could not export diagnostic logs: {{detail}}" }
    }
  },
  "today": {
    "title": "Today",
    "loading": "Loading status…",
    "noProvider": "No provider yet",
    "noSuccessfulRequest": "No successful requests yet",
    "addProvider": "Add provider",
    "providerTokens": "Current Provider token total",
    "requests_one": "{{count}} request",
    "requests_other": "{{count}} requests",
    "lastActivity": "Last activity",
    "proxy": {
      "start": "Start Proxy",
      "stopRestore": "Stop Proxy and restore Codex"
    },
    "quota": {
      "tightest": "Provider with the least weekly quota remaining",
      "remaining": "Quota remaining",
      "noData": "No Provider has reported quota yet"
    },
    "context": {
      "nearestThreshold": "Session closest to the compaction threshold",
      "usage": "Context usage",
      "threshold": "Compaction threshold {{percent}}%",
      "compactThreshold": "Compaction threshold",
      "averageTurn": "Average per turn",
      "atRate": "At this rate",
      "trend": "Context usage trend"
    },
    "findings": {
      "title": "Items requiring attention",
      "count_one": "{{count}} item",
      "count_other": "{{count}} items",
      "adjustContext": "Adjust context window"
    },
    "sessions": {
      "title": "Sessions",
      "count": "{{live}} active / {{total}} total",
      "rest": "Older sessions"
    },
    "providers": {
      "title": "Provider status",
      "noModels": "No available models",
      "quotaFailed": "Quota lookup failed",
      "notQueried": "Not checked yet",
      "lastChecked": "checked {{since}}",
      "refreshTitle": "Refresh quota for {{provider}}",
      "querying": "Checking…",
      "refresh": "Refresh",
      "empty": "No Providers are shown. Select them in Settings or add one in Models."
    },
    "health": {
      "title": "Connection quality",
      "connectionReuse": "Connection reuse",
      "reusing_one": "Reusing · {{count}} connection",
      "reusing_other": "Reusing · {{count}} connections",
      "notReusing": "Not reused",
      "firstByte": "Time to first token",
      "reasoning": "Reasoning",
      "history": "History retention",
      "historyValue_one": "{{days}} day · encrypted",
      "historyValue_other": "{{days}} days · encrypted"
    },
    "headroom": {
      "exceeded": "Threshold exceeded; compaction will trigger on the next turn",
      "noEstimate": "Not enough turns to estimate",
      "estimate_one": "Estimated {{count}} turn remaining",
      "estimate_other": "Estimated {{count}} turns remaining"
    },
    "errors": {
      "partialRefresh": "Some data failed to refresh: {{detail}}",
      "quotaRefresh": "Quota lookup failed: {{detail}}",
      "proxyOperation": "Proxy operation failed: {{detail}}"
    }
  },
  "models": {
    "title": "Models",
    "ui": {
      "catalogTitle": "Configure the Codex model catalog", "catalogHint": "Connect an account or a provider, then choose which of its models Codex offers.", "wireUnknown": "Cannot determine automatically; choose one", "customProvider": "Custom provider", "errors": { "partialRefresh": "Some data failed to refresh: {{detail}}", "noReset": "No Reset credits are available.", "confirmReset": "Use one Reset credit for {{account}}? This consumes one credit and cannot be undone.", "resetSuccess": "OpenAI credits were reset.", "resetCompleted": "Reset completed ({{code}}).", "resetFailed": "Reset failed: {{detail}}", "resetLookupFailed": "Reset lookup failed: {{detail}}", "oauthExpired": "The authorization code expired. Sign in again.", "oauthLoginFailed": "ChatGPT sign-in failed: {{detail}}", "oauthSwitchFailed": "Could not switch ChatGPT account: {{detail}}", "oauthRemoveFailed": "Could not remove ChatGPT account: {{detail}}", "oauthRefreshFailed": "Could not refresh ChatGPT sign-in: {{detail}}", "oauthLogoutFailed": "Could not sign out of ChatGPT: {{detail}}", "probeFailed": "Could not probe this endpoint: {{detail}}", "opencodeApiKeyRequired": "Enter an OpenCode Zen API key.", "opencodeConnectFailed": "Could not connect OpenCode Zen: {{detail}}", "opencodeFreeAttachFailed": "OpenCode Go connected, but attaching its free models failed: {{detail}}", "modelProbeFailed": "Could not verify {{model}}: {{detail}}", "modelToolProbeFailed": "{{model}} did not return a structured Codex tool call.", "routeRefreshFailed": "Could not refresh Provider status: {{detail}}", "reprobeFailed": "Capability probe failed: {{detail}}", "routeRemoveFailed": "Could not remove Provider: {{detail}}", "providerModelRequired": "Each Provider must keep at least one model.", "catalogRefreshFailed": "Could not update the Codex model catalog: {{detail}}", "wireRequired": "The API protocol could not be detected automatically. Choose Responses API or Chat Completions.", "modelRequired": "No model was detected. Enter a model name manually.", "routeAddFailed": "Could not add Provider: {{detail}}", "grokLoginFailed": "Grok sign-in failed: {{detail}}", "grokStartFailed": "Could not start Grok sign-in: {{detail}}", "grokCancelFailed": "Could not cancel Grok sign-in: {{detail}}", "grokSwitchFailed": "Could not switch Grok account: {{detail}}", "grokRefreshFailed": "Could not refresh Grok account: {{detail}}", "grokRemoveFailed": "Could not remove Grok account: {{detail}}", "detectAttention": "Needs input", "detectFact": "Detected" },
      "confirm": {
        "removeAccount": { "title": "Remove ChatGPT account", "confirmLabel": "Remove account", "factAccount": "Account", "factEffect": "Effect", "effectValue": "Vellum forgets this account's tokens; sign in again to use it here." },
        "logoutAll": { "title": "Sign out of all ChatGPT accounts", "confirmLabel": "Sign out all", "factAccount": "Accounts", "factEffect": "Effect", "accountsValue": "All connected ChatGPT accounts", "effectValue": "Every account is removed from Vellum; each one needs to sign in again." },
        "removeGrokAccount": { "title": "Remove Grok account", "confirmLabel": "Remove account", "factAccount": "Account", "factEffect": "Effect", "effectValue": "Vellum forgets this account's tokens; sign in again to use it here." },
        "removeRoute": { "title": "Remove Provider", "confirmLabel": "Remove Provider", "factProvider": "Provider", "factEffect": "Effect", "effectValue": "This Provider and its whole model catalog are removed; add it again from scratch to bring it back." },
        "consumeReset": { "title": "Use one Reset credit", "confirmLabel": "Use Reset", "factAccount": "Account", "factEffect": "Effect", "effectValue": "This consumes one Reset credit and cannot be undone." }
      },
      "chatgpt": { "title": "ChatGPT accounts", "description": "Choose which account official models use. Sent requests stay on the original account; new requests created after a successful switch use the new account. Tokens refresh automatically when possible; re-login is not normally required." },
      "pool": { "title": "Quota pool", "onHint": "Pooled accounts take over automatically; reserve part of each weekly allowance.", "offHint": "Manual selection remains in effect for new requests.", "rules": "Decision rules", "rulesTitle": "Quota pool decision rules", "rankHint": "The numbers are the order; use ▲▼ to adjust it.", "availableThisWeek": "Available to the pool this week", "availablePercent": "{{value}}% available this week", "currentAndNext": "Using {{current}} now; {{next}} is next.", "stalled": "No pooled account is usable; Vellum will not cross a gate or use an external account.", "empty": "The pool is empty. Add an account below to begin rotation.", "using": "In use", "next": "Next", "standby": "Standby", "outside": "Not in pool", "pause": "Pause", "resume": "Resume", "add": "Add to pool", "remove": "Remove", "moveUp": "Move earlier", "moveDown": "Move later", "burnable": "{{value}}% spendable", "atGate": "At gate", "gateRead": "{{floor}}% gate", "gateLabel": "Weekly gate for {{account}}", "gateValue": "{{floor}}% gate, {{left}}% left", "saveFailed": "Could not save quota pool: {{detail}}", "reason": { "paused": "Paused", "weeklyGate": "Weekly gate reached", "fiveHour": "5-hour window exhausted", "missingQuota": "Quota data incomplete" }, "rule": { "gate": { "title": "The gate is a floor", "body": "Automatic use stops at the weekly gate; the portion below it stays reserved. A 100% gate seals the account." }, "fiveHour": { "title": "The 5-hour window is read-only", "body": "It is an upstream throttle. The account waits when exhausted and returns after reset." }, "order": { "title": "You set the order", "body": "The numbers are the rotation order; adjust them with ▲▼. Accounts that cannot be used right now are skipped without changing the order of the rest." }, "pause": { "title": "Pause or remove", "body": "Pause keeps settings but excludes the account temporarily; remove keeps rotation from using it." }, "reset": { "title": "Reset stays manual", "body": "The pool never consumes a Reset automatically. Manual resets preserve the gate." }, "empty": { "title": "No usable account", "body": "Vellum stops explicitly instead of crossing a gate or using an external account." } } },
      "oauth": { "code": "Authorization code", "loginPage": "Login page", "browserHint": "The browser is open. This page will update after authorization.", "copied": "Authorization code copied", "copyFailed": "Copy failed; select the code manually", "copy": "Copy authorization code", "waiting": "Waiting for authorization…", "waitingBrowser": "Waiting for browser authorization", "login": "Sign in to ChatGPT", "refreshToken": "Refresh token", "logoutAll": "Sign out all" },
      "account": { "active": "Active", "authenticated": "Signed in", "current": "Current account", "useThis": "Use this" },
      "quota": { "failed": "Quota lookup failed", "loading": "Checking quota…", "retry": "Check again" },
      "reset": { "show": "Show expiry dates", "hide": "Hide expiry dates", "ledgerTitle": "Usage limit resets", "noneUsable": "No usable Reset credit on this account.", "untitled": "Reset credit", "spent": "Used", "lapsed": "Expired", "noExpiry": "No expiry date given", "failed": "Reset lookup failed", "loading": "Checking Reset…", "use": "Use reset" },
      "grok": { "title": "Grok accounts", "description": "New Grok Build requests use the current account. Switching is immediate; in-flight requests keep their original account.", "loginStatus": "Login status", "browserHint": "The official Grok CLI login flow is running; the account is added after authorization.", "loginIncomplete": "The official login flow did not finish.", "defaultAccount": "Grok CLI default account", "externalCli": "External CLI", "managed": "Managed by Vellum", "reauthenticate": "Re-authenticate", "refreshModels": "Refresh Grok models", "unlink": "Unlink", "empty": "No Grok accounts are available.", "add": "Add Grok account" },
      "opencode": { "title": "OpenCode Zen", "description": "Connect with just an API key. This matches what OpenCode's own app shows by default: only the free models. Paid Zen models need purchased credits this connection cannot verify, so they are left for the manual Add Provider flow below if you actually hold them.", "descriptionGo": "Connect with just an API key. OpenCode Go is a separate, smaller catalog behind its own endpoint — it does not include the premium GPT/Claude/Gemini models from OpenCode Zen, so this is the right choice when your key is a Go subscription. Vellum also connects OpenCode Zen's free models as a second Provider, since Go's own endpoint rejects them.", "freeRouteName": "{{name}} (Free models)", "catalog": "Catalog", "catalogZen": "OpenCode Zen (Free models)", "catalogGo": "OpenCode Go", "catalogHint": "Pick the plan you actually have. Zen connects only the free models; Go connects your Go-plan catalog, plus the same free models attached automatically.", "apiKey": "OpenCode Zen API key", "apiKeyPlaceholder": "Paste your OpenCode Zen API key", "connect": "Connect", "connecting": "Connecting…", "connected": "Connected" },
      "addProvider": { "title": "Add a Provider", "steps": { "endpoint": "Connect endpoint", "endpointHint": "Retrieve the Provider model catalog.", "probe": "Select and verify", "probeHint": "Choose a model, then verify only that model's Codex protocol capabilities.", "add": "Add", "addHint": "Confirm the result and add it to the Codex catalog." }, "endpoint": "Endpoint URL", "apiKey": "API key (optional; encrypted)", "apiKeyPlaceholder": "Leave blank for local or account-authenticated endpoints", "startProbe": "Get model list", "probing": "Retrieving model list…", "add": "Add Provider", "reprobe": "Start over" },
      "probe": { "reachable": "Reachable", "wire": "API protocol", "modelCount": "{{count}} models detected", "modelsMissing": "Not detected; enter manually", "context": "Context window", "required": "Required", "streaming": "Streaming", "supported": "Supported", "unsupported": "Not supported", "toolCalling": "Codex tool protocol", "typedToolCalls": "Structured tool calls verified", "toolCallingUnavailable": "Not verified", "chatOnly": "Chat only; no structured tool calls", "reasoning": "Reasoning", "detected": "Detected", "notDetected": "Not detected", "serverResume": "Upstream conversation retention", "remembers": "Retained upstream", "localHistory": "Not retained; Vellum stores history", "detectedModels": "Detected models", "selectionHint": "Only checked and verified models are added to the Codex catalog.", "verifyToImport": "Select to verify tool calling", "verificationRequired": "Capability verification required", "timeout": "Probe timed out", "unknown": "Unknown", "verifyModel": "Verify", "verifyingModel": "Verifying…",  "quotaWithRetry": "Quota limit (HTTP 429 · retry in {{seconds}}s)", "unauthorized": "Authentication failed (HTTP 401/403)", "protocolError": "Protocol format error", "toolCallMissing": "No structured tool call returned", "unsupportedOpenCodeProtocol": "Anthropic/Google-native protocol is not supported yet", "defaultModel": "Default model", "defaultModelHint": "All selected models are added to the Codex catalog; this only selects the initial model for the Provider.", "select": "Select", "wireHint": "This is the API protocol accepted by the Provider, not the response JSON format.", "manualPlaceholder": "Not detected; enter manually", "settingsTitle": "Endpoint probe settings", "requestSize": "Request size", "preferredWire": "Preferred API format", "preferredWireValue": "Try Responses first, then Chat Completions", "contextSource": "Context length source", "contextSourceValue": "Manual value, endpoint metadata, model cache, then Codex catalog", "history": "Conversation history", "historyValue": "If upstream cannot resume sessions, Vellum stores encrypted history." },
      "modelCatalog": { "rename": "Rename", "renameProvider": "Provider display name", "renameModel": "Model display name", "renameHint": "Display only. The upstream id is still what gets sent, and routes Codex already has keep working.", "allowPrivateNetworkHttp": "Allow plaintext HTTP to private networks", "allowPrivateNetworkHttpHint": "For a LAN or Tailscale address: plaintext HTTP to a non-loopback endpoint is refused by default; enabling this allows it to private-network addresses (LAN, CGNAT, Tailscale). Plaintext HTTP to the public internet is still refused.", "renameModelPlaceholder": "Leave empty to show the upstream id", "renameSave": "Save", "renameCancel": "Cancel", "free": "Free", "deprecated": "Retired", "vision": "Images", "visionOn": "This model accepts image input", "visionOff": "Text only; tick this before Codex will offer attachments", "visionHint": "Image support cannot be probed reliably — a text-only endpoint still returns 200 for a request carrying an image and lets the model invent an answer. Tick this according to what the model actually supports.", "tokenUnit": "tokens", "effort": "Effort", "responses": "Responses", "chat": "Chat Completions", "title": "Model catalog", "hint": "Checked models will appear in the Codex model catalog.", "empty": "No Providers yet. Add the first one using the form above.", "countSelected": "{{selected}} / {{total}} models", "count": "{{count}} models", "unknownWindow": "Window unknown", "reasoning": "Reasoning", "noReasoning": "No reasoning", "auto": "automatic", "capabilityMissing": "Model capabilities have not been obtained for this Provider.", "recent": "recently used", "reprobe": "Probe capabilities again", "reprobeInProgress": "Verifying {{count}} selected model(s)…", "reprobeSummary": "Verified {{succeeded}}/{{targeted}} selected model(s)", "reprobeSummaryWithFailures": "Verified {{succeeded}}/{{targeted}} selected model(s), {{failed}} failed", "effortNotProbed": "not yet probed", "effortUnverified": "could not verify", "effortReasonIgnored": "The Provider also accepted the deliberately invalid Effort value, so Vellum cannot prove that levels such as low, medium, or high actually take effect. Repeating the same probe is unlikely to change this result.", "effortReasonQuota": "The Effort probe was blocked by Provider quota or account access{{detail}}", "effortReasonProvider": "The Effort probe encountered a Provider error, timeout, or incomplete response{{detail}}", "effortReasonUnknown": "This Effort probe did not produce enough evidence to verify supported levels{{detail}}", "retryEffortProbe": "Retry Effort probe", "footerNote": "Enabling or disabling a Provider is saved immediately but takes effect after restarting the Proxy; the model catalog is re-read after restarting Codex. Until both are complete, the status is shown as pending.", "windowEdit": "Set the maximum context length", "windowHint": "Set the maximum context length for this model", "windowAuto": "automatic", "windowManual": "You set this by hand; clear the field to go back to the detected value.", "windowUnavailable": "This model is not in the Codex catalog yet, so there is nothing to set it on.", "windowInvalid": "The maximum context length must be a number greater than 0, or empty for automatic.", "windowSaveFailed": "Could not save the maximum context length: {{detail}}" }
    }
  },
  "context": {
    "awaitingCompaction": "No compaction has happened in this conversation yet.",
    "transcript": {
      "eyebrow": "Compaction transcript",
      "title": "Compaction instruction and replacement",
      "hint": "Shows the instruction sent to the model and the replacement text it returned. The next turn carries this replacement.",
      "chars": "{{value}} characters",
      "prompt": "Instruction sent",
      "promptNote": "The compaction prompt, exactly as the model received it.",
      "result": "Replacement written",
      "resultNote": "What the conversation carries forward from here.",
      "noPrompt": "The instruction for this compaction did not pass through Vellum.",
      "noResult": "The replacement for this compaction is not readable here.",
      "empty": "Nothing to show yet — either this conversation has not been compacted, or the compaction happened somewhere Vellum cannot read.",
      "unavailable": {
        "codexDesktopOpaque": "Codex Desktop stored this compaction as opaque state, so Vellum cannot read its instruction or replacement text.",
        "notCompacted": "This conversation has not been compacted yet.",
        "officialOpaque": "OpenAI Official owns this compaction as opaque state, so Vellum cannot read its instruction or replacement text.",
        "legacyUnreadable": "This older compaction record does not contain a readable instruction and replacement summary."
      }
    },
    "sessions": {
      "eyebrow": "Enhanced Codex core",
      "title": "Sessions on the Enhanced core",
      "hint": "Lists conversations running on the Enhanced Codex core. Conversations on OpenAI's native core are compacted inside Codex and are not shown here.",
      "listTitle": "Sessions",
      "loading": "Reading sessions…",
      "empty": "No conversation is running on the Enhanced Codex core right now.",
      "untitled": "Untitled session",
      "count_one": "{{count}} session",
      "count_other": "{{count}} sessions",
      "pick": "Show compaction for {{label}}",
      "headroom": "{{percent}}% before compaction",
      "imminent": "At the threshold",
      "search": "Search by title",
      "searchClear": "Clear the filter",
      "countFiltered": "{{shown}} of {{total}}",
      "noMatch": "Nothing matches “{{query}}”."
    },
    "title": "Context",
    "compactionEyebrow": "Context compaction",
    "compactionTitle": "Context compaction status and preview",
    "compactionHint": "Enter /compact in the Codex App to compact; Vellum shows the status and handoff history.",
    "before": "Before compaction",
    "after": "After compaction",
    "opaque": "Opaque",
    "legendAria": "Context composition",
    "previewLoading": "Calculating compaction preview…",
    "footerNote": "Compaction belongs to the executing Codex runtime. Vellum only shows observed events and keeps legacy journals read-only.",
    "segmentAria": { "before": "{{label}}, {{tokens}} tokens before compaction", "after": "{{label}}, {{tokens}} tokens after compaction" },
    "segmentLabel": {
      "canonical": "Canonical checkpoint",
      "reasoning": "Readable Reasoning Replay",
      "tool": "Tool continuation state",
      "retained": "Retained conversation",
      "observedTotal": "Codex-reported total"
    },
    "segmentNote": {
      "canonical": "Preserves the goal, constraints, decisions, progress, files, and next steps.",
      "reasoning": "Replays necessary reasoning as readable content without sending third-party private ciphertext.",
      "tool": "Preserves tool results and call pairing state that can be safely continued.",
      "retained": "Retains recent messages and active execution context, prioritizing up to {{turns}} turns.",
      "observedTotal": "This is the aggregate token transition reported by Codex; no category breakdown was exposed.",
      "default": "Processed by the Canonical compaction strategy."
    },
    "readout": {
      "tokenTransition": "{{before}} → {{after}} tokens", "savedDelta": "Saved {{tokens}} tokens", "tokenUnit": "tokens",
      "total": "Total", "officialStat": "{{before}} → OpenAI opaque canonical", "officialNote": "Official compaction state is encrypted; Vellum does not infer tokens from ciphertext size.",
      "saved": "Saved {{saved}} tokens ({{percent}}%)", "hover": "Hover a segment for its details", "share": "{{percent}}% of the pre-compaction context"
    },
    "summary": {
      "goal": "Goal", "acceptanceCriteria": "Acceptance criteria", "constraints": "Constraints", "userPreferences": "User preferences", "done": "Completed", "inProgress": "In progress", "blocked": "Blocked", "decisions": "Decisions", "changedFiles": "Changed files", "relevantFiles": "Relevant files", "commands": "Commands", "tests": "Tests", "unresolved": "Unresolved", "errors": "Errors", "criticalContext": "Critical context", "references": "References", "nextSteps": "Next steps"
    },
    "errors": { "partialRefresh": "Some data failed to refresh: {{detail}}" }
  },
  "log": {
    "title": "Log",
    "activityTitle": "Token activity",
    "loading": "Loading log…",
    "heading": "Token usage and request log",
    "days_one": "{{count}} day",
    "days_other": "{{count}} days",
    "boot_one": "Launched {{count}} time · PID {{pid}} · last launch {{time}}",
    "boot_other": "Launched {{count}} times · PID {{pid}} · last launch {{time}}",
    "bootFirst": "First launch · PID {{pid}}",
    "stats": {
      "total": "Total tokens",
      "peak": "Token peak",
      "longestRequest": "Longest request",
      "currentStreak": "Current streak",
      "longestStreak": "Longest streak"
    },
    "providers": {
      "title": "Cumulative tokens by Provider",
      "note": "OpenAI follows the Codex profile; third-party totals use upstream input + output",
      "others": "Other Providers", "fromProfile": "from Codex profile", "segmentAria": "{{provider}}: {{percent}} of cumulative tokens, {{value}}", "accountCount_one": "({{count}} account)",
      "accountCount_other": "({{count}} accounts)"
    },
    "tokens": "{{value}} tokens",
    "requests": {
      "title": "Request details",
      "note": "Showing the latest {{count}} entries",
      "empty": "No request log yet. It will appear after Codex sends its first request.",
      "filterEmpty": "No requests match the current filter.",
      "filter": { "statusAll": "All", "statusFailed": "Failed only", "providerAll": "All Providers" },
      "cached": "{{percent}}% cached",
      "connection": "conn {{id}}",
      "streamQualityTitle": "Stream quality: {{quality}}",
      "accountIdentityTitle": "Hashed control account A and execution account B",
      "streamQuality": {
        "incremental": "incremental",
        "end_flush": "end flush",
        "buffered": "buffered",
        "no_delta": "no delta"
      },
      "subagentChildren_one": "{{count}} sub-agent child",
      "subagentChildren_other": "{{count}} sub-agent children",
      "subagentChildrenTitle_one": "Locate the {{count}} spawned sub-agent run",
      "subagentChildrenTitle_other": "Locate the {{count}} spawned sub-agent runs"
    },
    "systemEvents": { "title": "System events" },
    "compaction": {
      "title": "Compaction",
      "note": "Auto-compaction decisions, separate from request rows",
      "empty": "No compaction events yet.",
      "tokens": "{{before}} → {{after}}",
      "items": "items {{before}} → {{after}}",
      "threshold": "threshold {{percent}}%",
      "window": "{{active}} / {{window}} context",
      "checkpoint": "checkpoint {{id}} (gen {{generation}})",
      "noCheckpoint": "no durable checkpoint (stateless)"
    },
    "subagent": {
      "title": "Sub-agents",
      "note": "One row per spawned agent — expand for the requested → child → completed timeline",
      "empty": "No sub-agent runs yet.",
      "summary": { "label": "Sub-agent run summary", "total": "{{count}} total", "completed": "{{count}} completed", "active": "{{count}} active", "attention": "{{count}} need attention" },
      "call": "call {{id}}",
      "child": "child {{id}}",
      "parent": "parent {{id}}",
      "locateParent": "Locate the parent request ({{id}})",
      "locateChild": "Locate the child request ({{id}})",
      "requestedAt": "requested {{time}}",
      "completedAt": "finished {{time}}",
      "linkLabel": "link: {{value}}",
      "outcome": "outcome: {{value}}",
      "error": "error: {{value}}",
      "unknownModel": "unknown model",
      "state": {
        "requested": "Requested",
        "running": "Running",
        "completed": "Completed",
        "failed": "Failed",
        "cancelled": "Cancelled",
        "ambiguous": "Ambiguous",
        "unlinked": "Unlinked"
      },
      "link": {
        "exact": "exact link",
        "heuristic": "heuristic link",
        "unlinked": "no link"
      },
      "timeline": {
        "requested": "requested",
        "child": "child",
        "completed": "completed",
        "pending": "waiting",
        "noChildLink": "no child match"
      }
    },
    "invokes": {
      "title": "Desktop calls",
      "note": "Latest {{count}} Tauri invokes — success and failure",
      "empty": "No desktop calls recorded in this session.",
      "ok": "ok",
      "error": "error"
    },
    "errors": {
      "partialRefresh": "Some data failed to refresh: {{detail}}"
    }
  },
  "remote": {
    "title": "Remote hosts",
    "blurb": "Converge an SSH host into a state Codex App can use directly. Chats, threads and sessions still belong to the Codex native daemon on that host.",
    "rescan": "Rescan",
    "rescanning": "Scanning…",
    "discovering": "Reading Codex and OpenSSH connection settings. You can keep working; SSH status loads in the background.",
    "featureDisabled": "The native Codex remote manager is not enabled in this build.",
    "legacyBrokerUnsupported": "A leftover broker pairing profile was found and is no longer supported. Rediscover this host over SSH. Vellum does not migrate old broker pairings.",
    "noHosts": "No usable connection found in either Codex App or your OpenSSH config.",
    "hostsAria": "Remote hosts",
    "probing": "Probing…",
    "probeFailed": "Probe failed",
    "notProbed": "Not probed yet",
    "updating": "Updating…",
    "lastUpdated": "Updated {{when}}",
    "sshInvalid": "SSH config is invalid",
    "otherHostBusy": "An operation is still running on {{host}}.",
    "goToHost": "Go there",
    "blockerUnknown": "Hit a condition this build does not recognise.",
    "releaseBlocked": "The bundled deployment package has not passed signature verification, so Install Codex and Update Agent are disabled. Planning and syncing an already-provisioned host still work.",
    "proxyImageUpdate": "A newer proxy image is included with this Vellum build. Select Resync to apply it to this host.",
    "state": {
      "unreachable": "Unreachable",
      "unmanaged": "Not deployed",
      "readyToPlan": "Half deployed",
      "drifted": "Drifted",
      "nativeActive": "In use",
      "detachedReady": "Runs detached"
    },
    "verdict": {
      "unreachable": "Cannot reach the Vellum Agent on this host. It may not be installed, or the SSH leg may be down.",
      "unmanaged": "Vellum does not manage this host yet. Deploy it and Codex App can use it directly.",
      "readyToPlan": "The proxy is running but has not taken over the Codex model catalog.",
      "drifted": "What is on the host differs from what Vellum recorded. Reconverge it.",
      "nativeActive": "Ready to use. Codex App is going through this host's native daemon.",
      "detachedReady": "Ready to use, and turns keep running after you close Vellum."
    },
    "summary": {
      "threads_one": "{{count}} thread running",
      "threads_other": "{{count}} threads running"
    },
    "act": {
      "bootstrap": "Deploy this host",
      "reconverge": "Reconverge",
      "plan": "Change models…",
      "replan": "Refresh preview",
      "apply": "Confirm and sync",
      "restartNative": "Restart the remote Codex daemon",
      "takeoverNative": "Take over the remote Codex daemon",
      "installCodex": "Install the pinned Codex CLI",
      "updateAgent": "Update the remote Agent",
      "syncDesktopCodex": "Sync the Desktop Codex runtime",
      "repair": "Repair the codex launcher",
      "bundle": "Export a diagnostic bundle",
      "restore": "Stop managing this host…",
      "retry": "Retry",
      "stopAppOwned": "Stop safely and retry",
      "grokLogin": "Sign in to Grok",
      "grokCancel": "Cancel Grok sign-in",
      "grokRefresh": "Refresh Grok credentials",
      "chatgptPair": "Pair the ChatGPT account selected on the desktop",
      "chatgptActivate": "Activate the ChatGPT account selected on the desktop",
      "chatgptPairRow": "Pair", "chatgptActivateRow": "Use", "chatgptPairAll": "Pair every desktop ChatGPT account",
      "chatgptPairSkip": "Skip this account",
      "executionLogin": "Add Official execution account",
      "executionSelect": "Use for model requests",
      "executionRemove": "Remove execution account",
      "devicePair": "Pair this phone"
    },
    "trust": {
      "title": "Confirm this host's SSH key",
      "explain": "This is trust-on-first-use: Vellum has not seen this host's SSH key before. Cross-check the fingerprint below through another channel before confirming it.",
      "factHost": "SSH host",
      "factFingerprint": "Fingerprint",
      "factCrossCheck": "Cross-check via",
      "crossCheckHint": "A channel you already trust — the host's own console, your Tailscale or VPN admin page, and so on.",
      "confirm": "Trust and continue",
      "checking": "Checking whether this host is already trusted…",
      "fetchFailed": "Could not read this host's SSH key.",
      "untrustedError": "This host's SSH key has not been confirmed yet."
    },
    "chore": {
      "restartNative": "Makes new config and catalog take effect. The proxy keeps running, but a turn in flight can be cut off.",
      "takeoverNative": "Codex App currently holds this daemon directly. Taking over replaces it with the Vellum-managed native daemon, and work in flight may be interrupted.",
      "installCodex": "Installs the bundled pinned Codex CLI on the host. The manifest and digest are verified before an atomic swap.",
      "updateAgent": "Updates only vellum-remote-agent on the host. Broker, proxy image and Codex CLI are not touched here.",
      "syncDesktopCodex": "Installs the exact official Linux Codex build matching this Desktop core, then qualifies and restarts the remote daemon. Desktop {{desktop}}; remote {{remote}}.",
      "repair": "Rebuilds the launcher behind the `codex` command so Codex App can find the managed CLI.",
      "bundle": "Exports Agent, Docker, proxy, daemon and operation logs with sensitive values redacted. Attach this when reporting a problem."
    },
    "fact": {
      "changes": "Changes",
      "untouched": "Leaves alone",
      "turns": "Turns in flight"
    },
    "confirm": {
      "gate": "Type the host name to continue: {{host}}",
      "bootstrap": {
        "changes": "Codex config, the model catalog, the proxy and the credentials it needs. The first run applies every qualified model; later runs reuse the selection you saved.",
        "untouched": "Your projects on the host, ordinary Codex threads, and software you installed yourself.",
        "turns": "Refused, never force-cancelled."
      },
      "restore": {
        "changes": "Restores the original Codex config and catalog, hands back the account lease, and stops the Vellum-managed proxy and runtime.",
        "untouched": "Your projects, ordinary Codex threads, and Codex and the Agent themselves.",
        "turns": "Refused, never force-cancelled."
      },
      "restartNative": {
        "changes": "Restarts the Codex native daemon on the host so new config and catalog take effect.",
        "untouched": "The proxy keeps running; config and catalog are not rewritten.",
        "turns": "A turn in flight will be cut off."
      },
      "takeoverNative": {
        "changes": "Replaces the app-server Codex App holds directly with the Vellum-managed native daemon.",
        "untouched": "Config, catalog and signed-in accounts.",
        "turns": "Work in flight on the Codex App side may be interrupted."
      },
      "installCodex": {
        "changes": "Installs the bundled pinned Codex CLI, verifying manifest and digest before an atomic swap.",
        "untouched": "Codex config, the catalog and existing threads.",
        "turns": "Nothing in flight is stopped; the new build takes effect after the daemon restarts."
      },
      "updateAgent": {
        "changes": "Updates vellum-remote-agent to the version bundled in this Vellum build and verifies its digest.",
        "untouched": "Broker, proxy image and Codex CLI are not updated here.",
        "turns": "The agent restarts itself, so remote operations pause briefly."
      },
      "syncDesktopCodex": {
        "changes": "Downloads the exact official OpenAI Linux artifact for this Desktop core, verifies its publisher digest, probes its app-server protocol, atomically installs it, and restarts the remote daemon.",
        "untouched": "The Vellum proxy image, model catalog, accounts, projects, and existing threads.",
        "turns": "A turn in flight may be interrupted when the remote daemon restarts."
      },
      "stopAppOwned": {
        "changes": "Safely stops the app-server Codex App holds directly, then runs the deployment again.",
        "untouched": "Config, catalog and signed-in accounts.",
        "turns": "If any turn is still running, Vellum refuses to stop rather than cutting it off."
      }
    },
    "blocker": {
      "agentUnavailable": "Cannot reach the Vellum Agent on the host.",
      "codexRestartRequired": "The settings are written, but nothing takes effect until a restart.",
      "credentialsMissing": "The host is missing a credential this deployment needs.",
      "desktopOfficialAccountMissing": "The desktop has no selected ChatGPT account, so nothing can be paired to the remote.",
      "dockerUnavailable": "No usable Docker on the host, so the proxy cannot start.",
      "intelMacUnsupported": "Intel Mac is not supported. The first release of Remote Manager targets Apple Silicon only.",
      "guiSessionUnavailable": "No macOS login session is available. The proxy stays resident after login and does not change power or auto-login settings.",
      "proxyPortConflict": "The remote proxy's default port is in use. Name another port in the deployment plan, or free 127.0.0.1:15722.",
      "insufficientDiskSpace": "There is not enough free space for download, extract, and rollback reserve. The replace was stopped. User data is not deleted automatically.",
      "incompleteObservation": "Managed runtime turns, tools, or approvals could not be fully observed, so the destructive operation was blocked.",
      "hostNotConfigured": "Vellum's configuration has not been written to this host yet.",
      "injectionRequiresReadyProxy": "Models can only be injected into the Codex catalog once the proxy is ready.",
      "invalidCompactionThreshold": "The compaction threshold is outside the allowed range.",
      "managedRuntimeRecoveryRequired": "The managed runtime is stuck in a bad state and needs repair first.",
      "nativeCodexVersionMismatch": "The Codex CLI on the host is the wrong version; the pinned build must be installed.",
      "nativeDaemonAppOwned": "Codex App is holding this host's app-server directly, so Vellum cannot take over.",
      "noModelsSelected": "No model is selected, so there is nothing to sync.",
      "officialAccountActivationRequired": "The remote ChatGPT account is paired but not activated.",
      "officialAccountPairingRequired": "The remote has not been paired with the ChatGPT account selected on the desktop.",
      "proxyConfigurationMissing": "The host has no proxy configuration yet.",
      "proxyConfigurationSchemaTooNew": "This host's proxy configuration is a newer format than this Vellum build understands. Deployment is blocked to avoid overwriting it — update Vellum first.",
      "proxyConfigurationUnreadable": "This host's proxy configuration could not be read (a permissions or I/O problem). Deployment is blocked rather than assumed safe to replace.",
      "systemdUserUnavailable": "The host has no usable user systemd, so the daemon cannot stay resident.",
      "versionMismatch": "The Agent version on the host does not match this Vellum build."
    },
    "configuration": {
      "upgradeRequired": "This host's proxy configuration needs a security upgrade. Redeploy to fix it automatically.",
      "repairRequired": "The existing proxy configuration is invalid. Redeploying will rebuild it.",
      "incompatible": "This host's proxy configuration is a newer format than this Vellum build understands. Update Vellum before deploying to this host.",
      "unreadable": "This host's proxy configuration could not be read (a permissions or I/O problem). This must be resolved on the host before deploying."
    },
    "phase": {
      "queued": "Queued",
      "cleanHostPreflight": "Verifying bundled artifacts and a clean host baseline",
      "hostPreflight": "Checking SSH, Docker and user services",
      "resolvingDesktopCodex": "Resolving and qualifying the Desktop-matched Codex runtime",
      "installCodex": "Installing the bundled pinned Codex CLI",
      "nativeDaemon": "Enabling durable Codex remote control",
      "deploymentPlan": "Resolving qualified models and credentials",
      "deploymentApply": "Installing proxy, credentials and catalog",
      "applying": "Applying the deployment plan",
      "verification": "Verifying proxy, daemon and detach readiness",
      "verified": "Ready",
      "restorePreflight": "Checking turns in flight and managed state",
      "restoreLease": "Restoring Codex config and model catalog",
      "restartNative": "Restarting native Codex with the restored configuration",
      "stopProxy": "Stopping the Vellum-managed proxy",
      "restoreVerification": "Verifying Vellum has left the data path",
      "restored": "No longer managed",
      "failed": "Failed"
    },
    "operation": {
      "bootstrap": "Deploying",
      "apply": "Syncing",
      "restore": "Withdrawing",
      "desktopCodexSync": "Syncing the Desktop Codex runtime",
      "elapsed": "{{clock}} elapsed"
    },
    "error": {
      "discovery": "Could not read the connection settings.",
      "desktopCodexMismatch": "The remote Codex runtime does not match this Desktop protocol. Sync the Desktop Codex runtime before retrying.",
      "boundaryKeyProvisionFailed": "Could not repair the remote proxy's authentication key. Retry, or export a diagnostic bundle if this keeps failing.",
      "actionFailed": "\"{{action}}\" failed. The host status above has been re-read and shows the current state.",
      "operationFailed": "{{action}} stopped at \"{{phase}}\"."
    },
    "detail": {
      "title": "Host detail",
      "host": "Host",
      "runtime": "Runtime",
      "version": "Versions",
      "sshResolved": "Resolved",
      "sshUnresolved": "Cannot resolve",
      "agentAbsent": "Not installed",
      "cores_one": "{{count}} core",
      "cores_other": "{{count}} cores",
      "diskFree": "{{free}} free of {{total}}",
      "dockerAbsent": "Not installed",
      "proxyReady": "Ready · {{image}}",
      "proxyNotReady": "Running, not ready",
      "proxyStopped": "Stopped",
      "config": "Config",
      "launcherLoginShell": "login shell",
      "launcherBroken": "Unavailable — run Repair",
      "daemonRunning": "Running · PID {{pid}}",
      "daemonStopped": "Not running",
      "daemonOwner": "Daemon owner",
      "durable": "durable",
      "notDurable": "not durable",
      "codexSource": "Codex source",
      "releaseTrust": "Bundle trust",
      "releaseUnverified": "Unverified · {{trust}}",
      "verified": "verified",
      "pinned": "Pinned versions",
      "inventoryBlockers": "Inventory blockers",
      "platform": "Platform",
      "proxyBackend": "Proxy backend",
      "persistence": "Persistence",
      "loginResident": "Resident after login",
      "lingerResident": "Resident via systemd linger",
      "managedHome": "Managed CODEX_HOME",
      "isolationLabel": "Isolation",
      "isolation": "Remote settings stay isolated from local Vellum / Enhanced / ~/.codex."
    },
    "desktopCodex": {
      "desktop": "Desktop Codex",
      "remote": "Remote Codex",
      "status": "Protocol compatibility",
      "states": {
        "current": "Qualified for this Desktop",
        "updateAvailable": "Remote update required",
        "qualificationRequired": "Protocol qualification required",
        "agentUpdateRequired": "Remote Agent update required first",
        "desktopUnavailable": "Desktop core unavailable",
        "unavailable": "Compatibility check unavailable"
      }
    },
    "account": {
      "title": "Accounts",
      "chatgptUnavailable": "Cannot read",
      "chatgpt": {
        "synchronized": "Synchronized",
        "pairingRequired": "Needs pairing",
        "pairingPending": "Pairing",
        "activationRequired": "Needs activation",
        "desktopAccountUnavailable": "No account selected on the desktop"
      },
      "grokReady": "Signed in as {{account}}, refresh timer active",
      "grokPartial": "Credentials installed, but the remote CLI or refresh token is not qualified yet",
      "grokAbsent": "Not set up",
      "pairingHint": "Sign in with the account selected on the desktop, and only that one:",
      "pairingRemaining": "{{remaining}} more to go",
      "pairingActive": "in use on this host",
      "pairingPaired": "paired, not in use",
      "pairingMissing": "not paired on this host",
      "pairingUnknown": "could not be read",
      "desktopDefault": "desktop default",
      "grokDeviceLogin": "Grok device login:",
      "grokWaitingUrl": "waiting for the URL…",
      "grokStarting": "Waiting for Grok CLI…"
    },
    "control": {
      "title": "Remote control",
      "identity": "Control account A",
      "sameAccountHint": "Desktop and phone must use the same ChatGPT account and workspace. Pairing never changes the remote daemon identity.",
      "deviceHint": "Short-lived device pairing:",
      "expiresAt": "expires {{when}}"
    },
    "execution": {
      "title": "Proxy execution accounts",
      "independentHint": "Official execution account B is used only for model requests. Selecting it does not change or disconnect Remote control.",
      "empty": "No Vellum-managed Official execution account.",
      "selected": "Selected",
      "select": "Select",
      "remove": "Remove",
      "namePlaceholder": "Display name for this Official account",
      "loginHint": "Complete Official device login:"
    },
    "maintenance": {
      "title": "Maintenance"
    },
    "danger": {
      "body": "Withdraws Vellum's management of this host and puts Codex back the way it was before deployment. This is not a disconnect button — using the host again means deploying it again. Your projects and ordinary Codex threads are not deleted."
    },
    "sessions": {
      "cap": "Native session observer",
      "title": "Codex daemon threads",
      "unknown": "Native session status has not been read yet.",
      "empty": "The native daemon has no visible thread right now.",
      "unreadable": "The native app-server thread list cannot be read right now.",
      "turns_one": "{{count}} turn · last {{last}}",
      "turns_other": "{{count}} turns · last {{last}}",
      "observability": {
        "nativeAppServer": "Observable",
        "unsupported": "Unsupported",
        "daemonDown": "Daemon down"
      }
    },
    "plan": {
      "cap": "Deployment plan",
      "title": "Model and policy sync",
      "ready": "Ready to sync",
      "blocked": "Blocked",
      "configHash": "Config hash",
      "catalogHash": "Catalog hash",
      "reviewPolicy": "Auto review",
      "reviewPolicyPending": "Settings changed — pending reapply",
      "credentials": "Credentials",
      "noCredentials": "None needed",
      "revision": "Revision",
      "revisionValue": "{{desired}} desired / {{observed}} observed",
      "planHash": "Plan hash",
      "rollback": "Rollback",
      "managed": "Managed fields",
      "changed": "differs",
      "same": "identical"
    }
  },
  "onboarding": {
    "title": "Get started with Vellum",
    "acts": { "what": "Overview", "connect": "Connect", "features": "Features", "launch": "Launch" },
    "folio": { "1": "I", "2": "II", "3": "III", "4": "IV" },
    "ui": { "providerSeparator": ", ", "back": "Back", "skipHint": "You can continue without a Provider and add one later from Models.", "skip": "Skip for now", "continue": "Continue", "enter": "Enter Vellum", "enterWithoutProxy": "Enter without starting Proxy", "start": "Start", "skipSetup": "Already configured; enter directly", "unwritten": "Act {{step}}, not started", "visited": "Visited; go back", "login": "Sign in", "collapse": "Collapse", "fillEndpoint": "Enter endpoint", "connected": "Connected", "unavailable": "Unavailable", "waitingAuth": "Waiting for authorization", "notConnected": "Not connected", "waitingAuthEllipsis": "Waiting for authorization…", "addAnother": "Add another", "enterCode": "Enter this code in the browser", "providerType": "Provider type", "customEndpoint": "Custom API endpoint", "opencodeHint": "OpenCode Zen uses its official API endpoint. Enter only your API key; Vellum will discover and verify compatible models.", "opencodeApiKey": "OpenCode Zen API key", "opencodeApiKeyPlaceholder": "Paste your OpenCode Zen API key", "displayName": "Display name", "displayNamePlaceholder": "e.g. self-hosted vLLM", "endpoint": "Endpoint", "optionalPlaceholder": "Leave blank if not needed", "endpointHint": "Blank means this endpoint is not verified.", "probing": "Probing…", "probeAndAdd": "Probe and add", "notConnectedYet": "Not connected yet" },
    "overture": { "lead": "Scraped and rewritten.", "history": "Medieval vellum was too costly to use only once. A filled page had its ink lifted off and was written on again — and the old text never quite disappeared.", "mission": "This app does the same to your context. When the window fills it rewrites rather than truncates, and what mattered is still there.", "subtitle": "A Codex proxy that runs only on your machine · the next four acts are written on this page" },
    "what": { "axisTargets": "ChatGPT / Grok / custom", "title": "It stands in the middle", "lead": "Codex continues to call its original endpoint. Vellum sits in the middle and forwards requests to the Provider you choose—switch Provider, model, or context without editing Codex configuration.", "axisAria": "Codex CLI through the local Vellum Proxy to ChatGPT, Grok, or a custom endpoint", "client": "Your client", "local": "Local · not forwarded", "answering": "Answers from", "noConfigTitle": "No config edits", "noConfigBody": "Vellum takes over the Codex endpoint on start and restores it on stop. No manual edits or half-applied settings.", "noTruncateTitle": "No hard truncation", "noTruncateBody": "Near the threshold, the conversation becomes a checkpoint with the goal, completed work, and exact next step, then continues instead of cutting history.", "reviewTitle": "Approval can use another model", "reviewBody": "Operations that once required your y/n can be assessed by another model. Sandbox, network, and file permissions stay unchanged.", "privacy": "Everything stays on this machine. Vellum has no server and your conversations do not pass through us; credentials are encrypted locally." },
    "connect": { "title": "Connect a Provider", "lead": "One Provider is enough to start. Add more later or keep several active—different sessions can use different Providers, and the one nearing its quota is visible first.", "chatgptClaim": "Use your existing subscription", "chatgptDetail": "Sign in with the official device-code flow; no API key is needed. Subscription Reset credits are available on the Models page.", "grokClaim": "Use the official CLI login", "grokDetail": "Grok CLI must be installed on this machine. After login, multiple accounts can be linked and their quotas are tracked separately.", "grokUnavailable": "Grok CLI is not installed or could not be detected.", "customName": "Custom endpoint", "customClaim": "Any OpenAI-compatible service", "customDetail": "Enter an endpoint and API key; Vellum detects available models and context length. Self-hosted, proxy, and third-party services use this path.", "codexToolProtocolUnavailable": "The endpoint is reachable, but none of its models returned a typed function call. Configure tool calling in vLLM, Ollama, or llama.cpp before adding it to Codex.", "credentialNote": "Credentials are encrypted with the system keychain and kept locally. They are not written to Codex configuration or logs." },
    "features": { "title": "These are already available", "lead": "Nothing here needs extra setup to take effect. This act shows what is available and where to change it later.", "reviewTerm": "Auto review\nmodel-selectable", "settingsPage": "Settings", "reviewBody": "Codex approval requests can be assessed by a selected Provider and model, with an optional fallback. The UI marks fallback use instead of switching silently.", "resetTerm": "Reset credits\nuse them here", "modelsPage": "Models", "resetBody": "ChatGPT Reset credits are shown beside each account. Use one here and the balance is refreshed immediately.", "sessionTerm": "Each window\ntracks itself", "todayPage": "Today", "sessionBody": "Each Codex window has its own conversation and context window. Today shows the session closest to the compaction threshold first; it is not one averaged number." },
    "launch": { "title": "Connect Codex", "runningTitle": "Using Vellum", "lead": "When started, Vellum takes over Codex endpoint settings and restores them unchanged when stopped. The action is reversible.", "runningLead": "The endpoint is managed and requests are forwarded to your connected Providers. Stopping restores the settings from before Vellum was installed.", "startProxy": "Start Proxy", "noProviderHint": "No Provider is connected yet—you can start, but no model will answer until one is connected.", "connectedProviders": "Connected Providers", "howToStart": "How to start", "howToStartValue": "Open a new Codex window. Restart an existing window so it uses Vellum.", "howToStop": "How to stop", "howToStopValue": "Use Stop Proxy and restore Codex on the Today page", "reopenGuide": "Reopen this guide", "reopenGuideValue": "Settings page" }
  }
};

export default en;
