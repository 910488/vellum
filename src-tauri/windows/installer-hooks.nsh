; Codex Desktop, not Vellum, starts the bundled bridge (through CODEX_CLI_PATH),
; and the bridge starts the Enhanced core and relay. They can outlive Vellum
; and keep their executables open, so the installer stops them before it
; overwrites or removes the files.
!macro VELLUM_STOP_SIDECARS
  nsExec::Exec 'taskkill /F /T /IM vellum-codex-app-server.exe'
  nsExec::Exec 'taskkill /F /T /IM vellum-enhanced-codex.exe'
  nsExec::Exec 'taskkill /F /T /IM vellum-codex-relay.exe'
  Sleep 500
!macroend

!macro NSIS_HOOK_PREINSTALL
  !insertmacro VELLUM_STOP_SIDECARS
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  !insertmacro VELLUM_STOP_SIDECARS
!macroend
