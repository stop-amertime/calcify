@echo off
REM bench-splash.bat -- Measure wall-clock time for the mode 13h grey fill.
REM
REM The C BIOS splash (bootle-ctest.css) fills the 320x200 framebuffer grey
REM before anything else. The last pixel lands at physical 0xA0000+63999 = 719359.
REM We halt the bench when that byte goes non-zero. Invariant: the halt tick is
REM ALWAYS 1,828,538 (conformant op count). What we optimise is "Elapsed".
REM
REM See docs/benchmarking.md -> "Splash-fill benchmark".

setlocal
cd /d "%~dp0"

set CSS=output\bootle-ctest.css
set HALT_ADDR=719359
set MAX_TICKS=5000000
set BATCH=50000

echo === calcite splash-fill benchmark ===
echo   CSS:       %CSS%
echo   Halt addr: %HALT_ADDR% (last mode-13h pixel, 0xA0000 + 63999)
echo   Fill done: tick 1,828,538 (invariant)
echo.

cargo run --release --bin calcite-bench -- -i %CSS% -n %MAX_TICKS% --halt %HALT_ADDR% --batch %BATCH% %*
