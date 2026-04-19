@echo off
REM Kill all running calcite-debugger.exe processes, then rebuild the
REM release binary. Useful after editing debugger source, or when the
REM linker fails with "Permission denied" on deps\calcite_debugger-*.exe
REM because an orphaned debugger is holding the file open.
REM
REM WARNING: this also kills the calcite-debugger that your active MCP
REM client is talking to. Most clients will auto-respawn it on the next
REM tool call; if yours doesn't, restart the client manually.

setlocal
cd /d "%~dp0"

echo [1/2] Killing calcite-debugger processes...
taskkill /F /IM calcite-debugger.exe 2>nul
if %errorlevel% equ 0 (
    echo       Done.
) else (
    echo       No calcite-debugger processes were running.
)

echo [2/2] Rebuilding calcite-debugger ^(release^)...
cargo build --release -p calcite-debugger
if %errorlevel% neq 0 (
    echo.
    echo Build FAILED. See errors above.
    endlocal
    exit /b %errorlevel%
)

echo.
echo Rebuild complete. The next MCP tool call should launch the new binary.
endlocal
