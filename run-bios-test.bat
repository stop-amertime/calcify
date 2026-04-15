@echo off
REM run-bios-test.bat -- Rebuild the C BIOS, regen CSS for bootle.com, and
REM launch the web-based calcite UI pointed at it.
REM
REM The web UI uses a canvas for mode-13h graphics so the splash renders at
REM full fidelity. Terminal half-block rendering is too low-res for the
REM 320x200 graphics.

setlocal
cd /d "%~dp0"

set WORKTREE=C:\Users\AdmT9N0CX01V65438A\AppData\Local\Temp\css-dos-bios-splash
set CSS_NAME=bootle-ctest.css
set CSS=output\%CSS_NAME%
set PROGRAM=programs\bootle.com
set PORT=8765

echo === Building C BIOS (bios.bin) ===
call node "%WORKTREE%\bios\build.mjs"
if errorlevel 1 goto :fail

echo.
echo === Regenerating CSS from bootle.com with new BIOS ===
call node "%WORKTREE%\transpiler\generate-dos.mjs" "%CD%\%PROGRAM%" -o "%CD%\%CSS%"
if errorlevel 1 goto :fail

echo.
echo === Starting local HTTP server on port %PORT% ===
echo     Serves the calcite web UI and the generated CSS.
start "calcite-http" /MIN python -m http.server %PORT%

REM Wait a moment for the server to bind the port
timeout /t 1 /nobreak >nul

echo.
echo === Opening browser to the splash test ===
start "" "http://localhost:%PORT%/web/index.html?css=%CSS_NAME%"

echo.
echo The browser should now be open and auto-loading the CSS.
echo Wait ~5 seconds for the CSS to compile in the worker, then press
echo the Start button in the UI.
echo.
echo Press any key here to stop the HTTP server and exit.
pause >nul

REM Kill the minimized python server
taskkill /FI "WindowTitle eq calcite-http*" /T /F >nul 2>&1
goto :eof

:fail
echo.
echo BUILD FAILED
pause
