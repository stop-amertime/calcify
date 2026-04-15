@echo off
REM run-oldbios-test.bat -- Generate CSS using the OLD asm BIOS (from the
REM main CSS-DOS repo on master), then serve it via the web UI. Sanity
REM check that the browser pipeline works end-to-end without touching the
REM bios-splash worktree at all.

setlocal
cd /d "%~dp0"

set CSSDIR=..\CSS-DOS
set GENERATOR=%CSSDIR%\transpiler\generate-dos.mjs
set PROGRAM=programs\bootle.com
set CSS_NAME=bootle-oldbios.css
set CSS=output\%CSS_NAME%
set PORT=8765

echo === Regenerating CSS from %PROGRAM% using OLD asm BIOS ===
call node "%GENERATOR%" "%CD%\%PROGRAM%" -o "%CD%\%CSS%"
if errorlevel 1 goto :fail

REM Reuse server if already running on %PORT%
netstat -ano | findstr ":%PORT% " | findstr LISTENING >nul
if errorlevel 1 (
    echo.
    echo === Starting HTTP server on port %PORT% ===
    start "calcite-http" /MIN python -m http.server %PORT%
    timeout /t 1 /nobreak >nul
) else (
    echo.
    echo HTTP server already running on port %PORT%, reusing.
)

echo.
echo === Opening browser ===
echo   http://localhost:%PORT%/web/index.html?css=%CSS_NAME%
start "" "http://localhost:%PORT%/web/index.html?css=%CSS_NAME%"

echo.
echo Wait ~5s for the CSS to compile in the worker, then click Start.
echo You should see the old text-mode "Gossamer BIOS v1.0" splash then boot.
echo.
pause >nul
goto :eof

:fail
echo.
echo BUILD FAILED
pause
