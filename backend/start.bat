@echo off
setlocal

set "PORT=%~1"
if "%PORT%"=="" set "PORT=8091"

set "PID="
for /f "tokens=5" %%a in ('netstat -ano ^| findstr :%PORT% ^| findstr LISTENING') do (
    set "PID=%%a"
)
if defined PID (
    echo 端口 %PORT% 已被占用 (pid %PID%)，尝试停止该进程...
    taskkill /PID %PID% /T /F >nul 2>&1 || echo 无法通过 taskkill 停止 %PID%
    timeout /t 2 /nobreak >nul
    set "PID2="
    for /f "tokens=5" %%a in ('netstat -ano ^| findstr :%PORT% ^| findstr LISTENING') do (
        set "PID2=%%a"
    )
    if defined PID2 (
        echo 端口仍被占用，请手动检查 (pid %PID2%)
        exit /b 1
    )
)

set "VENV_PATH=..\.venv\Scripts\python.exe"
if not exist "%VENV_PATH%" set "VENV_PATH=python.exe"

set "LOGFILE=server_%PORT%.log"

echo 正在启动服务 (端口 %PORT%)... (日志 -> %LOGFILE%)
start /B cmd /c "%VENV_PATH% -m uvicorn main:app --host 0.0.0.0 --port %PORT% > %LOGFILE% 2>&1"

timeout /t 2 /nobreak >nul

for /f "tokens=5" %%a in ('netstat -ano ^| findstr :%PORT% ^| findstr LISTENING') do (
    echo 已启动 (pid %%a, 端口 %PORT%)
    exit /b 0
)

echo 启动失败，请检查日志 %LOGFILE%
exit /b 1
