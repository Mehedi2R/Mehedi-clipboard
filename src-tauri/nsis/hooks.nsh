; Mehedi Clipboard — NSIS installer hooks
;
; Goal: only ONE installation of the app may exist on a machine.
;  * same version  -> clean replace (repair install)
;  * newer version -> in-place upgrade over the old one
;  * never a second "Mehedi Clipboard" entry in Apps & features
;
; Tauri includes this file through bundle.windows.nsis.installerHooks.

!include "LogicLib.nsh"

!macro MC_REMOVE_OLD ROOT
  ; Tauri writes its uninstall entry under the product name key.
  ReadRegStr $R0 ${ROOT} \
    "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCTNAME}" \
    "UninstallString"
  ReadRegStr $R1 ${ROOT} \
    "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCTNAME}" \
    "InstallLocation"

  ${If} $R0 != ""
    ; Strip surrounding quotes so we can call the uninstaller silently.
    StrCpy $R2 $R0 1
    ${If} $R2 == '"'
      StrCpy $R0 $R0 "" 1
      StrLen $R3 $R0
      IntOp $R3 $R3 - 1
      StrCpy $R0 $R0 $R3
    ${EndIf}

    ${If} $R1 == ""
      StrCpy $R1 "$INSTDIR"
    ${EndIf}

    ; /P keeps user data (history, settings) — this is an upgrade, not a purge.
    ExecWait '"$R0" /P /S _?=$R1' $R4
    Sleep 800

    ; The old uninstaller copies itself to %TEMP% when run with _?= ; clean up
    ; the leftover registry entry so only the new install is listed.
    DeleteRegKey ${ROOT} \
      "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCTNAME}"
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREINSTALL
  ; 1) Close the running app so files are not locked during the upgrade.
  nsExec::Exec 'taskkill /IM "Mehedi Clipboard.exe" /F /T'
  Pop $0
  Sleep 600

  ; 2) Remove any previous installation, no matter how it was installed.
  ;    Per-user install (HKCU) first, then machine-wide (HKLM).
  !insertmacro MC_REMOVE_OLD "HKCU"
  !insertmacro MC_REMOVE_OLD "HKLM"
!macroend

!macro NSIS_HOOK_POSTINSTALL
  ; Nothing extra: Tauri writes the fresh uninstall entry itself.
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  nsExec::Exec 'taskkill /IM "Mehedi Clipboard.exe" /F /T'
  Pop $0
  Sleep 400
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  ; Drop the autostart Run key so a removed app cannot start with Windows.
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "Mehedi Clipboard"
!macroend
