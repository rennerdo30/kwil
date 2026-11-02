; Reusable renner.dev per-user installer. MIT, copyright 2026 rennerdo30.
; Consumers provide product.nsh and the generated payload.nsh. No app-specific
; logo, palette, payload name or directory is baked into this template.
Unicode true
RequestExecutionLevel user
ManifestDPIAware true
SetCompressor /FINAL zlib
SetDatablockOptimize on
ShowInstDetails show
ShowUninstDetails show

!include "MUI2.nsh"
!include "LogicLib.nsh"
!include "FileFunc.nsh"
!include "Sections.nsh"
!include "product.nsh"
!include "payload.nsh"

Name "${APP_NAME}"
OutFile "${OUTPUT_FILE}"
InstallDir "$LOCALAPPDATA\Programs\${APP_FOLDER}"
InstallDirRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APP_ID}" "InstallLocation"
BrandingText "renner.dev"
VIProductVersion "${PE_VERSION}"
VIAddVersionKey /LANG=1033 "ProductName" "${APP_NAME}"
VIAddVersionKey /LANG=1033 "CompanyName" "renner.dev"
VIAddVersionKey /LANG=1033 "FileDescription" "${APP_NAME} Setup"
VIAddVersionKey /LANG=1033 "FileVersion" "${APP_VERSION}"
VIAddVersionKey /LANG=1033 "ProductVersion" "${APP_VERSION}"
VIAddVersionKey /LANG=1033 "LegalCopyright" "Copyright 2026 rennerdo30"

!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APP_ID}"
!define MUI_ICON "${APP_ICON}"
!define MUI_UNICON "${APP_ICON}"
!define MUI_BGCOLOR "${APP_BACKGROUND}"
!define MUI_TEXTCOLOR "${APP_FOREGROUND}"
!define MUI_DIRECTORYPAGE_BGCOLOR "${APP_SURFACE}"
!define MUI_COMPONENTSPAGE_SMALLDESC
!define MUI_INSTFILESPAGE_COLORS "${APP_FOREGROUND} ${APP_SURFACE}"
!define MUI_INSTFILESPAGE_PROGRESSBAR colored
!define MUI_WELCOMEFINISHPAGE_BITMAP "${APP_SIDEBAR}"
!define MUI_UNWELCOMEFINISHPAGE_BITMAP "${APP_SIDEBAR}"
!define MUI_HEADERIMAGE
!define MUI_HEADERIMAGE_RIGHT
!define MUI_HEADERIMAGE_BITMAP "${APP_HEADER}"
!define MUI_HEADERIMAGE_UNBITMAP "${APP_HEADER}"
!define MUI_ABORTWARNING
!define MUI_WELCOMEPAGE_TITLE "Install ${APP_NAME}"
!define MUI_WELCOMEPAGE_TEXT "${APP_TAGLINE}$\r$\n$\r$\n${APP_VERSION} · ${APP_ARCH_LABEL}$\r$\n$\r$\nInstalled for your account. Your settings and documents stay yours."
!define MUI_FINISHPAGE_TITLE "${APP_NAME} is ready"
!define MUI_FINISHPAGE_RUN "$INSTDIR\${APP_EXE}"
!define MUI_FINISHPAGE_RUN_NOTCHECKED
!define MUI_CUSTOMFUNCTION_GUIINIT RD_GuiInit
!define MUI_PAGE_CUSTOMFUNCTION_SHOW RD_PageShow
!insertmacro MUI_PAGE_WELCOME
!define MUI_PAGE_CUSTOMFUNCTION_SHOW RD_PageShow
!insertmacro MUI_PAGE_COMPONENTS
!define MUI_PAGE_CUSTOMFUNCTION_SHOW RD_PageShow
!insertmacro MUI_PAGE_DIRECTORY
!define MUI_PAGE_CUSTOMFUNCTION_SHOW RD_PageShow
!define MUI_PAGE_CUSTOMFUNCTION_LEAVE RD_InstallLeave
!insertmacro MUI_PAGE_INSTFILES
!define MUI_PAGE_CUSTOMFUNCTION_SHOW RD_PageShow
!insertmacro MUI_PAGE_FINISH
!define MUI_PAGE_CUSTOMFUNCTION_SHOW un.RD_PageShow
!insertmacro MUI_UNPAGE_CONFIRM
!define MUI_PAGE_CUSTOMFUNCTION_SHOW un.RD_PageShow
!insertmacro MUI_UNPAGE_INSTFILES
!define MUI_PAGE_CUSTOMFUNCTION_SHOW un.RD_PageShow
!insertmacro MUI_UNPAGE_FINISH
!insertmacro MUI_LANGUAGE "English"

Var Existing
Var Stage
Var Backup
Var Problem
Var SetupMutex
Var MenuChoice
Var DesktopChoice

Section "${APP_NAME}" Main
  SectionIn RO
  Call RD_Install
SectionEnd
Section "Start menu shortcut" StartMenu
  ; Handled in RD_Install, after checking every selected destination.
SectionEnd
Section /o "Desktop shortcut" Desktop
SectionEnd

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${Main} "${APP_TAGLINE}"
  !insertmacro MUI_DESCRIPTION_TEXT ${StartMenu} "Find ${APP_NAME} in your Start menu."
  !insertmacro MUI_DESCRIPTION_TEXT ${Desktop} "Add a shortcut to your desktop."
!insertmacro MUI_FUNCTION_DESCRIPTION_END

Function RD_InstallLeave
  ${If} $Problem != ""
    Abort
  ${EndIf}
FunctionEnd

!macro RD_Common Prefix
Function ${Prefix}RD_CloseMutex
  ${If} $SetupMutex != 0
    System::Call 'kernel32::CloseHandle(p $SetupMutex)'
    StrCpy $SetupMutex 0
  ${EndIf}
FunctionEnd

Function ${Prefix}RD_CheckPath
  Exch $0
  Push $1
  Push $2
  ${DoWhile} $0 != ""
    System::Call 'kernel32::GetFileAttributesW(w r0)i.r1'
    ${If} $1 != -1
      IntOp $2 $1 & 0x400
      ${If} $2 != 0
        StrCpy $Problem "A linked path cannot be used for setup: $0"
        ${ExitDo}
      ${EndIf}
    ${EndIf}
    ${GetParent} "$0" $1
    ${If} $1 == $0
      ${ExitDo}
    ${EndIf}
    StrCpy $0 $1
  ${Loop}
  Pop $2
  Pop $1
  Pop $0
FunctionEnd

Function ${Prefix}RD_CheckFile
  Exch $0
  Push $1
  Push $2
  Push $0
  Call ${Prefix}RD_CheckPath
  ${If} $Problem == ""
    System::Call 'kernel32::GetFileAttributesW(w r0)i.r1'
    ${If} $1 != -1
      IntOp $2 $1 & 0x10
      ${If} $2 != 0
        StrCpy $Problem "A folder occupies an application file: $0"
      ${ElseIf} $Existing != 1
        StrCpy $Problem "Setup will not replace a file from another installation: $0"
      ${Else}
        ; A running image or another file owner must release it themselves.
        ; Never terminate a process by executable name or schedule a reboot.
        System::Call 'kernel32::CreateFileW(w r0,i 0x80000000,i 0,p 0,i 3,i 0,p 0)p.r2'
        ${If} $2 == -1
          StrCpy $Problem "Close ${APP_NAME} and retry. This file is in use or inaccessible: $0"
        ${Else}
          System::Call 'kernel32::CloseHandle(p r2)'
        ${EndIf}
      ${EndIf}
    ${EndIf}
  ${EndIf}
  Pop $2
  Pop $1
  Pop $0
FunctionEnd

Function ${Prefix}RD_ValidateDirectory
  GetFullPathName $INSTDIR "$INSTDIR"
  ${GetRoot} "$INSTDIR" $0
  ${If} $INSTDIR == ""
  ${OrIf} $INSTDIR == $0
    StrCpy $Problem "Choose an application folder, not a drive root."
    Return
  ${EndIf}
  Push "$INSTDIR"
  Call ${Prefix}RD_CheckPath
  ${If} $Problem != ""
    Return
  ${EndIf}
  StrCpy $Existing 0
  IfFileExists "$INSTDIR\.renner-install.ini" 0 rd_no_owner_${Prefix}
    ReadINIStr $0 "$INSTDIR\.renner-install.ini" "Installation" "AppId"
    ReadINIStr $1 "$INSTDIR\.renner-install.ini" "Installation" "Architecture"
    ReadINIStr $2 "$INSTDIR\.renner-install.ini" "Installation" "Location"
    ReadINIStr $3 "$INSTDIR\.renner-install.ini" "Installation" "Layout"
    ${If} $0 != "${APP_ID}"
    ${OrIf} $1 != "${APP_ARCH}"
    ${OrIf} $2 != $INSTDIR
    ${OrIf} $3 != "${APP_LAYOUT}"
      StrCpy $Problem "This folder belongs to a different installation. Uninstall that version first."
      Return
    ${EndIf}
    StrCpy $Existing 1
  rd_no_owner_${Prefix}:
  !ifmacrodef RD_VALIDATE_LEGACY
    !insertmacro RD_VALIDATE_LEGACY "${Prefix}"
  !endif
FunctionEnd

Function ${Prefix}RD_ThemeChildren
  Exch $0
  Push $1
  Push $2
  SetCtlColors $0 "${APP_FOREGROUND}" "${APP_BACKGROUND}"
  System::Call 'user32::GetWindow(p r0,i 5)p.r1'
  ${DoWhile} $1 != 0
    System::Call 'user32::GetClassNameW(p r1,w .r2,i ${NSIS_MAX_STRLEN})'
    ${If} $2 != "Button"
      SetCtlColors $1 "${APP_FOREGROUND}" "${APP_BACKGROUND}"
    ${EndIf}
    System::Call 'user32::GetWindow(p r1,i 2)p.r1'
  ${Loop}
  Pop $2
  Pop $1
  Pop $0
FunctionEnd

Function ${Prefix}RD_PageShow
  Push $HWNDPARENT
  Call ${Prefix}RD_ThemeChildren
  FindWindow $0 "#32770" "" $HWNDPARENT
  ${If} $0 != 0
    Push $0
    Call ${Prefix}RD_ThemeChildren
  ${EndIf}
FunctionEnd
!macroend
!insertmacro RD_Common ""
!insertmacro RD_Common "un."

Function RD_GuiInit
  System::Call 'dwmapi::DwmSetWindowAttribute(p $HWNDPARENT,i 20,*i 1,i 4)'
  Call RD_PageShow
FunctionEnd

Function .onInit
  SetShellVarContext current
  SetRegView 64
  StrCpy $SetupMutex 0
  System::Call 'kernel32::CreateMutexW(p 0,i 0,w "Local\renner.dev.${APP_ID}.setup")p.r0 ?e'
  Pop $1
  StrCpy $SetupMutex $0
  ${If} $0 == 0
  ${OrIf} $1 == 183
    SetErrorLevel 2
    Abort "Another ${APP_NAME} setup is running."
  ${EndIf}
  System::Alloc 64
  Pop $0
  System::Call 'kernel32::GetNativeSystemInfo(p r0)'
  System::Call '*$0(&i2 .r1)'
  System::Free $0
  ${If} $1 != ${APP_NATIVE_ARCH}
    Call RD_CloseMutex
    SetErrorLevel 2
    Abort "Use the ${APP_NAME} installer matching this Windows architecture."
  ${EndIf}
FunctionEnd

Function RD_Install
  StrCpy $Problem ""
  Call RD_ValidateDirectory
  !insertmacro RD_CheckPayload ""
  Push "$INSTDIR\Uninstall.exe"
  Call RD_CheckFile
  ${If} $Problem != ""
    Goto rd_install_failed
  ${EndIf}
  StrCpy $MenuChoice 0
  StrCpy $DesktopChoice 0
  ${If} ${SectionIsSelected} ${StartMenu}
    StrCpy $MenuChoice 1
  ${EndIf}
  ${If} ${SectionIsSelected} ${Desktop}
    StrCpy $DesktopChoice 1
  ${EndIf}
  System::Call 'kernel32::GetCurrentProcessId()i.r0'
  StrCpy $Stage "$INSTDIR\.renner-stage-$0"
  StrCpy $Backup "$INSTDIR\.renner-backup-$0"
  IfFileExists "$Stage\*.*" rd_stage_exists
  IfFileExists "$Backup\*.*" rd_stage_exists
  ClearErrors
  CreateDirectory "$Stage"
  CreateDirectory "$Backup"
  IfErrors rd_stage_exists
  !insertmacro RD_StagePayload
  WriteUninstaller "$Stage\Uninstall.exe"
  WriteINIStr "$Stage\.renner-install.ini" "Installation" "AppId" "${APP_ID}"
  WriteINIStr "$Stage\.renner-install.ini" "Installation" "Architecture" "${APP_ARCH}"
  WriteINIStr "$Stage\.renner-install.ini" "Installation" "Location" "$INSTDIR"
  WriteINIStr "$Stage\.renner-install.ini" "Installation" "Layout" "${APP_LAYOUT}"
  WriteINIStr "$Stage\.renner-install.ini" "Installation" "Version" "${APP_VERSION}"
  WriteINIStr "$Stage\.renner-install.ini" "Installation" "StartMenu" "$MenuChoice"
  WriteINIStr "$Stage\.renner-install.ini" "Installation" "Desktop" "$DesktopChoice"
  IfErrors rd_stage_failed
  !insertmacro RD_BackupPayload
  !insertmacro RD_CommitPayload
  ; Registration is per-user and the installer never changes PATH or defaults.
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayName" "${APP_NAME}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayVersion" "${APP_VERSION}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "Publisher" "renner.dev"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayIcon" "$INSTDIR\${APP_EXE}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "URLInfoAbout" "${APP_URL}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "UninstallString" '$\"$INSTDIR\Uninstall.exe$\"'
  WriteRegStr HKCU "${UNINSTALL_KEY}" "QuietUninstallString" '$\"$INSTDIR\Uninstall.exe$\" /S'
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoRepair" 1
  ${If} $MenuChoice == 1
    CreateShortcut "$SMPROGRAMS\${APP_NAME}.lnk" "$INSTDIR\${APP_EXE}"
  ${EndIf}
  ${If} $DesktopChoice == 1
    CreateShortcut "$DESKTOP\${APP_NAME}.lnk" "$INSTDIR\${APP_EXE}"
  ${EndIf}
  IfErrors rd_transaction_failed
  !insertmacro RD_CleanBackup
  !insertmacro RD_CleanStage
  SetErrorLevel 0
  Return
  rd_stage_exists:
    StrCpy $Problem "Setup staging is unavailable. Existing files were retained."
    Goto rd_install_failed
  rd_stage_failed:
    StrCpy $Problem "Setup could not prepare the payload. Existing files were retained."
    Goto rd_install_failed
  rd_transaction_failed:
    !insertmacro RD_RollbackPayload
    StrCpy $Problem "Setup could not complete. Original files and rollback evidence were retained."
  rd_install_failed:
    DetailPrint "$Problem"
    IfSilent +2
      MessageBox MB_ICONSTOP|MB_OK "$Problem"
    Call RD_CloseMutex
    SetErrorLevel 2
    Abort
FunctionEnd

Function .onInstSuccess
  Call RD_CloseMutex
FunctionEnd
Function .onInstFailed
  Call RD_CloseMutex
FunctionEnd
Function .onGUIEnd
  Call RD_CloseMutex
FunctionEnd

Function un.onInit
  SetShellVarContext current
  SetRegView 64
  StrCpy $Problem ""
  StrCpy $SetupMutex 0
  Call un.RD_ValidateDirectory
  ${If} $Existing != 1
    StrCpy $Problem "The original installation record is missing. No files were removed."
  ${EndIf}
  ${If} $Problem != ""
    SetErrorLevel 2
    Abort "$Problem"
  ${EndIf}
FunctionEnd

Section "Uninstall"
  !insertmacro RD_CheckPayload "un."
  ${If} $Problem != ""
    DetailPrint "$Problem"
    IfSilent +2
      MessageBox MB_ICONSTOP|MB_OK "$Problem"
    SetErrorLevel 2
    Abort
  ${EndIf}
  ReadINIStr $MenuChoice "$INSTDIR\.renner-install.ini" "Installation" "StartMenu"
  ReadINIStr $DesktopChoice "$INSTDIR\.renner-install.ini" "Installation" "Desktop"
  !ifmacrodef RD_BEFORE_UNINSTALL
    !insertmacro RD_BEFORE_UNINSTALL
  !endif
  !insertmacro RD_RemovePayload
  ${If} $MenuChoice == 1
    Delete "$SMPROGRAMS\${APP_NAME}.lnk"
  ${EndIf}
  ${If} $DesktopChoice == 1
    Delete "$DESKTOP\${APP_NAME}.lnk"
  ${EndIf}
  Delete "$INSTDIR\Uninstall.exe"
  Delete "$INSTDIR\.renner-install.ini"
  !insertmacro RD_RemoveRegistry
  ; Nonrecursive: unknown files, portable settings and other user data survive.
  RMDir "$INSTDIR"
SectionEnd
