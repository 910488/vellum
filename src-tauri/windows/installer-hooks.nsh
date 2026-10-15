; Codex Desktop, not Vellum, starts the bundled bridge (through CODEX_CLI_PATH),
; and the bridge starts the Enhanced core and relay. They can outlive Vellum
; and keep their executables open, so the installer stops them before it
; overwrites or removes the files.
!include "LogicLib.nsh"
Var VellumRetiredMask
Var VellumRetiredSuffix

!ifndef VELLUM_INSTALL_CORE
  !if /FileExists "${__FILEDIR__}\..\binaries\vellum-enhanced-codex.exe"
    !define VELLUM_INSTALL_CORE 1
  !else
    !define VELLUM_INSTALL_CORE 0
  !endif
!endif
!ifndef VELLUM_INSTALL_BRIDGE
  !if /FileExists "${__FILEDIR__}\..\binaries\vellum-codex-app-server.exe"
    !define VELLUM_INSTALL_BRIDGE 1
  !else
    !define VELLUM_INSTALL_BRIDGE 0
  !endif
!endif
!ifndef VELLUM_INSTALL_RELAY
  !if /FileExists "${__FILEDIR__}\..\binaries\vellum-codex-relay.exe"
    !define VELLUM_INSTALL_RELAY 1
  !else
    !define VELLUM_INSTALL_RELAY 0
  !endif
!endif

!macro VELLUM_RESTORE_IMAGE FILE FLAG
  IntOp $R2 $VellumRetiredMask & ${FLAG}
  ${If} $R2 != 0
    ClearErrors
    Rename "$INSTDIR\${FILE}.old-$R0-$R1" "$INSTDIR\${FILE}"
    ${If} ${Errors}
      DetailPrint "Unable to restore ${FILE} from its retired image."
    ${EndIf}
  ${EndIf}
!macroend

Function VellumRollbackRetiredImages
  Push $R2
  !insertmacro VELLUM_RESTORE_IMAGE "vellum-codex-relay.exe" 32
  !insertmacro VELLUM_RESTORE_IMAGE "vellum-enhanced-codex.exe" 16
  !insertmacro VELLUM_RESTORE_IMAGE "vellum-codex-app-server.exe" 8
  !insertmacro VELLUM_RESTORE_IMAGE "binaries\vellum-codex-relay.exe" 4
  !insertmacro VELLUM_RESTORE_IMAGE "binaries\vellum-enhanced-codex.exe" 2
  !insertmacro VELLUM_RESTORE_IMAGE "binaries\vellum-codex-app-server.exe" 1
  Pop $R2
FunctionEnd
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
!macro VELLUM_RETIRE_IMAGE FILE FLAG
  ${If} ${FileExists} "$INSTDIR\${FILE}"
    ClearErrors
    Rename "$INSTDIR\${FILE}" "$INSTDIR\${FILE}.old-$R0-$R1"
    ${If} ${Errors}
      DetailPrint "Unable to retire ${FILE}; restoring this run's images before relaunching Vellum."
      Call VellumRollbackRetiredImages
      Exec '"$INSTDIR\vellum-proxy-desktop.exe"'
      MessageBox MB_OK|MB_ICONSTOP "Vellum could not replace ${FILE}. Installation was stopped. Please run this installer again." /SD IDOK
      SetErrorLevel 1
      Abort
    ${EndIf}
    IntOp $VellumRetiredMask $VellumRetiredMask | ${FLAG}
  ${EndIf}
!macroend

!macro VELLUM_RETIRE_SIDECARS
  StrCpy $VellumRetiredMask 0
  StrCpy $VellumRetiredSuffix "$R0-$R1"
!if ${VELLUM_INSTALL_BRIDGE} == 1
  !insertmacro VELLUM_RETIRE_IMAGE "binaries\vellum-codex-app-server.exe" 1
!endif
!if ${VELLUM_INSTALL_CORE} == 1
  !insertmacro VELLUM_RETIRE_IMAGE "binaries\vellum-enhanced-codex.exe" 2
!endif
!if ${VELLUM_INSTALL_RELAY} == 1
  !insertmacro VELLUM_RETIRE_IMAGE "binaries\vellum-codex-relay.exe" 4
!endif
  ; Resources are installed beneath binaries; legacy root images are not
  ; destinations in this payload and must not be retired.
!macroend

; Never leave an original absent when this payload did not replace it.
!macro VELLUM_RESTORE_UNREPLACED_IMAGE FILE FLAG
  IntOp $R2 $VellumRetiredMask & ${FLAG}
  ${If} $R2 != 0
  ${AndIfNot} ${FileExists} "$INSTDIR\${FILE}"
    ClearErrors
    Rename "$INSTDIR\${FILE}.old-$VellumRetiredSuffix" "$INSTDIR\${FILE}"
    ${If} ${Errors}
      DetailPrint "Unable to restore unreplaced ${FILE}; retaining its old image."
    ${EndIf}
  ${EndIf}
!macroend

!macro NSIS_HOOK_POSTINSTALL
  Push $R2
  !insertmacro VELLUM_RESTORE_UNREPLACED_IMAGE "vellum-codex-relay.exe" 32
  !insertmacro VELLUM_RESTORE_UNREPLACED_IMAGE "vellum-enhanced-codex.exe" 16
  !insertmacro VELLUM_RESTORE_UNREPLACED_IMAGE "vellum-codex-app-server.exe" 8
  !insertmacro VELLUM_RESTORE_UNREPLACED_IMAGE "binaries\vellum-codex-relay.exe" 4
  !insertmacro VELLUM_RESTORE_UNREPLACED_IMAGE "binaries\vellum-enhanced-codex.exe" 2
  !insertmacro VELLUM_RESTORE_UNREPLACED_IMAGE "binaries\vellum-codex-app-server.exe" 1
  Pop $R2
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
  !insertmacro VELLUM_RETIRE_SIDECARS
  Pop $1
  Pop $0
  Pop $R1
  Pop $R0
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  !insertmacro VELLUM_STOP_SIDECARS
!macroend
