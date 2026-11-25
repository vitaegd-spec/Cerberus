@echo off
setlocal
cd /d "%~dp0"
title CERBERUS launcher

if not exist "%~dp0cerberus.exe" (
    echo CERBERUS has not been built yet - building it now.
    echo The first build downloads and compiles Tauri and takes a few minutes...
    echo.
    powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0build.ps1"
    if errorlevel 1 (
        echo.
        echo Build FAILED - copy the errors above and send them back.
        pause
        exit /b 1
    )
)

rem Start CERBERUS as a normal (non-elevated) program so drag and drop from
rem Explorer works. CERBERUS asks for administrator permission (UAC) once by
rem itself, for the helper that does the actual mounting.
rem If CERBERUS is already running in the tray, the new copy just shows it.
start "" "%~dp0cerberus.exe"
exit /b 0
