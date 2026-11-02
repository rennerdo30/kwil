; Preserve the shipped per-user installation identity and destination.
!define APP_ID "Kwil"
!define APP_NAME "Kwil"
!define APP_FOLDER "Kwil"
!define APP_EXE "kwil.exe"
!define APP_TAGLINE "A fast, plain text editor."
!define APP_URL "https://rennerdo30.github.io/kwil/"
!define APP_VERSION "${VERSION}"
!define PE_VERSION "${VI_VERSION}"
!define APP_ARCH "${ARCH}"
!define APP_LAYOUT "kwil-exe-com-notices-v1"
!if "${ARCH}" == "arm64"
  !define APP_ARCH_LABEL "Windows ARM64"
  !define APP_NATIVE_ARCH 12
!else if "${ARCH}" == "x64"
  !define APP_ARCH_LABEL "Windows x64"
  !define APP_NATIVE_ARCH 9
!else
  !error "Unsupported Kwil architecture"
!endif
!define OUTPUT_FILE "${OUTFILE}"
!define APP_ICON "..\assets\logo\kwil.ico"
!define APP_SIDEBAR "art\sidebar.bmp"
!define APP_HEADER "art\header.bmp"
; website/style.css night, night-ink, night-2 and ice palette.
!define APP_BACKGROUND "0a101c"
!define APP_FOREGROUND "e9f4ff"
!define APP_SURFACE "16243d"
!define APP_ACCENT "8fe3ff"

; Pre-template Kwil setups have no .renner-install.ini. Adopt only the
; registered Kwil folder with its complete installed executable pair and
; uninstaller. The shared checks still reject linked paths and locked files.
; Never treat a malformed existing ownership marker as a legacy install.
!macro RD_VALIDATE_LEGACY Prefix
  !if "${Prefix}" == ""
    IfFileExists "$INSTDIR\.renner-install.ini" kwil_legacy_end
    ReadRegStr $0 HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Kwil" "InstallLocation"
    ReadRegStr $1 HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Kwil" "DisplayName"
    ReadRegStr $2 HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Kwil" "Publisher"
    ReadRegStr $3 HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Kwil" "UninstallString"
    ${If} $0 == $INSTDIR
    ${AndIf} $1 == "Kwil"
    ${AndIf} $2 == "rennerdo30"
    ${AndIf} $3 == '"$INSTDIR\uninstall.exe"'
    ${AndIf} ${FileExists} "$INSTDIR\kwil.exe"
    ${AndIf} ${FileExists} "$INSTDIR\kwil.com"
    ${AndIf} ${FileExists} "$INSTDIR\uninstall.exe"
      StrCpy $Existing 1
    ${EndIf}
    kwil_legacy_end:
  !endif
!macroend

; Preserve Kwil's existing file-type cleanup. The command can refuse the
; elevated hosted-runner profile; retain the existing log-and-continue policy.
!macro RD_BEFORE_UNINSTALL
  nsExec::ExecToLog /TIMEOUT=10000 '"$INSTDIR\kwil.com" --unregister-file-types'
  Pop $0
  DetailPrint "kwil --unregister-file-types: $0"
  Delete "$INSTDIR\.kwil-update-journal"
  Delete "$INSTDIR\.kwil-update.lock"
!macroend
