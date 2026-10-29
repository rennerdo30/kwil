; Kwil per-user Windows installer (NSIS 3). Built by .github/workflows/release.yml:
;   makensis /DVERSION=0.1.0-beta.3 /DVI_VERSION=0.1.0.0 /DARCH=x64 /DSRC=<dir> /DOUTFILE=<file> installer\kwil.nsi
; SRC holds kwil.exe, kwil.com, LICENSE.txt and THIRD-PARTY-NOTICES.txt.
;
; Installs to %LOCALAPPDATA%\Programs\Kwil without admin rights, with a Start
; menu shortcut and an Installed apps entry. No PATH or default-app changes.
; Kwil's updater later replaces kwil.exe/kwil.com in place and keeps the
; entry's DisplayVersion current (crates/update/src/installed.rs). Uninstall
; first runs "Remove all" for file types, then removes only installed files;
; settings in %APPDATA%\kwil stay.

!macro REQUIRE name
  !ifndef ${name}
    !error "Pass /D${name}=..."
  !endif
!macroend
!insertmacro REQUIRE VERSION
!insertmacro REQUIRE VI_VERSION
!insertmacro REQUIRE ARCH
!insertmacro REQUIRE SRC
!insertmacro REQUIRE OUTFILE

Unicode true
ManifestDPIAware true
RequestExecutionLevel user
SetCompressor /SOLID lzma

!include MUI2.nsh
!include LogicLib.nsh
!include x64.nsh
!include FileFunc.nsh

!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\Kwil"

Name "Kwil"
OutFile "${OUTFILE}"
InstallDir "$LOCALAPPDATA\Programs\Kwil"
InstallDirRegKey HKCU "${UNINSTALL_KEY}" "InstallLocation"
BrandingText "Kwil ${VERSION}"

VIProductVersion "${VI_VERSION}"
VIAddVersionKey "ProductName" "Kwil"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "FileDescription" "Kwil ${VERSION} setup"
VIAddVersionKey "LegalCopyright" "MIT License"

!define MUI_ICON "..\assets\logo\kwil.ico"
!define MUI_UNICON "..\assets\logo\kwil.ico"
!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN "$INSTDIR\kwil.exe"
!define MUI_FINISHPAGE_RUN_TEXT "Start Kwil"

!insertmacro MUI_PAGE_LICENSE "${SRC}\LICENSE.txt"
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

; A running kwil.exe or kwil.com cannot be replaced or removed. Ask the user
; to close Kwil; silent runs give up instead of waiting.
!macro WAIT_FOR_KWIL un
Function ${un}WaitForKwil
  retry:
  ${If} ${FileExists} "$INSTDIR\kwil.exe"
    ClearErrors
    FileOpen $0 "$INSTDIR\kwil.exe" a
    IfErrors running
    FileClose $0
  ${EndIf}
  ${If} ${FileExists} "$INSTDIR\kwil.com"
    ClearErrors
    FileOpen $0 "$INSTDIR\kwil.com" a
    IfErrors running
    FileClose $0
  ${EndIf}
  Return
  running:
  MessageBox MB_RETRYCANCEL|MB_ICONEXCLAMATION "Kwil is running. Close every Kwil window, then choose Retry." /SD IDCANCEL IDRETRY retry
  Abort "Kwil is still running."
FunctionEnd
!macroend
!insertmacro WAIT_FOR_KWIL ""
!insertmacro WAIT_FOR_KWIL "un."

Function .onInit
  !if "${ARCH}" == "arm64"
    ${IfNot} ${IsNativeARM64}
      MessageBox MB_ICONSTOP "This is the Kwil installer for Windows on ARM. Use the x64 installer on this PC." /SD IDOK
      Abort
    ${EndIf}
  !else
    ${IfNot} ${RunningX64}
      MessageBox MB_ICONSTOP "Kwil needs 64-bit Windows." /SD IDOK
      Abort
    ${EndIf}
  !endif
FunctionEnd

Section "Kwil"
  Call WaitForKwil
  SetOutPath "$INSTDIR"
  File "${SRC}\kwil.exe"
  File "${SRC}\kwil.com"
  File "${SRC}\LICENSE.txt"
  File "${SRC}\THIRD-PARTY-NOTICES.txt"
  WriteUninstaller "$INSTDIR\uninstall.exe"
  CreateShortcut "$SMPROGRAMS\Kwil.lnk" "$INSTDIR\kwil.exe"

  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayName" "Kwil"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "Publisher" "rennerdo30"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayIcon" "$INSTDIR\kwil.exe,0"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKCU "${UNINSTALL_KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegStr HKCU "${UNINSTALL_KEY}" "URLInfoAbout" "https://github.com/rennerdo30/kwil"
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoRepair" 1
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "EstimatedSize" "$0"
SectionEnd

Section "Uninstall"
  Call un.WaitForKwil
  ; Kwil's "Open with" entries point into this folder: undo them first, exactly
  ; like "Remove all" on the file-types page. A failure is logged, not fatal.
  ${If} ${FileExists} "$INSTDIR\kwil.com"
    nsExec::ExecToLog '"$INSTDIR\kwil.com" --unregister-file-types'
    Pop $0
    DetailPrint "kwil --unregister-file-types: $0"
  ${EndIf}

  Delete "$INSTDIR\kwil.exe"
  Delete "$INSTDIR\kwil.com"
  Delete "$INSTDIR\LICENSE.txt"
  Delete "$INSTDIR\THIRD-PARTY-NOTICES.txt"
  ; Updater state (crates/update/src/transaction.rs).
  Delete "$INSTDIR\.kwil-update-journal"
  Delete "$INSTDIR\.kwil-update.lock"
  FindFirst $0 $1 "$INSTDIR\.kwil-stage-*"
  ${DoWhile} $1 != ""
    RMDir /r "$INSTDIR\$1"
    FindNext $0 $1
  ${Loop}
  FindClose $0
  Delete "$INSTDIR\uninstall.exe"
  ; Only when empty: files the user put here stay.
  RMDir "$INSTDIR"

  Delete "$SMPROGRAMS\Kwil.lnk"
  DeleteRegKey HKCU "${UNINSTALL_KEY}"
SectionEnd
