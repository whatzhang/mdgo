@echo off
setlocal

set "PORT=%~1"
if "%PORT%"=="" set "PORT=8091"

set "FOUND=0"
for /f "tokens=5" %%a in ('netstat -ano ^| findstr :%PORT% ^| findstr LISTENING') do (
    set "FOUND=1"
    echo 正在停止端口 %PORT% 的服务 (pid %%a)...
    taskkill /PID %%a /T /F >nul 2>&1 || echo 无法停止 %%a
)

if "%FOUND%"=="0" (
    echo 端口 %PORT% 上没有运行的服务
    exit /b 0
)

echo 已停止
