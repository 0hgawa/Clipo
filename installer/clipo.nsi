; Clipo — per-user installer (no admin, no UAC).
;
; Build it with NSIS (https://nsis.sourceforge.io):
;   makensis clipo.nsi
; (or just run build.ps1 next to this file.)
;
; Lives in installer/ inside the repo. The .nsi, build.ps1 and branding art are
; tracked; the built Clipo-Setup.exe / .minisig / latest.json are git-ignored.
;
; No runtime bootstrapper: the Slint app is a single native exe, so the payload
; is just clipo.exe + the ffmpeg sidecar (GIF export). Translations and icons
; are compiled into the binary.

Unicode true
; Without this the installer is bitmap-stretched on high-DPI screens (blurry
; text); the manifest makes Windows render it crisply at the real DPI.
ManifestDPIAware true

!define APP_NAME    "Clipo"
!define APP_EXE     "clipo.exe"
!define APP_VERSION "0.1.0"
!define PUBLISHER   "Ohgawa"
!define APP_ID      "Clipo"
!define UNINST_KEY  "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APP_ID}"

; Source artefacts (the built release). Absolute paths so makensis can run from
; anywhere; spaces are fine inside the quotes.
!define SRC    "D:\Apps\Clipo\target\release"
!define ASSETS "D:\Apps\Clipo\assets"
; The installer's own art + the build output live in this folder (installer/).
!define HERE   "D:\Apps\Clipo\installer"

Name "${APP_NAME}"
; Absolute so the output always lands in installer/ regardless of makensis' cwd.
OutFile "${HERE}\Clipo-Setup.exe"
RequestExecutionLevel user
InstallDir "$LOCALAPPDATA\Programs\${APP_NAME}"
InstallDirRegKey HKCU "Software\${APP_ID}" "InstallDir"
SetCompressor /SOLID lzma
ShowInstDetails show
ShowUninstDetails show
BrandingText "Copyright (c) 2026 ${PUBLISHER}"

!include "MUI2.nsh"
!include "FileFunc.nsh"

!define MUI_ICON   "${ASSETS}\icon.ico"
!define MUI_UNICON "${ASSETS}\icon.ico"
; Custom chrome — replaces the default blue NSIS background: a header strip on
; the inner pages and a sidebar on the
; Welcome/Finish pages, for both the installer and the uninstaller.
!define MUI_HEADERIMAGE
!define MUI_HEADERIMAGE_BITMAP        "${HERE}\installer-header.bmp"
!define MUI_HEADERIMAGE_UNBITMAP      "${HERE}\installer-header.bmp"
!define MUI_WELCOMEFINISHPAGE_BITMAP  "${HERE}\installer-sidebar.bmp"
!define MUI_UNWELCOMEFINISHPAGE_BITMAP "${HERE}\installer-sidebar.bmp"
!define MUI_FINISHPAGE_RUN "$INSTDIR\${APP_EXE}"
!define MUI_FINISHPAGE_RUN_TEXT "Launch Clipo"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

VIProductVersion "0.1.0.0"
VIAddVersionKey "ProductName"     "${APP_NAME}"
VIAddVersionKey "FileVersion"     "${APP_VERSION}"
VIAddVersionKey "ProductVersion"  "${APP_VERSION}"
VIAddVersionKey "CompanyName"     "${PUBLISHER}"
VIAddVersionKey "LegalCopyright"  "Copyright (c) 2026 ${PUBLISHER}"
VIAddVersionKey "FileDescription" "${APP_NAME} Setup"

; Close a running instance (and its ffmpeg child) so the files aren't locked.
!macro CloseRunning
  nsExec::Exec 'taskkill /IM ${APP_EXE} /F /T'
!macroend

Function .onInit
  ; Re-running over an existing install just updates in place: InstallDirRegKey
  ; resolves $INSTDIR to the current location and the files overwrite (settings
  ; like autostart / "open with" are kept). The modern, friction-free pattern —
  ; uninstall lives in Windows' Add/Remove Programs, where it has a real button.
  !insertmacro CloseRunning
FunctionEnd

Section "Install"
  SetOutPath "$INSTDIR"
  File "${SRC}\${APP_EXE}"
  File "${SRC}\ffmpeg.exe"

  CreateShortcut "$SMPROGRAMS\${APP_NAME}.lnk" "$INSTDIR\${APP_EXE}"
  CreateShortcut "$DESKTOP\${APP_NAME}.lnk"    "$INSTDIR\${APP_EXE}"

  WriteUninstaller "$INSTDIR\Uninstall.exe"
  WriteRegStr HKCU "Software\${APP_ID}" "InstallDir" "$INSTDIR"

  ; Add/Remove Programs (per-user).
  WriteRegStr   HKCU "${UNINST_KEY}" "DisplayName"     "${APP_NAME}"
  WriteRegStr   HKCU "${UNINST_KEY}" "DisplayVersion"  "${APP_VERSION}"
  WriteRegStr   HKCU "${UNINST_KEY}" "Publisher"       "${PUBLISHER}"
  WriteRegStr   HKCU "${UNINST_KEY}" "DisplayIcon"     "$INSTDIR\${APP_EXE}"
  WriteRegStr   HKCU "${UNINST_KEY}" "UninstallString" '"$INSTDIR\Uninstall.exe"'
  WriteRegStr   HKCU "${UNINST_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoRepair" 1
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  IntFmt $0 "0x%08X" $0
  WriteRegDWORD HKCU "${UNINST_KEY}" "EstimatedSize" "$0"

  ; On a silent install (Setup.exe /S) the MUI Finish page's "run" checkbox
  ; never shows, so launch the app ourselves. (The in-app updater no longer runs
  ; the installer — it swaps clipo.exe directly — so this only covers a manual /S.)
  IfSilent 0 +2
    Exec '"$INSTDIR\${APP_EXE}"'
SectionEnd

Section "Uninstall"
  !insertmacro CloseRunning

  ; Program files.
  Delete "$INSTDIR\${APP_EXE}"
  Delete "$INSTDIR\ffmpeg.exe"
  Delete "$INSTDIR\Uninstall.exe"
  RMDir  "$INSTDIR"

  ; Shortcuts.
  Delete "$SMPROGRAMS\${APP_NAME}.lnk"
  Delete "$DESKTOP\${APP_NAME}.lnk"

  ; Undo the HKCU integrations the app may have created.
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "Clipo"
  DeleteRegValue HKCU "Software\Classes\.png\OpenWithProgids"  "Clipo.Image"
  DeleteRegValue HKCU "Software\Classes\.jpg\OpenWithProgids"  "Clipo.Image"
  DeleteRegValue HKCU "Software\Classes\.jpeg\OpenWithProgids" "Clipo.Image"
  DeleteRegValue HKCU "Software\Classes\.gif\OpenWithProgids"  "Clipo.Image"
  DeleteRegValue HKCU "Software\Classes\.webp\OpenWithProgids" "Clipo.Image"
  DeleteRegValue HKCU "Software\Classes\.bmp\OpenWithProgids"  "Clipo.Image"
  DeleteRegKey   HKCU "Software\Classes\Clipo.Image"

  ; Our own keys. User data (%APPDATA%\Clipo: settings + captures) is left intact.
  DeleteRegKey HKCU "${UNINST_KEY}"
  DeleteRegKey HKCU "Software\${APP_ID}"
SectionEnd
