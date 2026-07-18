@echo off
setlocal

:: Switch to UTF-8 code page to avoid Chinese character encoding issues
chcp 65001 >nul

:: --- 1. Parse port argument ---
set "PORT=%~1"
if "%PORT%"=="" set "PORT=8091"

set "FOUND=0"
for /f "tokens=5" %%a in ('netstat -ano ^| findstr ":%PORT%" ^| findstr "LISTENING"') do (
    set "FOUND=1"
    echo [INFO] Stopping process on port %PORT% (pid %%a)...
    taskkill /PID %%a /T /F >nul 2>&1
    if errorlevel 1 (
        echo [WARN] Failed to stop process %%a
    )
)

if "%FOUND%"=="0" (
    echo [INFO] No service running on port %PORT%
    exit /b 0
)

echo [INFO] Service on port %PORT% stopped
