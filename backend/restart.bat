@echo off
setlocal

set "PORT=%~1"
if "%PORT%"=="" set "PORT=8091"

call "%~dp0stop.bat" %PORT%
timeout /t 1 /nobreak >nul
call "%~dp0start.bat" %PORT%
