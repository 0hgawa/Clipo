@echo off
chcp 65001 > nul
title Clipo - dev
setlocal

set "ROOT=%~dp0"

set "CLIPO_LOG=clipo=trace,clipo_core=trace,clipo_capture=trace,clipo_overlay=trace,tauri=info,wry=info,warn"
set "RUST_BACKTRACE=1"

if not exist "%ROOT%ui\node_modules\.bin\tauri.cmd" (
    echo ERROR: tauri CLI not installed. Run "pnpm install" from %ROOT% first.
    pause
    goto :eof
)

echo ============================================
echo  Clipo dev
echo  CLIPO_LOG=%CLIPO_LOG%
echo ============================================
echo.

cd /d "%ROOT%crates\clipo"
call "%ROOT%ui\node_modules\.bin\tauri.cmd" dev

echo.
pause
endlocal
