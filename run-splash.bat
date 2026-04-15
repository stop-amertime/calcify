@echo off
REM run-splash.bat -- Rebuild C BIOS splash, regenerate CSS, serve, and open browser.
setlocal
cd /d "%~dp0"

set WORKTREE=C:\Users\AdmT9N0CX01V65438A\AppData\Local\Temp\css-dos-bios-splash
set PORT=8765
set CSS_NAME=bootle-ctest.css
set CSS_PATH=output\%CSS_NAME%

echo === Building C BIOS ===
call node "%WORKTREE%\bios\build.mjs"
if errorlevel 1 goto :fail

echo.
echo === Regenerating CSS ===
call node "%WORKTREE%\transpiler\generate-dos.mjs" "%WORKTREE%\dos\bin\command.com" -o "%CD%\%CSS_PATH%"
if errorlevel 1 goto :fail

echo.
echo === Ensuring HTTP server on port %PORT% ===
netstat -ano | findstr ":%PORT% " | findstr LISTENING >nul
if errorlevel 1 (
    start "calcite-http" /MIN python serve.py %PORT%
    timeout /t 1 /nobreak >nul
) else (
    echo   already running, reusing
)

echo.
echo === Opening browser ===
echo   http://localhost:%PORT%/web/index.html?css=%CSS_NAME%
start "" "http://localhost:%PORT%/web/index.html?css=%CSS_NAME%"

echo.
echo Done. Click Run in the UI.
pause
goto :eof

:fail
echo BUILD FAILED
pause
