@echo off
REM run-web.bat -- Build a DOS program into a cabinet and run it in Chrome via calcite (WASM).
REM
REM   run-web.bat                         Interactive menu
REM   run-web.bat programs\foo.com        Build and run one file
REM   run-web.bat path\to\cart            Build and run a cart folder
REM
REM Pipeline:
REM   .com/.exe  ->  temp cart (.cache\carts\<name>\)  ->  CSS-DOS builder
REM   cart folder                                      ->  CSS-DOS builder
REM                                                        output\<name>.css
REM   static server (tools\serve-web.mjs, port %PORT%)
REM   Chrome -> http://localhost:%PORT%/web/index.html?css=<name>.css
REM
REM BIOS is picked by the cart's program.json (preset dos-muslin by default,
REM or dos-corduroy for the C BIOS). No more BIOS toggle in this script -- set
REM it per cart with:
REM     { "preset": "dos-corduroy" }     in program.json.

setlocal enabledelayedexpansion
cd /d "%~dp0"

set CSSDIR=..\CSS-DOS
set BUILDER=%CSSDIR%\builder\build.mjs
set PROGRAMDIR=programs
set OUTPUTDIR=output
set CACHEDIR=.cache\carts
REM Port 8766 (not 8765 -- that one collides with CSS-DOS's player/serve.mjs).
set PORT=8766

if not exist "%PROGRAMDIR%" mkdir "%PROGRAMDIR%"
if not exist "%OUTPUTDIR%" mkdir "%OUTPUTDIR%"
if not exist "%CACHEDIR%" mkdir "%CACHEDIR%"

if not exist "%BUILDER%" (
    echo ERROR: CSS-DOS builder not found at %BUILDER%
    echo        Expected a sibling CSS-DOS checkout.
    exit /b 1
)

REM --- Arg mode ---
if not "%~1"=="" (
    set "TARGET=%~1"
    goto :resolve_target
)

REM --- Interactive menu ---
:menu
cls
echo.
echo   calcite web runner
echo   ==================
echo.

set COUNT=0

REM Pre-built cabinets in output\
for %%f in (%OUTPUTDIR%\*.css) do (
    set /a COUNT+=1
    set "PATH_!COUNT!=%%f"
    set "TYPE_!COUNT!=cabinet"
    set N=!COUNT!
    for %%a in ("%%f") do set "_SZ=%%~za"
    set /a _MB=!_SZ! / 1048576
    echo     !N!. [prebuilt] %%~nxf	(!_MB! MB^)
)

if !COUNT! GTR 0 echo.

REM .com files in programs\
for %%f in (%PROGRAMDIR%\*.com) do (
    set /a COUNT+=1
    set "PATH_!COUNT!=%%f"
    set "TYPE_!COUNT!=file"
    set N=!COUNT!
    echo     !N!. %%~nxf	(%%~zf bytes^)
)
REM .exe files in programs\
for %%f in (%PROGRAMDIR%\*.exe) do (
    set /a COUNT+=1
    set "PATH_!COUNT!=%%f"
    set "TYPE_!COUNT!=file"
    set N=!COUNT!
    echo     !N!. %%~nxf	(%%~zf bytes^)
)
REM Cart-style folders in programs\
for /d %%d in (%PROGRAMDIR%\*) do (
    set /a COUNT+=1
    set "PATH_!COUNT!=%%d"
    set "TYPE_!COUNT!=dir"
    set N=!COUNT!
    echo     !N!. %%~nxd\	(cart folder^)
)

if !COUNT!==0 (
    echo     ^(no programs in %PROGRAMDIR%\ or cabinets in %OUTPUTDIR%\^)
    echo.
    echo Drop .com / .exe files or cart folders into %PROGRAMDIR%\ and retry.
    exit /b 1
)

echo.
echo     q. Quit
echo.
set /p CHOICE=Pick a number:
if /i "!CHOICE!"=="q" exit /b 0
if "!CHOICE!"=="" goto :menu

set "TARGET="
set "SEL_TYPE="
for /L %%i in (1,1,!COUNT!) do (
    if "!CHOICE!"=="%%i" (
        set "TARGET=!PATH_%%i!"
        set "SEL_TYPE=!TYPE_%%i!"
    )
)
if "!TARGET!"=="" (
    echo Invalid choice.
    timeout /t 1 /nobreak >nul
    goto :menu
)

REM Prebuilt cabinet: skip the build step entirely.
if /i "!SEL_TYPE!"=="cabinet" (
    for %%f in ("!TARGET!") do set "NAME=%%~nf"
    set "CABINET=!TARGET!"
    echo.
    echo Using prebuilt cabinet: !CABINET!
    goto :launch
)

:resolve_target
if not exist "%TARGET%" (
    echo ERROR: not found: %TARGET%
    exit /b 1
)

REM Decide: file or directory? Wrap into temp cart if it's a file.
set IS_DIR=0
if exist "%TARGET%\" set IS_DIR=1

if "%IS_DIR%"=="1" (
    REM Cart folder: use as-is. Cabinet name from folder name.
    for %%X in ("%TARGET%") do set "CARTPATH=%%~fX"
    for %%X in ("%TARGET%") do set "NAME=%%~nxX"
) else (
    REM File: wrap into a temp cart (one .com/.exe, builder infers defaults).
    for %%X in ("%TARGET%") do set "NAME=%%~nX"
    set "CARTPATH=%CACHEDIR%\!NAME!"
    if not exist "!CARTPATH!" mkdir "!CARTPATH!"
    REM Refresh the copy every time in case the binary changed.
    copy /y "%TARGET%" "!CARTPATH!\" >nul
)

set "CABINET=%OUTPUTDIR%\%NAME%.css"

echo.
echo Building cart:  %CARTPATH%
echo Cabinet:        %CABINET%
echo.

call node "%BUILDER%" "%CARTPATH%" -o "%CABINET%"
if errorlevel 1 (
    echo.
    echo Build failed.
    if "%~1"=="" ( pause & goto :menu ) else ( exit /b 1 )
)

echo.

:launch
REM Start the static server if *our* server isn't already running on %PORT%.
REM Probe /__calcite -- a bare port check is not enough, other local servers
REM (CSS-DOS's player/serve.mjs on 8765) may be squatting on %PORT%.
REM Laid out as linear goto-labels -- cmd parses goto-across-block weirdly
REM inside if/else parens, so we avoid that shape entirely.
set OURS=0
for /f "delims=" %%R in ('curl -s -f --max-time 1 http://localhost:%PORT%/__calcite 2^>nul') do (
    if "%%R"=="calcite-serve-web" set OURS=1
)

if "!OURS!"=="1" goto :server_already_ours
goto :maybe_start_server

:server_already_ours
echo Static server already running on port %PORT%, killing it for a clean restart.
for /f "tokens=5" %%P in ('netstat -ano ^| findstr ":%PORT% " ^| findstr LISTENING') do (
    taskkill /f /pid %%P >nul 2>nul
)
REM Give the OS a moment to release the port.
timeout /t 1 /nobreak >nul
goto :maybe_start_server

:maybe_start_server
REM Is something else on this port? Warn so the user can kill it.
netstat -ano | findstr ":%PORT% " | findstr LISTENING >nul
if not errorlevel 1 (
    echo WARNING: port %PORT% is in use by another process.
    echo          Close it or change PORT in run-web.bat.
    exit /b 1
)
echo Starting static server on port %PORT%...
start "calcite-serve" /MIN cmd /c "node tools\serve-web.mjs --port %PORT%"
set WAITED=0
:waitloop
timeout /t 1 /nobreak >nul
set /a WAITED+=1
curl -s -f --max-time 1 http://localhost:%PORT%/__calcite >nul 2>nul
if not errorlevel 1 goto :open_browser
if !WAITED! lss 5 goto :waitloop
echo WARNING: server did not respond within 5s, opening anyway.

:open_browser

set "URL=http://localhost:%PORT%/web/index.html?css=%NAME%.css"
echo.
echo Opening %URL%
start "" "%URL%"

echo.
if "%~1"=="" (
    echo Press any key to return to the menu (server keeps running).
    pause >nul
    goto :menu
)

endlocal
