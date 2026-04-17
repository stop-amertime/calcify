@echo off
REM run-web.bat -- Interactive menu to run CSS-DOS programs in the browser.
REM Pick a program, regen its CSS (via CSS-DOS transpiler + C BIOS from the
REM bios-splash worktree), start a local HTTP server, and open the browser
REM at the web UI pointed at the CSS.

setlocal enabledelayedexpansion
cd /d "%~dp0"

set CSSDIR=..\CSS-DOS
set OUTPUTDIR=output
set PROGRAMDIR=programs
set PORT=8765
set BIOS=C

REM ─── Build list of programs + pre-built CSS ───────────────────────────
set COUNT=0

REM .com / .exe files in programs/
for %%f in (%PROGRAMDIR%\*.com %PROGRAMDIR%\*.exe) do (
    set /a COUNT+=1
    set "NAME_!COUNT!=%%~nxf"
    set "PATH_!COUNT!=%%f"
    set "TYPE_!COUNT!=prog"
)

REM pre-built .css files in output/
for %%f in (%OUTPUTDIR%\*.css) do (
    set /a COUNT+=1
    set "NAME_!COUNT!=%%~nxf (prebuilt)"
    set "PATH_!COUNT!=%%f"
    set "TYPE_!COUNT!=css"
)

if %COUNT%==0 (
    echo No programs or CSS files found.
    echo Put .com/.exe in %PROGRAMDIR%\ or .css in %OUTPUTDIR%\.
    pause
    goto :eof
)

:menu
cls
echo.
echo === CSS-DOS — run in browser ===
if "%BIOS%"=="C" (
    echo BIOS: C   ^(press 'b' to switch to ASM^)
) else (
    echo BIOS: ASM ^(press 'b' to switch to C^)
)
echo.
for /L %%i in (1,1,%COUNT%) do (
    echo   %%i. !NAME_%%i!
)
echo.
echo   b. Toggle BIOS
echo   q. Quit
echo.
set /p CHOICE=Pick a number:

if /i "%CHOICE%"=="q" goto :eof
if /i "%CHOICE%"=="b" (
    if "%BIOS%"=="C" ( set BIOS=ASM ) else ( set BIOS=C )
    goto :menu
)
if "%CHOICE%"=="" goto :menu

REM Validate numeric selection
set VALID=0
for /L %%i in (1,1,%COUNT%) do (
    if "%CHOICE%"=="%%i" set VALID=1
)
if %VALID%==0 (
    echo Invalid choice.
    timeout /t 1 /nobreak >nul
    goto :menu
)

set "SEL_NAME=!NAME_%CHOICE%!"
set "SEL_PATH=!PATH_%CHOICE%!"
set "SEL_TYPE=!TYPE_%CHOICE%!"

REM ─── Determine the CSS file to serve ──────────────────────────────────
if "%SEL_TYPE%"=="css" (
    set "CSS_PATH=%SEL_PATH%"
    for %%f in ("%SEL_PATH%") do set "CSS_NAME=%%~nxf"
    echo.
    echo Using pre-built CSS: %SEL_PATH%
) else (
    for %%f in ("%SEL_PATH%") do set "BASE=%%~nf"
    set "CSS_NAME=!BASE!.css"
    set "CSS_PATH=%OUTPUTDIR%\!CSS_NAME!"

    if "%BIOS%"=="C" (
        echo.
        echo === Building C BIOS (bios.bin) ===
        call node "%CSSDIR%\bios\build.mjs"
        if errorlevel 1 goto :fail

        echo.
        echo === Regenerating CSS from !SEL_NAME! (C BIOS) ===
        call node "%CSSDIR%\transpiler\generate-dos-c.mjs" "%CD%\%SEL_PATH%" -o "%CD%\!CSS_PATH!"
        if errorlevel 1 goto :fail
    ) else (
        echo.
        echo === Regenerating CSS from !SEL_NAME! (ASM BIOS) ===
        call node "%CSSDIR%\transpiler\generate-dos.mjs" "%CD%\%SEL_PATH%" -o "%CD%\!CSS_PATH!"
        if errorlevel 1 goto :fail
    )
)

REM ─── Start HTTP server (if not already running on %PORT%) ─────────────
REM Simple check: try to claim the port. If netstat shows it in LISTENING,
REM reuse the existing server.
netstat -ano | findstr ":%PORT% " | findstr LISTENING >nul
if errorlevel 1 (
    echo.
    echo === Starting HTTP server on port %PORT% ===
    start "calcite-http" /MIN python serve.py %PORT%
    timeout /t 1 /nobreak >nul
) else (
    echo.
    echo HTTP server already running on port %PORT%, reusing.
)

REM ─── Open browser ─────────────────────────────────────────────────────
echo.
echo === Opening browser ===
echo   http://localhost:%PORT%/web/index.html?css=!CSS_NAME!
start "" "http://localhost:%PORT%/web/index.html?css=!CSS_NAME!"

echo.
echo Browser is loading. The CSS will compile in the worker (~5s), then
echo click Start in the UI.
echo.
echo Press any key to return to the program list (server stays running).
pause >nul
goto :menu

:fail
echo.
echo BUILD FAILED
pause
goto :menu
