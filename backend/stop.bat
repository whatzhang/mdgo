@echo off
setlocal
set "PORT=%~1"
if "%PORT%"=="" set "PORT=8091"

set "FOUND=0"
for /f "tokens=5" %%a in ('netstat -ano ^| findstr ":%PORT%" ^| findstr "LISTENING"') do (
    set "FOUND=1"
    taskkill /PID %%a /T /F >nul 2>&1
)

if "%FOUND%"=="0" exit /b 0
exit /b 0
