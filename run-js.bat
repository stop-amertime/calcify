@echo off
REM run-js.bat — Run DOS programs through JS8086 reference emulator
REM
REM   run-js.bat              Interactive menu
REM   run-js.bat <program>    Run a specific program directly
REM
REM Builds the disk image (via mkfat12), then runs ref-dos.mjs.
REM Useful for debugging: passes all extra args to ref-dos.mjs.
REM
REM Example:
REM   run-js.bat                         Interactive pick
REM   run-js.bat programs\bootle.com     Run bootle directly
REM   run-js.bat programs\bootle.com --ticks=50000 --int-trace

setlocal enabledelayedexpansion
cd /d "%~dp0"

set CSSDIR=..\CSS-DOS
set KERNEL=%CSSDIR%\dos\bin\kernel.sys
set MKFAT12=%CSSDIR%\tools\mkfat12.mjs
set DISKIMG=%CSSDIR%\dos\disk.img
set CONFIGSYS=%CSSDIR%\dos\config.sys
set COMMAND_COM=%CSSDIR%\dos\bin\command.com
set REFDOS=tools\ref-dos.mjs
set PROGDIR=programs

if not exist "%PROGDIR%" mkdir "%PROGDIR%"

REM --- Web mode: run-js.bat web <program> ---
if /i "%~1"=="web" (
    if not "%~2"=="" (
        set "EXEC=%~2"
        set "EXTRA_ARGS="
        set "PTYPE="
        goto :run_web
    )
    echo Usage: run-js.bat web programs\foo.com
    goto :eof
)

REM --- Direct invocation: run-js.bat <program> [ref-dos flags...] ---
if not "%~1"=="" if not "%~1"=="--help" (
    set "EXEC=%~1"
    REM Collect remaining args for ref-dos.mjs
    set "EXTRA_ARGS="
    shift
    :collect_args
    if not "%~1"=="" (
        set "EXTRA_ARGS=!EXTRA_ARGS! %~1"
        shift
        goto :collect_args
    )
    goto :run_direct
)

if "%~1"=="--help" (
    echo Usage: run-js.bat [program] [--ticks=N] [--trace] [--int-trace] ...
    echo.
    echo   No arguments: interactive menu
    echo   program:      path to .com/.exe file
    echo   Extra flags are passed to ref-dos.mjs
    echo.
    echo ref-dos.mjs flags:
    echo   --ticks=N          Max instructions (default 1000000^)
    echo   --trace            Print register state every tick
    echo   --trace-from=N     Start tracing at tick N
    echo   --int-trace        Log all INT calls
    echo   --int-trace-from=N Start INT tracing at tick N
    echo   --port-trace       Log all I/O port accesses
    goto :eof
)

REM --- Interactive menu ---
:menu
cls
echo.
echo   CSS-DOS (JS8086 Reference Emulator)
echo   ====================================
echo.

set COUNT=0

REM Standalone .com files
for %%f in (%PROGDIR%\*.com) do (
    set /a COUNT+=1
    set "NAME_!COUNT!=%%~nf"
    set "EXEC_!COUNT!=%%f"
    set "TYPE_!COUNT!=com"
    set N=!COUNT!
    echo     !N!. %%~nf.com	(%%~zf bytes^)
)

REM Standalone .exe files
for %%f in (%PROGDIR%\*.exe) do (
    set /a COUNT+=1
    set "NAME_!COUNT!=%%~nf"
    set "EXEC_!COUNT!=%%f"
    set "TYPE_!COUNT!=exe"
    set N=!COUNT!
    echo     !N!. %%~nf.exe	(%%~zf bytes^)
)

REM Subdirectory programs
for /d %%d in (%PROGDIR%\*) do (
    if /i not "%%~nxd"==".cache" (
        set _FOUND=
        for %%f in ("%%d\*.exe") do (
            if not defined _FOUND (
                set /a COUNT+=1
                set "NAME_!COUNT!=%%~nd"
                set "EXEC_!COUNT!=%%f"
                set "TYPE_!COUNT!=dir"
                set "DIR_!COUNT!=%%d"
                set _FOUND=1
                set N=!COUNT!
                echo     !N!. %%~nd\%%~nxf	(%%~zf bytes + data^)
            )
        )
        if not defined _FOUND (
            for %%f in ("%%d\*.com") do (
                if not defined _FOUND (
                    set /a COUNT+=1
                    set "NAME_!COUNT!=%%~nd"
                    set "EXEC_!COUNT!=%%f"
                    set "TYPE_!COUNT!=dir"
                    set "DIR_!COUNT!=%%d"
                    set _FOUND=1
                    set N=!COUNT!
                    echo     !N!. %%~nd\%%~nxf	(%%~zf bytes + data^)
                )
            )
        )
    )
)

if %COUNT%==0 (
    echo     No programs found. Drop .COM/.EXE files into %PROGDIR%\
    echo.
    pause
    goto :eof
)

echo.
echo     0. Exit
echo.
set /p CHOICE="  Pick a number: "

if "%CHOICE%"=="0" goto :eof
if "%CHOICE%"=="" goto :menu

set VALID=0
for /l %%i in (1,1,%COUNT%) do (
    if "%CHOICE%"=="%%i" set VALID=1
)
if %VALID%==0 (
    echo   Invalid choice.
    timeout /t 1 /nobreak >nul
    goto :menu
)

set "EXEC=!EXEC_%CHOICE%!"
set "PTYPE=!TYPE_%CHOICE%!"

set "EXTRA_ARGS="

:run_direct
REM Determine program name for CONFIG.SYS
for %%f in ("!EXEC!") do set "PROGNAME=%%~nxf"

REM Determine if this is a directory program (has companion files)
set "_DATAFLAGS="
if defined PTYPE if "!PTYPE!"=="dir" (
    set "_DIR=!DIR_%CHOICE%!"
    for %%f in ("!_DIR!\*") do (
        if /i not "%%f"=="!EXEC!" (
            set "_DATAFLAGS=!_DATAFLAGS! --file %%~nxf "%%f""
        )
    )
    for /d %%s in ("!_DIR!\*") do (
        for %%f in ("%%s\*") do (
            set "_DATAFLAGS=!_DATAFLAGS! --file %%~ns\%%~nxf "%%f""
        )
    )
)

REM Check if it's SHELL.COM (interactive shell mode)
set "IS_SHELL=0"
for %%f in ("!EXEC!") do if /i "%%~nxf"=="shell.com" set "IS_SHELL=1"

REM Write CONFIG.SYS
if "!IS_SHELL!"=="1" (
    echo SHELL=\COMMAND.COM> "%CONFIGSYS%"
) else (
    echo SHELL=\!PROGNAME!> "%CONFIGSYS%"
)

REM Build disk image via mkfat12
echo.
echo   Building disk image for !PROGNAME!...

set MKFAT_CMD=node "%MKFAT12%" -o "%DISKIMG%" --file KERNEL.SYS "%KERNEL%" --file CONFIG.SYS "%CONFIGSYS%"

if "!IS_SHELL!"=="1" (
    set "MKFAT_CMD=!MKFAT_CMD! --file COMMAND.COM "%COMMAND_COM%""
) else (
    for %%f in ("!EXEC!") do set "MKFAT_CMD=!MKFAT_CMD! --file !PROGNAME! "%%~ff""
)

if defined _DATAFLAGS set "MKFAT_CMD=!MKFAT_CMD! !_DATAFLAGS!"

!MKFAT_CMD!
if errorlevel 1 (
    echo   FAILED: disk image generation
    pause
    goto :eof
)

echo   Running JS8086 reference emulator...
echo.

node "%REFDOS%" !EXTRA_ARGS!

echo.
echo   Done. Press any key...
pause >nul
goto :eof

:run_web
REM Build disk image then launch browser server
for %%f in ("!EXEC!") do set "PROGNAME=%%~nxf"

set "IS_SHELL=0"
for %%f in ("!EXEC!") do if /i "%%~nxf"=="shell.com" set "IS_SHELL=1"

if "!IS_SHELL!"=="1" (
    echo SHELL=\COMMAND.COM> "%CONFIGSYS%"
) else (
    echo SHELL=\!PROGNAME!> "%CONFIGSYS%"
)

set MKFAT_CMD=node "%MKFAT12%" -o "%DISKIMG%" --file KERNEL.SYS "%KERNEL%" --file CONFIG.SYS "%CONFIGSYS%"
if "!IS_SHELL!"=="1" (
    set "MKFAT_CMD=!MKFAT_CMD! --file COMMAND.COM "%COMMAND_COM%""
) else (
    for %%f in ("!EXEC!") do set "MKFAT_CMD=!MKFAT_CMD! --file !PROGNAME! "%%~ff""
)

echo.
echo   Building disk image for !PROGNAME!...
!MKFAT_CMD!
if errorlevel 1 (
    echo   FAILED
    pause
    goto :eof
)

echo.
echo   Starting JS8086 web server...
echo   Open http://localhost:8086 in your browser.
echo.
node tools\serve-js8086.mjs
