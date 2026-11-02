; Kwil payload adapter for the shared renner.dev template.
!macro RD_CheckPayload Prefix
  Push "$INSTDIR\kwil.exe"
  Call ${Prefix}RD_CheckFile
  Push "$INSTDIR\kwil.com"
  Call ${Prefix}RD_CheckFile
  Push "$INSTDIR\LICENSE.txt"
  Call ${Prefix}RD_CheckFile
  Push "$INSTDIR\THIRD-PARTY-NOTICES.txt"
  Call ${Prefix}RD_CheckFile
!macroend

!macro RD_StagePayload
  SetOutPath "$Stage"
  File "${SRC}\kwil.exe"
  IfErrors rd_stage_failed
  File "${SRC}\kwil.com"
  IfErrors rd_stage_failed
  File "${SRC}\LICENSE.txt"
  IfErrors rd_stage_failed
  File "${SRC}\THIRD-PARTY-NOTICES.txt"
  IfErrors rd_stage_failed
!macroend

!macro RD_BackupPayload
  IfFileExists "$INSTDIR\kwil.exe" 0 +3
    Rename "$INSTDIR\kwil.exe" "$Backup\kwil.exe"
    IfErrors rd_transaction_failed
  IfFileExists "$INSTDIR\kwil.com" 0 +3
    Rename "$INSTDIR\kwil.com" "$Backup\kwil.com"
    IfErrors rd_transaction_failed
  IfFileExists "$INSTDIR\LICENSE.txt" 0 +3
    Rename "$INSTDIR\LICENSE.txt" "$Backup\LICENSE.txt"
    IfErrors rd_transaction_failed
  IfFileExists "$INSTDIR\THIRD-PARTY-NOTICES.txt" 0 +3
    Rename "$INSTDIR\THIRD-PARTY-NOTICES.txt" "$Backup\THIRD-PARTY-NOTICES.txt"
    IfErrors rd_transaction_failed
  IfFileExists "$INSTDIR\Uninstall.exe" 0 +3
    Rename "$INSTDIR\Uninstall.exe" "$Backup\Uninstall.exe"
    IfErrors rd_transaction_failed
  IfFileExists "$INSTDIR\.renner-install.ini" 0 +3
    Rename "$INSTDIR\.renner-install.ini" "$Backup\.renner-install.ini"
    IfErrors rd_transaction_failed
!macroend

!macro RD_CommitPayload
  Rename "$Stage\kwil.exe" "$INSTDIR\kwil.exe"
  IfErrors rd_transaction_failed
  Rename "$Stage\kwil.com" "$INSTDIR\kwil.com"
  IfErrors rd_transaction_failed
  Rename "$Stage\LICENSE.txt" "$INSTDIR\LICENSE.txt"
  IfErrors rd_transaction_failed
  Rename "$Stage\THIRD-PARTY-NOTICES.txt" "$INSTDIR\THIRD-PARTY-NOTICES.txt"
  IfErrors rd_transaction_failed
  Rename "$Stage\Uninstall.exe" "$INSTDIR\Uninstall.exe"
  IfErrors rd_transaction_failed
  Rename "$Stage\.renner-install.ini" "$INSTDIR\.renner-install.ini"
  IfErrors rd_transaction_failed
!macroend

!macro RD_RollbackPayload
  IfFileExists "$Backup\kwil.exe" 0 +3
    Delete "$INSTDIR\kwil.exe"
    Rename "$Backup\kwil.exe" "$INSTDIR\kwil.exe"
  IfFileExists "$Backup\kwil.com" 0 +3
    Delete "$INSTDIR\kwil.com"
    Rename "$Backup\kwil.com" "$INSTDIR\kwil.com"
  IfFileExists "$Backup\LICENSE.txt" 0 +3
    Delete "$INSTDIR\LICENSE.txt"
    Rename "$Backup\LICENSE.txt" "$INSTDIR\LICENSE.txt"
  IfFileExists "$Backup\THIRD-PARTY-NOTICES.txt" 0 +3
    Delete "$INSTDIR\THIRD-PARTY-NOTICES.txt"
    Rename "$Backup\THIRD-PARTY-NOTICES.txt" "$INSTDIR\THIRD-PARTY-NOTICES.txt"
  IfFileExists "$Backup\Uninstall.exe" 0 +3
    Delete "$INSTDIR\Uninstall.exe"
    Rename "$Backup\Uninstall.exe" "$INSTDIR\Uninstall.exe"
  IfFileExists "$Backup\.renner-install.ini" 0 +3
    Delete "$INSTDIR\.renner-install.ini"
    Rename "$Backup\.renner-install.ini" "$INSTDIR\.renner-install.ini"
!macroend

!macro RD_CleanBackup
  Delete "$Backup\kwil.exe"
  Delete "$Backup\kwil.com"
  Delete "$Backup\LICENSE.txt"
  Delete "$Backup\THIRD-PARTY-NOTICES.txt"
  Delete "$Backup\Uninstall.exe"
  Delete "$Backup\.renner-install.ini"
  RMDir "$Backup"
!macroend

!macro RD_CleanStage
  ; File extraction made Stage the process working directory. Release it
  ; before removing the empty directory on Windows.
  SetOutPath "$TEMP"
  Delete "$Stage\kwil.exe"
  Delete "$Stage\kwil.com"
  Delete "$Stage\LICENSE.txt"
  Delete "$Stage\THIRD-PARTY-NOTICES.txt"
  Delete "$Stage\Uninstall.exe"
  Delete "$Stage\.renner-install.ini"
  RMDir "$Stage"
!macroend

!macro RD_RemovePayload
  Delete "$INSTDIR\kwil.exe"
  Delete "$INSTDIR\kwil.com"
  Delete "$INSTDIR\LICENSE.txt"
  Delete "$INSTDIR\THIRD-PARTY-NOTICES.txt"
!macroend

!macro RD_RemoveRegistry
  DeleteRegValue HKCU "${UNINSTALL_KEY}" "DisplayName"
  DeleteRegValue HKCU "${UNINSTALL_KEY}" "DisplayVersion"
  DeleteRegValue HKCU "${UNINSTALL_KEY}" "Publisher"
  DeleteRegValue HKCU "${UNINSTALL_KEY}" "DisplayIcon"
  DeleteRegValue HKCU "${UNINSTALL_KEY}" "InstallLocation"
  DeleteRegValue HKCU "${UNINSTALL_KEY}" "URLInfoAbout"
  DeleteRegValue HKCU "${UNINSTALL_KEY}" "UninstallString"
  DeleteRegValue HKCU "${UNINSTALL_KEY}" "QuietUninstallString"
  DeleteRegValue HKCU "${UNINSTALL_KEY}" "NoModify"
  DeleteRegValue HKCU "${UNINSTALL_KEY}" "NoRepair"
  DeleteRegValue HKCU "${UNINSTALL_KEY}" "EstimatedSize"
  DeleteRegKey /ifempty HKCU "${UNINSTALL_KEY}"
!macroend
