; Codex Desktop, not Vellum, starts the bundled bridge (through CODEX_CLI_PATH),
; and the bridge starts the Enhanced core and relay. They can outlive Vellum
; and keep their executables open, so the installer stops them before it
; overwrites or removes the files.
!macro VELLUM_STOP_SIDECARS
  Push $R2
  nsExec::Exec 'taskkill /F /T /IM vellum-codex-app-server.exe'
  Pop $R2
  nsExec::Exec 'taskkill /F /T /IM vellum-enhanced-codex.exe'
  Pop $R2
  nsExec::Exec 'taskkill /F /T /IM vellum-codex-relay.exe'
  Pop $R2
  Pop $R2
!macroend

; Desktop can respawn a killed bridge immediately. Rename the old image so
; respawning cannot re-lock the destination NSIS is about to populate.
!macro VELLUM_RETIRE_IMAGE FILE
  IfFileExists "$INSTDIR\${FILE}" 0 +7
  ClearErrors
  Rename "$INSTDIR\${FILE}" "$INSTDIR\${FILE}.old-$R0-$R1"
  IfErrors 0 +4
  MessageBox MB_OK|MB_ICONSTOP "Vellum could not replace ${FILE}. Installation was stopped. Please run this installer again." /SD IDOK
  SetErrorLevel 1
  Abort
!macroend

!macro NSIS_HOOK_PREINSTALL
  !insertmacro VELLUM_STOP_SIDECARS
  Push $R0
  Push $R1
  Push $0
  Push $1
  System::Call 'kernel32::GetTickCount() i.r0'
  System::Call 'kernel32::GetCurrentProcessId() i.r1'
  StrCpy $R0 $0
  StrCpy $R1 $1
  !insertmacro VELLUM_RETIRE_IMAGE "binaries\vellum-codex-app-server.exe"
  !insertmacro VELLUM_RETIRE_IMAGE "binaries\vellum-enhanced-codex.exe"
  !insertmacro VELLUM_RETIRE_IMAGE "binaries\vellum-codex-relay.exe"
  !insertmacro VELLUM_RETIRE_IMAGE "vellum-codex-app-server.exe"
  !insertmacro VELLUM_RETIRE_IMAGE "vellum-enhanced-codex.exe"
  !insertmacro VELLUM_RETIRE_IMAGE "vellum-codex-relay.exe"
  Pop $1
  Pop $0
  Pop $R1
  Pop $R0
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  !insertmacro VELLUM_STOP_SIDECARS
!macroend
