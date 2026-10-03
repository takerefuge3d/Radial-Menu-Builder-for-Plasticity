; Installer hooks for the .exe (NSIS) installer.
;
; Until 1.3.0 the app was called "Plasticity Radial Menu Builder". Windows tracks installs by
; that name, so without this the renamed app would install beside the old one instead of
; replacing it. Before installing, remove the old copy quietly, the way an update does: the
; app's settings, themes and matcap collections live under com.radial.builder and are kept.
; (The .msi installer handles this itself, through the upgrade code in tauri.conf.json.)

!define OLD_PRODUCTNAME "Plasticity Radial Menu Builder"
!define OLD_UNINSTKEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\${OLD_PRODUCTNAME}"

; An old .exe install, per user (HKCU) or for everyone (HKLM).
!macro RMB_REMOVE_OLD_NSIS ROOT
  ReadRegStr $R7 ${ROOT} "${OLD_UNINSTKEY}" "UninstallString"
  ReadRegStr $R8 ${ROOT} "${OLD_UNINSTKEY}" "InstallLocation"
  ${If} $R7 != ""
    ; InstallLocation is saved in quotes
    StrCpy $R9 $R8 1
    ${If} $R9 == '"'
      StrCpy $R8 $R8 "" 1
      StrCpy $R8 $R8 -1
    ${EndIf}
    DetailPrint "Removing ${OLD_PRODUCTNAME} (this app's old name)..."
    ; /UPDATE keeps the app data; _?= makes the uninstaller finish before we carry on
    ExecWait '$R7 /S /UPDATE _?=$R8'
    Delete "$R8\uninstall.exe"
    RMDir "$R8"
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREINSTALL
  Push $R7
  Push $R8
  Push $R9
  Push $0
  Push $1

  !insertmacro RMB_REMOVE_OLD_NSIS HKCU
  !insertmacro RMB_REMOVE_OLD_NSIS HKLM

  ; An old .msi install. Its uninstall entry is keyed by product code, so look for it by name.
  StrCpy $0 0
  rmb_msi_loop:
    EnumRegKey $1 HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall" $0
    StrCmp $1 "" rmb_msi_done
    IntOp $0 $0 + 1
    ReadRegStr $R8 HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\$1" "DisplayName"
    StrCmp $R8 "${OLD_PRODUCTNAME}" 0 rmb_msi_loop
    ReadRegStr $R7 HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\$1" "UninstallString"
    StrCpy $R9 $R7 7
    StrCmp $R9 "MsiExec" 0 rmb_msi_loop ; StrCmp ignores case
    DetailPrint "Removing ${OLD_PRODUCTNAME} (this app's old name)..."
    ExecWait 'msiexec.exe /x $1 /passive /norestart'
  rmb_msi_done:

  Pop $1
  Pop $0
  Pop $R9
  Pop $R8
  Pop $R7
!macroend
