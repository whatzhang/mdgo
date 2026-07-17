@echo off
setlocal
set "PORT=%~1"
if "%PORT%"=="" set "PORT=8091"

rem --- kill existing process on the port ---
set "PID="
for /f "tokens=5" %%a in ('netstat -ano ^| findstr ":%PORT%" ^| findstr "LISTENING"') do set "PID=%%a"
if defined PID (
    taskkill /PID %PID% /T /F >nul 2>&1
    timeout /t 2 /nobreak >nul
    set "PID2="
    for /f "tokens=5" %%a in ('netstat -ano ^| findstr ":%PORT%" ^| findstr "LISTENING"') do set "PID2=%%a"
    if defined PID2 exit /b 1
)

rem --- resolve venv python ---
set "SCRIPT_DIR=%~dp0"
set "VENV_PY=%SCRIPT_DIR%..\.venv\Scripts\python.exe"

rem --- validate venv ---
"%VENV_PY%" --version >nul 2>&1
if errorlevel 1 (
    echo venv is broken at "%VENV_PY%"
    exit /b 1
)

rem --- start server ---
set "LOGFILE=%SCRIPT_DIR%server_%PORT%.log"
start /B "" cmd /c ""%VENV_PY%" -m uvicorn main:app --host 0.0.0.0 --port %PORT% > "%LOGFILE%" 2>&1"
timeout /t 3 /nobreak >nul

rem --- verify ---
for /f "tokens=5" %%a in ('netstat -ano ^| findstr ":%PORT%" ^| findstr "LISTENING"') do exit /b 0
exit /b 1
