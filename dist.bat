@echo off
setlocal DisableDelayedExpansion
where.exe pwsh.exe >nul 2>&1
if errorlevel 1 goto :missing
pwsh.exe -NoLogo -NoProfile -File "%~dp0scripts\dist.ps1" %*
set "kwil_dist_exit=%ERRORLEVEL%"
if not "%kwil_dist_exit%"=="0" goto :failed
exit /b 0
:missing
echo ERROR: PowerShell 7 ^(pwsh.exe^) is required on PATH. No tools were installed. 1>&2
set "kwil_dist_exit=1"
:failed
echo Kwil distribution failed. Review the error and any retained artifact paths above. 1>&2
rem Bounded delay keeps Explorer errors readable. CI may set KWIL_DIST_NO_PAUSE=1.
if "%KWIL_DIST_NO_PAUSE%"=="1" goto :done
echo This window will remain open for up to 15 seconds. Press a key to continue.
timeout.exe /t 15 >nul 2>&1
:done
exit /b %kwil_dist_exit%
