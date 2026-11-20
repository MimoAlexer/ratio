# -*- coding: utf-8 -*-
!ifdef TARGET_STUB
Target ${TARGET_STUB}
!endif
Unicode true
RequestExecutionLevel user
ManifestDPIAware true
!include "MUI2.nsh"
!include "LogicLib.nsh"
!include "WordFunc.nsh"
!include "x64.nsh"
!include "payload.nsh"

Name "Ratio"
OutFile "installer.exe"
InstallDir "$LOCALAPPDATA\Programs\Ratio"
SetCompressor /SOLID lzma
SetDateSave off
AllowSkipFiles off
ShowInstDetails show
ShowUninstDetails show
BrandingText "Ratio"
VIProductVersion "${VERSION}.0"
VIAddVersionKey /LANG=1033 "ProductName" "Ratio"
VIAddVersionKey /LANG=1033 "ProductVersion" "${VERSION}"
VIAddVersionKey /LANG=1033 "FileDescription" "Ratio desktop installer"
VIAddVersionKey /LANG=1033 "FileVersion" "${VERSION}"
VIAddVersionKey /LANG=1033 "LegalCopyright" "Ratio contributors"

!define EDGE_CLIENT "Software\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}"
!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\Ratio"
!define LEGACY_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\WorkingSet"
Var LegacyDir
Var LegacyExeHandle
!define MUI_ICON "icon.ico"
!define MUI_UNICON "icon.ico"
!define MUI_FINISHPAGE_RUN "$INSTDIR\ratio.exe"
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

; Input $0 is the documented four-part REG_SZ; result $1 is 1 when usable.
Function CheckVersion
  ${StrFilter} "$0" "1" "." "" $2
  ${WordFind} "$0" "." "#" $3
  ${WordFind} "$0" "." "*" $4
  StrCpy $1 0
  ${If} $2 == $0
  ${AndIf} $3 == 4
  ${AndIf} $4 == 3
    ${VersionCompare} "$0" "0.0.0.0" $1
  ${EndIf}
FunctionEnd

Function .onInit
  SetShellVarContext current
  StrCpy $INSTDIR "$LOCALAPPDATA\Programs\Ratio"
  ${IfNot} ${RunningX64}
    MessageBox MB_OK|MB_ICONSTOP "Ratio requires 64-bit Windows." /SD IDOK
    SetErrorLevel 2
    Quit
  ${EndIf}

  ; Microsoft's Evergreen registration is machine-wide in the 32-bit
  ; registry view, or per user in the native view.
  SetRegView 32
  ReadRegStr $0 HKLM "${EDGE_CLIENT}" "pv"
  SetRegView 64
  Call CheckVersion
  ${If} $1 == 1
    Return
  ${EndIf}
  ReadRegStr $0 HKCU "${EDGE_CLIENT}" "pv"
  Call CheckVersion
  ${If} $1 == 1
    Return
  ${EndIf}

  MessageBox MB_YESNO|MB_ICONEXCLAMATION "Ratio needs Microsoft Edge WebView2 Runtime. Open Microsoft's download page now? Choose the Evergreen Runtime, install it, then run this installer again." /SD IDNO IDNO no_browser
  ExecShell "open" "https://developer.microsoft.com/microsoft-edge/webview2/#download"
  no_browser:
    SetErrorLevel 3
    Quit
FunctionEnd

!macro RetireDll name
  ReadRegDWORD $0 HKCU "${UNINSTALL_KEY}" "Owns-${name}"
  ${If} $0 == 1
    Delete "$INSTDIR\${name}"
    DeleteRegValue HKCU "${UNINSTALL_KEY}" "Owns-${name}"
  ${EndIf}
!macroend

; v0.1.0 recorded these fields even when its /D switch selected a custom
; location. Never execute its uninstall command or recurse into that folder.
Function CheckLegacyInstall
  StrCpy $LegacyDir ""
  StrCpy $LegacyExeHandle ""
  ReadRegStr $0 HKCU "${LEGACY_KEY}" "DisplayName"
  ${If} $0 == ""
    Return
  ${EndIf}
  ReadRegStr $1 HKCU "${LEGACY_KEY}" "InstallLocation"
  ReadRegStr $2 HKCU "${LEGACY_KEY}" "UninstallString"
  ReadRegStr $3 HKCU "${LEGACY_KEY}" "DisplayVersion"
  ${If} $0 != "Working Set"
  ${OrIf} $1 == ""
  ${OrIf} $2 != '"$1\Uninstall.exe"'
  ${OrIf} $3 != "0.1.0"
    Goto unrecognized
  ${EndIf}
  GetFullPathName $LegacyDir "$1"
  ${If} $LegacyDir != $1
    Goto unrecognized
  ${EndIf}
  ${If} $LegacyDir != "$LOCALAPPDATA\Programs\Working Set"
    ; A custom location must contain the original 0.1.0.0 program.
    ClearErrors
    GetDLLVersion "$LegacyDir\working-set.exe" $0 $1
    IfErrors unrecognized
    ${If} $0 != 1
    ${OrIf} $1 != 0
      Goto unrecognized
    ${EndIf}
  ${EndIf}
  IfFileExists "$LegacyDir\working-set.exe" 0 done
  ; Request delete access before creating any new files. Hold this handle
  ; until the new installation has been written.
  System::Call 'kernel32::CreateFileW(w "$LegacyDir\working-set.exe", i 0x10000, i 7, p 0, i 3, i 0x80, p 0) p.r0'
  ${If} $0 == -1
    MessageBox MB_OK|MB_ICONSTOP "Close Working Set and run the Ratio installer again." /SD IDOK
    SetErrorLevel 4
    Quit
  ${EndIf}
  StrCpy $LegacyExeHandle $0
  done:
    Return
  unrecognized:
    MessageBox MB_OK|MB_ICONSTOP "Windows has an unrecognized Working Set uninstall entry. Use Installed apps to remove the previous installation, then run the Ratio installer again." /SD IDOK
    SetErrorLevel 5
    Quit
FunctionEnd

!macro RetireLegacyDll name reused
  ReadRegDWORD $0 HKCU "${LEGACY_KEY}" "Owns-${name}"
  ${If} $0 == 1
  ${AndIf} $LegacyDir != "$INSTDIR"
    Delete "$LegacyDir\${name}"
  !if ${reused} == 0
  ${ElseIf} $0 == 1
    Delete "$LegacyDir\${name}"
  !endif
  ${EndIf}
!macroend

Function FinishLegacyInstall
  ${If} $LegacyDir == ""
    Return
  ${EndIf}
  ${If} $LegacyExeHandle != ""
    System::Call 'kernel32::CloseHandle(p $LegacyExeHandle)'
    StrCpy $LegacyExeHandle ""
  ${EndIf}
  ClearErrors
  IfFileExists "$LegacyDir\working-set.exe" 0 legacy_app_removed
  Delete "$LegacyDir\working-set.exe"
  IfErrors legacy_busy
  legacy_app_removed:
  !insertmacro RemoveLegacyDlls
  Delete "$LegacyDir\working-set.ico"
  ${If} $LegacyDir != "$INSTDIR"
    Delete "$LegacyDir\Uninstall.exe"
  ${EndIf}
  ${If} $LegacyDir == "$LOCALAPPDATA\Programs\Working Set"
    RMDir "$LegacyDir"
  ${EndIf}
  Delete "$SMPROGRAMS\Working Set.lnk"
  DeleteRegKey HKCU "${LEGACY_KEY}"
  Return
  legacy_busy:
    MessageBox MB_OK|MB_ICONSTOP "Close Working Set and run the Ratio installer again to finish the upgrade." /SD IDOK
    SetErrorLevel 4
    Quit
FunctionEnd

Section "Install"
  Call CheckLegacyInstall
  SetOutPath "$INSTDIR"
  SetOverwrite on
  !insertmacro InstallPayload
  File /oname=ratio.ico "icon.ico"
  WriteUninstaller "$INSTDIR\Uninstall.exe"
  CreateShortcut "$SMPROGRAMS\Ratio.lnk" "$INSTDIR\ratio.exe" "" "$INSTDIR\ratio.ico"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayName" "Ratio"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "Publisher" "Ratio contributors"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayIcon" '"$INSTDIR\ratio.ico",0'
  WriteRegStr HKCU "${UNINSTALL_KEY}" "UninstallString" '"$INSTDIR\Uninstall.exe"'
  WriteRegStr HKCU "${UNINSTALL_KEY}" "QuietUninstallString" '"$INSTDIR\Uninstall.exe" /S'
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoRepair" 1
  Call FinishLegacyInstall
SectionEnd

Section "Uninstall"
  SetShellVarContext current
  SetRegView 64
  ; Check for a running process before touching shortcuts or registration.
  SetOutPath "$TEMP"
  IfFileExists "$INSTDIR\ratio.exe" 0 app_removed
  ClearErrors
  Delete "$INSTDIR\ratio.exe"
  IfErrors 0 app_removed
    MessageBox MB_OK|MB_ICONSTOP "Close Ratio and run the uninstaller again." /SD IDOK
    SetErrorLevel 4
    Quit
  app_removed:
  Delete "$SMPROGRAMS\Ratio.lnk"
  !insertmacro RemovePayload
  Delete "$INSTDIR\ratio.ico"
  Delete "$INSTDIR\Uninstall.exe"
  RMDir "$INSTDIR"
  DeleteRegKey HKCU "${UNINSTALL_KEY}"
SectionEnd
