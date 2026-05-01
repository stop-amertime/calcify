@echo off
REM Start the calcite-debugger daemon in the background.
REM
REM The daemon listens on 127.0.0.1:3334 (override with CALCITE_DEBUGGER_ADDR)
REM and keeps in-memory session state across MCP client reconnects.
REM
REM mcp-shim.mjs does NOT autostart the daemon — start it explicitly
REM with this script before connecting an MCP client. (Autostart was
REM removed because it caused ghost-daemon races; see the comment at
REM the top of tools/mcp-shim.mjs.)
REM
REM To stop: taskkill /F /IM calcite-debugger.exe
REM (or use kill-and-rebuild.bat which kills + rebuilds).

setlocal
cd /d "%~dp0"

set ADDR=%CALCITE_DEBUGGER_ADDR%
if "%ADDR%"=="" set ADDR=127.0.0.1:3334

set BIN=%CALCITE_DEBUGGER_BIN%
if "%BIN%"=="" set BIN=%~dp0target\release\calcite-debugger.exe

if not exist "%BIN%" (
    echo [daemon] binary not found at %BIN%
    echo          run: cargo build --release -p calcite-debugger
    exit /b 1
)

echo [daemon] starting %BIN% --listen %ADDR%
start "calcite-debugger daemon" /B "%BIN%" --listen %ADDR%
echo [daemon] started. verify with: netstat -ano ^| findstr :%ADDR:~-4%
endlocal
