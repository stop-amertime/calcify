@echo off
REM run.bat — Run DOS programs through CSS-DOS (DOS + Calcite)
REM
REM   run.bat              Interactive menu
REM   run.bat diagnose     Pick a program and run conformance diagnosis
REM
REM Programs can be:
REM   programs/foo.com           Standalone .COM file
REM   programs/foo.exe           Standalone .EXE file
REM   programs/bar/BAR.EXE       .EXE with companion data files
REM   programs/bar/DATA.DAT      (all non-EXE/COM files passed as --data)

setlocal enabledelayedexpansion
cd /d "%~dp0"

set CALCITE=target\release\calcite-cli.exe
set DEBUGGER=target\release\calcite-debugger.exe
set CSSDIR=..\CSS-DOS
set GENERATOR=%CSSDIR%\transpiler\generate-dos.mjs
set BIOSBIN=%CSSDIR%\bios\css-emu-bios.bin
set PROGDIR=programs
set OUTPUTDIR=output

if not exist "%PROGDIR%" mkdir "%PROGDIR%"
if not exist "%OUTPUTDIR%" mkdir "%OUTPUTDIR%"

if /i "%~1"=="diagnose" goto :diagnose_menu

REM --- Interactive menu ---
:menu
cls
echo.
echo   CSS-DOS
echo   ======
echo.

REM Build numbered list: standalone .com/.exe files + subdirectory programs
set COUNT=0

REM Pre-built CSS files in output/ (run directly, no generation needed)
for %%f in (%OUTPUTDIR%\*.css) do (
    set /a COUNT+=1
    set "NAME_!COUNT!=%%~nf"
    set "EXEC_!COUNT!="
    set "TYPE_!COUNT!=css"
    set "CSS_!COUNT!=%%f"
    set N=!COUNT!
    for %%a in ("%%f") do set "_SZ=%%~za"
    set /a _MB=!_SZ! / 1048576
    echo     !N!. [CSS] %%~nf	(!_MB! MB^)
)

if !COUNT! GTR 0 echo.

REM Standalone .com files
for %%f in (%PROGDIR%\*.com) do (
    set /a COUNT+=1
    set "NAME_!COUNT!=%%~nf"
    set "EXEC_!COUNT!=%%f"
    set "TYPE_!COUNT!=com"
    set "SIZE_!COUNT!=%%~zf"
    set N=!COUNT!
    echo     !N!. %%~nf.com	(%%~zf bytes^)
)

REM Standalone .exe files
for %%f in (%PROGDIR%\*.exe) do (
    set /a COUNT+=1
    set "NAME_!COUNT!=%%~nf"
    set "EXEC_!COUNT!=%%f"
    set "TYPE_!COUNT!=exe"
    set "SIZE_!COUNT!=%%~zf"
    set N=!COUNT!
    echo     !N!. %%~nf.exe	(%%~zf bytes^)
)

REM Subdirectory programs (look for .exe or .com inside each subdir)
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
                set "SIZE_!COUNT!=%%~zf"
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
                    set "SIZE_!COUNT!=%%~zf"
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

REM Validate choice
set VALID=0
for /l %%i in (1,1,%COUNT%) do (
    if "%CHOICE%"=="%%i" set VALID=1
)
if %VALID%==0 (
    echo   Invalid choice.
    timeout /t 1 /nobreak >nul
    goto :menu
)

set "NAME=!NAME_%CHOICE%!"
set "EXEC=!EXEC_%CHOICE%!"
set "PTYPE=!TYPE_%CHOICE%!"

REM Build calcite if needed
if not exist "%CALCITE%" (
    echo.
    echo   Building calcite...
    cargo build --release -p calcite-cli
    if errorlevel 1 (pause & goto :menu)
)

REM For pre-built CSS, skip generation
if "!PTYPE!"=="css" (
    set "CSS=!CSS_%CHOICE%!"
    goto :run_program
)

REM Generate CSS
set CSS=%OUTPUTDIR%\%NAME%.css
call :generate "%EXEC%" "%CSS%" "%PTYPE%" "!DIR_%CHOICE%!"
if errorlevel 1 (pause & goto :menu)

:run_program

title CSS-DOS: %NAME%
cls
"%CALCITE%" --input "%CSS%" --ticks 4294967295 --interactive-batch 50000 --speed 1.0

echo.
echo   Program exited. Press any key to return to menu...
pause >nul
goto :menu

REM --- Diagnose menu ---
:diagnose_menu
cls
echo.
echo   CSS-DOS Conformance Diagnosis
echo   =============================
echo.

set COUNT=0

for %%f in (%PROGDIR%\*.com) do (
    set /a COUNT+=1
    set "NAME_!COUNT!=%%~nf"
    set "EXEC_!COUNT!=%%f"
    set "TYPE_!COUNT!=com"
    set N=!COUNT!
    echo     !N!. %%~nf.com	(%%~zf bytes^)
)

for %%f in (%PROGDIR%\*.exe) do (
    set /a COUNT+=1
    set "NAME_!COUNT!=%%~nf"
    set "EXEC_!COUNT!=%%f"
    set "TYPE_!COUNT!=exe"
    set N=!COUNT!
    echo     !N!. %%~nf.exe	(%%~zf bytes^)
)

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
    pause
    goto :eof
)

echo.
echo     0. Exit
echo.
set /p CHOICE="  Pick a number: "

if "%CHOICE%"=="0" goto :eof
if "%CHOICE%"=="" goto :diagnose_menu

set VALID=0
for /l %%i in (1,1,%COUNT%) do (
    if "%CHOICE%"=="%%i" set VALID=1
)
if %VALID%==0 (
    echo   Invalid choice.
    timeout /t 1 /nobreak >nul
    goto :diagnose_menu
)

set "NAME=!NAME_%CHOICE%!"
set "EXEC=!EXEC_%CHOICE%!"
set "PTYPE=!TYPE_%CHOICE%!"

set /p TICKS="  Ticks to check [5000]: "
if "%TICKS%"=="" set TICKS=5000

REM Build both binaries
if not exist "%CALCITE%" (
    echo   Building calcite-cli...
    cargo build --release -p calcite-cli
    if errorlevel 1 (pause & goto :diagnose_menu)
)
if not exist "%DEBUGGER%" (
    echo   Building calcite-debugger...
    cargo build --release -p calcite-debugger
    if errorlevel 1 (pause & goto :diagnose_menu)
)

REM Generate CSS
set CSS=%OUTPUTDIR%\%NAME%.css
call :generate "%EXEC%" "%CSS%" "%PTYPE%" "!DIR_%CHOICE%!"
if errorlevel 1 (pause & goto :diagnose_menu)

echo.
echo   Starting debugger...
start /b "" "%DEBUGGER%" -i "%CSS%"
timeout /t 4 /nobreak >nul

echo   Running diagnosis (%TICKS% ticks)...
echo.
node tools\diagnose.mjs "%EXEC%" "%BIOSBIN%" --ticks=%TICKS%

curl -s -X POST http://localhost:3333/shutdown >nul 2>&1

echo.
echo   Press any key to return to menu...
pause >nul
goto :diagnose_menu

REM --- Generate CSS subroutine ---
REM %1 = executable path, %2 = output CSS, %3 = type (com/exe/dir), %4 = directory (for dir type)
:generate
set _EXEC=%~1
set _CSS=%~2
set _TYPE=%~3
set _DIR=%~4

REM Always regenerate — transpiler changes must take effect immediately
if exist "%_CSS%" del "%_CSS%"

echo.
echo   Generating CSS for %~nx1 via DOS...

REM Build --data flags for companion files in subdirectory
set _DATAFLAGS=
if "%_TYPE%"=="dir" (
    REM Files directly in the program directory (not in subdirs)
    for %%f in ("%_DIR%\*") do (
        if /i not "%%f"=="%_EXEC%" (
            set "_DATAFLAGS=!_DATAFLAGS! --data %%~nxf "%%f""
        )
    )
    REM Files in subdirectories: pass as SUBDIR\FILENAME
    for /d %%s in ("%_DIR%\*") do (
        for %%f in ("%%s\*") do (
            set "_DATAFLAGS=!_DATAFLAGS! --data %%~ns\%%~nxf "%%f""
        )
    )
)

REM Calculate appropriate --mem value for this program
for /f %%m in ('node tools\calc-mem.mjs "%_EXEC%"') do set _MEM=%%m
echo   Memory: !_MEM!

node --max-old-space-size=8192 "%GENERATOR%" "%_EXEC%" -o "%_CSS%" --mem !_MEM! !_DATAFLAGS!
if errorlevel 1 (
    echo   FAILED: CSS generation
    del "%_CSS%" 2>nul
    exit /b 1
)
echo   Done.
goto :eof
