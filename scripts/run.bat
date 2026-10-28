@echo off
rem Build and start Kwil from this checkout, to check the current state.
rem
rem   scripts\run.bat                 open the five small editor samples in the window UI
rem   scripts\run.bat some\file.txt   open a file
rem   scripts\run.bat --pull          update to the latest main first (fast-forward only)
rem   scripts\run.bat --release       optimized build (slower to build, the real speed)
rem   scripts\run.bat --big           open the 1 GB test log (generated on first use)
rem   scripts\run.bat --tui           terminal UI instead of the window
rem
rem Other options (for example --version, --help) are passed to kwil.
setlocal EnableDelayedExpansion
cd /d "%~dp0.."

set "PULL="
set "RELEASE="
set "BIG="
set "FILE="
set "KWIL_ARGS="

:parse
if "%~1"=="" goto parsed
if /i "%~1"=="--pull" (set "PULL=1" & shift & goto parse)
if /i "%~1"=="--release" (set "RELEASE=--release" & shift & goto parse)
if /i "%~1"=="--big" (set "BIG=1" & shift & goto parse)
set "ARG=%~1"
if "!ARG:~0,1!"=="-" (
    set "KWIL_ARGS=!KWIL_ARGS! %1"
) else (
    set FILE=!FILE! "%~1"
)
shift
goto parse
:parsed

if defined PULL (
    echo Updating to the latest main...
    git pull --ff-only origin main || (echo Pull failed. Fix the checkout first. & exit /b 1)
)

rem Use half of the CPU threads so the PC stays responsive while building.
set /a JOBS=%NUMBER_OF_PROCESSORS% / 2
if %JOBS% LSS 1 set JOBS=1
if %JOBS% GTR 8 set JOBS=8

if defined BIG (
    if not exist "fixtures\log-1g.log" (
        echo Generating the 1 GB test log once, about a minute...
        if not exist fixtures mkdir fixtures
        cargo run --release -j %JOBS% --quiet --manifest-path spike\textbench\Cargo.toml -- gen fixtures\log-1g.log 1024 || exit /b 1
    )
    set FILE="fixtures\log-1g.log"
)
if not defined FILE if "!KWIL_ARGS!"=="" set FILE="examples\smoke\example.log" "examples\smoke\example.json" "examples\smoke\Example.java" "examples\markdown\showcase.md" "examples\smoke\example.log.gz"

if defined KWIL_RUN_DRY_RUN (echo Files: !FILE! & echo Options: !KWIL_ARGS! & exit /b 0)

echo Building Kwil %RELEASE%...
cargo build %RELEASE% -j %JOBS% --quiet -p kwil || (echo Build failed. & exit /b 1)

set "KWIL_NO_UPDATE=1"
cargo run %RELEASE% -j %JOBS% --quiet -p kwil --bin kwil -- !KWIL_ARGS! !FILE!
exit /b %ERRORLEVEL%
