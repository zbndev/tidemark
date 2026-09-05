; Tidemark per-user NSIS installer (todo 21).
;
; Per-user, never elevated: everything lands under HKCU and
; %LOCALAPPDATA%\Programs\tidemark (context none/elevated=false per the plan).
; The Start-menu shortcut carries the System.AppUserModel.ID property
; (io.github.zbndev.Tidemark) — REQUIRED for todo 16's toast transport to
; attribute notifications. The uninstaller removes the todo-14 Scheduled Task
; (TidemarkDaemon), the HKCU Run value (Tidemark), the AUMID registry key,
; the shortcut and every installed file.
;
; Build (from data/packaging/windows/, after stage-gtk-runtime.sh and a
; `cargo build --release -p tidemark -p tidemarkd`):
;   makensis /DSRC_DIR=..\..\target\release /DGTK_DIR=nsis-staging\gtk installer.nsi

Unicode true
ManifestDPIAware true

!define APP_ID "io.github.zbndev.Tidemark"
!define APP_NAME "Tidemark"
!define UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APP_ID}"
!define AUMID_KEY "Software\Classes\AppUserModelId\${APP_ID}"
!define DAEMON_TASK "TidemarkDaemon"
!define RUN_VALUE "Tidemark"

!ifndef VERSION
  !define VERSION "0.3.1"
!endif
!ifndef SRC_DIR
  !define SRC_DIR "..\..\target\release"
!endif
!ifndef GTK_DIR
  !define GTK_DIR "nsis-staging\gtk"
!endif
!ifndef OUT_FILE
  !define OUT_FILE "tidemark-installer.exe"
!endif

Name "${APP_NAME}"
OutFile "${OUT_FILE}"
InstallDir "$LOCALAPPDATA\Programs\tidemark"
InstallDirRegKey HKCU "${UNINST_KEY}" "InstallLocation"
RequestExecutionLevel user
SetCompressor /SOLID lzma

!include "MUI2.nsh"
!include "LogicLib.nsh"

!define MUI_ABORTWARNING
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!define MUI_TEXT_FINISH_RUN_TEXT "Launch ${APP_NAME}"
!define MUI_FINISHPAGE_RUN "$INSTDIR\tidemark.exe"
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

; The System.AppUserModel.ID shortcut property is set by set-aumid.ps1,
; embedded into $PLUGINSDIR at install time (see the Install section). NSIS's
; CreateShortcut cannot write shell properties, and the System plugin's raw
; COM path silently no-ops on Save, so a small proven helper is used instead.

Section "Install"
  SetShellVarContext current
  SetOutPath "$INSTDIR"

  ; A previous daemon or UI would hold the files open; stop them quietly.
  nsExec::Exec 'taskkill /IM tidemark.exe /IM tidemarkd.exe /F'

  File "${SRC_DIR}\tidemark.exe"
  File "${SRC_DIR}\tidemarkd.exe"
  File /r "${GTK_DIR}\*.*"

  ; Start-menu shortcut with the toast-identity property. The property is
  ; REQUIRED (todo 16); if the helper cannot set it, the install is aborted
  ; rather than shipping a silent toast-identity breakage. The shortcut's icon
  ; is the staged tidemark.ico: the exe carries no embedded Win32 icon, so
  ; without it Start, the taskbar button and Alt-Tab all fall back to generic.
  InitPluginsDir
  File "/oname=$PLUGINSDIR\set-aumid.ps1" "set-aumid.ps1"
  CreateDirectory "$SMPROGRAMS\${APP_NAME}"
  CreateShortcut "$SMPROGRAMS\${APP_NAME}\${APP_NAME}.lnk" "$INSTDIR\tidemark.exe" "" "$INSTDIR\share\tidemark.ico"
  nsExec::ExecToLog 'powershell -NoProfile -ExecutionPolicy Bypass -File "$PLUGINSDIR\set-aumid.ps1" -Lnk "$SMPROGRAMS\${APP_NAME}\${APP_NAME}.lnk" -Aumid "${APP_ID}"'
  Pop $0
  ${If} $0 != 0
    DetailPrint "could not set the AppUserModelID on the shortcut (error $0)"
    Abort "The Start-menu shortcut's AppUserModelID property could not be set; toasts would be misattributed. Installation aborted."
  ${EndIf}

  ; The AUMID's display name for the toast platform. Also upgrades the
  ; "Tidemark (dev)" residue the todo-16 dev helper may have left behind.
  WriteRegStr HKCU "${AUMID_KEY}" "DisplayName" "${APP_NAME}"

  ; Per-user uninstall registration.
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayName" "${APP_NAME}"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINST_KEY}" "Publisher" "zbndev"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayIcon" "$INSTDIR\share\tidemark.ico,0"
  WriteRegStr HKCU "${UNINST_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINST_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKCU "${UNINST_KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoRepair" 1
  WriteUninstaller "$INSTDIR\uninstall.exe"
SectionEnd

Section "Uninstall"
  SetShellVarContext current

  ; Stop the todo-14 lifecycle artifacts and any running processes first, so
  ; "uninstall while running" does not orphan files or re-spawn the task.
  nsExec::Exec 'taskkill /IM tidemark.exe /IM tidemarkd.exe /F'
  nsExec::Exec 'schtasks /Delete /TN "${DAEMON_TASK}" /F'
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "${RUN_VALUE}"
  DeleteRegKey HKCU "${AUMID_KEY}"

  RMDir /r "$SMPROGRAMS\${APP_NAME}"
  RMDir /r "$INSTDIR"
  DeleteRegKey HKCU "${UNINST_KEY}"
SectionEnd
