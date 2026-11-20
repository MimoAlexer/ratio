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

Name "Working Set"
OutFile "installer.exe"
InstallDir "$LOCALAPPDATA\Programs\Working Set"
SetCompressor /SOLID lzma
SetDateSave off
AllowSkipFiles off
ShowInstDetails show
ShowUninstDetails show
BrandingText "Working Set"
VIProductVersion "${VERSION}.0"
VIAddVersionKey /LANG=1033 "ProductName" "Working Set"
VIAddVersionKey /LANG=1033 "ProductVersion" "${VERSION}"
VIAddVersionKey /LANG=1033 "FileDescription" "Working Set desktop installer"
VIAddVersionKey /LANG=1033 "FileVersion" "${VERSION}"
VIAddVersionKey /LANG=1033 "LegalCopyright" "Working Set contributors"

!define EDGE_CLIENT "Software\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}"
!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\WorkingSet"
!define MUI_ICON "icon.ico"
!define MUI_UNICON "icon.ico"
!define MUI_FINISHPAGE_RUN "$INSTDIR\working-set.exe"
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
  ${IfNot} ${RunningX64}
    MessageBox MB_OK|MB_ICONSTOP "Working Set requires 64-bit Windows." /SD IDOK
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

  MessageBox MB_YESNO|MB_ICONEXCLAMATION "Working Set needs Microsoft Edge WebView2 Runtime. Open Microsoft's download page now? Choose the Evergreen Runtime, install it, then run this installer again." /SD IDNO IDNO no_browser
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

Section "Install"
  SetOutPath "$INSTDIR"
  SetOverwrite on
  !insertmacro InstallPayload
  File /oname=working-set.ico "icon.ico"
  WriteUninstaller "$INSTDIR\Uninstall.exe"
  CreateShortcut "$SMPROGRAMS\Working Set.lnk" "$INSTDIR\working-set.exe" "" "$INSTDIR\working-set.ico"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayName" "Working Set"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "Publisher" "Working Set contributors"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayIcon" '"$INSTDIR\working-set.ico",0'
  WriteRegStr HKCU "${UNINSTALL_KEY}" "UninstallString" '"$INSTDIR\Uninstall.exe"'
  WriteRegStr HKCU "${UNINSTALL_KEY}" "QuietUninstallString" '"$INSTDIR\Uninstall.exe" /S'
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoRepair" 1
SectionEnd

Section "Uninstall"
  SetShellVarContext current
  SetRegView 64
  ; Check for a running process before touching shortcuts or registration.
  SetOutPath "$TEMP"
  IfFileExists "$INSTDIR\working-set.exe" 0 app_removed
  ClearErrors
  Delete "$INSTDIR\working-set.exe"
  IfErrors 0 app_removed
    MessageBox MB_OK|MB_ICONSTOP "Close Working Set and run the uninstaller again." /SD IDOK
    SetErrorLevel 4
    Quit
  app_removed:
  Delete "$SMPROGRAMS\Working Set.lnk"
  !insertmacro RemovePayload
  Delete "$INSTDIR\working-set.ico"
  Delete "$INSTDIR\Uninstall.exe"
  RMDir "$INSTDIR"
  DeleteRegKey HKCU "${UNINSTALL_KEY}"
SectionEnd
