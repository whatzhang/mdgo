@echo off
chcp 65001 >nul 2>&1
setlocal enabledelayedexpansion

REM ==============================================================================
REM MDGo - Build and Test Script (Windows Batch)
REM Usage:
REM   build.bat install      Install all dependencies (Node + Rust)
REM   build.bat dev          Start Tauri dev mode (Frontend + Backend + Desktop)
REM   build.bat check        Check frontend build + Rust compilation
REM   build.bat test         Run all tests
REM   build.bat build        Build production Tauri desktop app
REM   build.bat clean        Clean build artifacts
REM ==============================================================================

REM -- Version Info --
set "SCRIPT_VERSION=1.1.0"
set "PROJECT_VERSION=0.1.0"

REM -- Path Settings --
set "SCRIPT_DIR=%~dp0"
if "%SCRIPT_DIR:~-1%"=="\" set "SCRIPT_DIR=%SCRIPT_DIR:~0,-1%"

REM Get normalized absolute path using cd command
pushd "%SCRIPT_DIR%\.."
set "PROJECT_DIR=%CD%"
popd

set "TAURI_DIR=%SCRIPT_DIR%"
set "TAURI_SRC=%TAURI_DIR%\src-tauri"
set "BACKEND_DIR=%PROJECT_DIR%\backend"

goto :main

REM ==============================================================================
REM Function Definitions
REM ==============================================================================

REM -- Fix 1: Add dependency check function --
:check_basic_dependencies
echo [INFO]  Checking basic dependencies...

where node >nul 2>&1
if errorlevel 1 (
    echo [ERROR] Node.js not found, please install Node.js first
    echo [INFO]  Download: https://nodejs.org/
    exit /b 1
)

where npm >nul 2>&1
if errorlevel 1 (
    echo [ERROR] npm not found, please install Node.js first
    echo [INFO]  Download: https://nodejs.org/
    exit /b 1
)

where cargo >nul 2>&1
if errorlevel 1 (
    echo [ERROR] Rust/Cargo not found, please install Rust first
    echo [INFO]  Download: https://rustup.rs/
    exit /b 1
)

echo [OK]    Basic dependencies check passed
goto :eof

:install_deps
echo [INFO]  Installing Node dependencies...
pushd "%TAURI_DIR%"
call npm install
if errorlevel 1 (
    echo [ERROR] Node dependencies installation failed
    popd
    exit /b 1
)
popd
echo [OK]    Node dependencies installed

echo [INFO]  Checking Rust toolchain...

REM -- Fix 3: Check if rustup exists --
where rustup >nul 2>&1
if errorlevel 1 (
    echo [ERROR] rustup not found, please install Rust first
    echo [INFO]  Visit https://rustup.rs/ to install Rust
    exit /b 1
)

rustup show active-toolchain 2>nul
if errorlevel 1 (
    echo [INFO]  Installing Rust stable toolchain...
    rustup toolchain install stable
    if errorlevel 1 (
        echo [ERROR] Rust toolchain installation failed
        exit /b 1
    )
)
echo [OK]    Rust toolchain ready
goto :eof

:check_frontend
echo [INFO]  Building frontend (Vite)...
pushd "%TAURI_DIR%"

if not exist "node_modules" (
    echo [INFO]  node_modules not found, installing dependencies...
    call npm install
    if errorlevel 1 (
        echo [ERROR] Dependencies installation failed
        popd
        exit /b 1
    )
)

REM -- Fix 4: Check if vite is in package.json --
if not exist "package.json" (
    echo [ERROR] package.json not found
    popd
    exit /b 1
)

findstr /C:"\"vite\"" package.json >nul 2>&1
if errorlevel 1 (
    echo [WARN]  vite dependency not found in package.json, trying to continue...
)

REM 清理构建缓存，确保使用最新代码
if exist "dist" rmdir /s /q "dist"
if exist "%PROJECT_DIR%\.vite" rmdir /s /q "%PROJECT_DIR%\.vite"

call npx vite build
if errorlevel 1 (
    echo [ERROR] Frontend build failed
    popd
    exit /b 1
)
popd
echo [OK]    Frontend build successful - %TAURI_DIR%\dist\
goto :eof

:check_rust
echo [INFO]  Checking Rust code compilation...
pushd "%TAURI_SRC%"
cargo check -j 1
if errorlevel 1 (
    echo [ERROR] Rust code compilation failed
    popd
    exit /b 1
)
popd
echo [OK]    Rust code compilation passed
goto :eof

:run_tests
echo [INFO]  Running Rust tests...
pushd "%TAURI_SRC%"
cargo test -j 1
set TEST_EXIT_CODE=%ERRORLEVEL%
popd

REM -- Fix 6: Accurately distinguish test failures and no test cases --
if %TEST_EXIT_CODE% equ 0 (
    echo [OK]    Rust tests passed
) else if %TEST_EXIT_CODE% equ 101 (
    echo [WARN]  No Rust test cases found
) else (
    echo [ERROR] Rust tests failed (exit code: %TEST_EXIT_CODE%)
    exit /b 1
)

:run_dev
echo [INFO]  Starting Tauri dev mode...

pushd "%TAURI_DIR%"

if not exist "node_modules" (
    echo [INFO]  node_modules not found, installing dependencies...
    call npm install
    if errorlevel 1 (
        echo [ERROR] Dependencies installation failed
        popd
        exit /b 1
    )
)

REM -- Fix 5: Check if tauri-cli exists --
if not exist "package.json" (
    echo [ERROR] package.json not found
    popd
    exit /b 1
)

findstr /C:"@tauri-apps/cli" package.json >nul 2>&1
if errorlevel 1 (
    echo [WARN]  @tauri-apps/cli dependency not found in package.json, trying to continue...
)

call npx tauri dev
set DEV_EXIT_CODE=%ERRORLEVEL%
popd

if %DEV_EXIT_CODE% neq 0 (
    echo [ERROR] Tauri dev mode startup failed
    exit /b 1
)
goto :eof

:run_build
echo [INFO]  Building Tauri desktop app...
call :check_frontend
if errorlevel 1 exit /b 1

pushd "%TAURI_DIR%"
call npx tauri build
if errorlevel 1 (
    echo [ERROR] Tauri app build failed
    popd
    exit /b 1
)
popd

echo [OK]    Tauri app build completed!
REM -- Fix 10: Display build artifact paths --
echo [INFO]  Build artifacts located at: %TAURI_SRC%\target\release\
echo [INFO]  Windows installer: %TAURI_SRC%\target\release\bundle\msi\
echo [INFO]  Windows executable: %TAURI_SRC%\target\release\mdgo.exe
goto :eof

:clean_all
echo [INFO]  Cleaning build artifacts...
if exist "%TAURI_DIR%\dist" (
    rmdir /s /q "%TAURI_DIR%\dist"
    echo [OK]    Deleted frontend build artifacts %TAURI_DIR%\dist\
)
pushd "%TAURI_SRC%"
cargo clean 2>nul
if errorlevel 1 (
    echo [WARN]  Rust clean failed
) else (
    echo [OK]    Cleaned Rust build cache
)
popd
echo [OK]    Cleanup completed
goto :eof

REM -- Fix 8: Add version info display --
:show_help
echo MDGo - Local Document Knowledge Base Build and Test Script
echo.
echo Script Version: %SCRIPT_VERSION%  Project Version: %PROJECT_VERSION%
echo.
echo Usage: build.bat ^<command^>
echo.
echo Commands:
echo   install    Install all dependencies (Node + Rust)
echo   dev        Start Tauri dev mode, npx tauri dev
echo   check      Check frontend build + Rust compilation, npx vite build ^&^& cargo check
echo   test       Run all tests, cargo test
echo   build      Build production Tauri desktop app, npx tauri build
echo   clean      Clean build artifacts, cargo clean
echo   help       Show this help message
goto :eof

REM ==============================================================================
REM Main Command Dispatcher
REM ==============================================================================
:main
REM -- Fix 1: Check basic dependencies at script start --
call :check_basic_dependencies
if errorlevel 1 goto :end

set "CMD=%~1"
if "%CMD%"=="" set "CMD=help"

if "%CMD%"=="install" ( call :install_deps & goto :end )
if "%CMD%"=="dev"     ( call :run_dev & goto :end )

REM -- Fix 9: Fix check command chaining --
if "%CMD%"=="check" (
    call :check_frontend
    if errorlevel 1 goto :end
    call :check_rust
    if errorlevel 1 goto :end
    echo [OK]    All checks passed!
    goto :end
)

if "%CMD%"=="test"    ( call :run_tests & goto :end )
if "%CMD%"=="build"   ( call :run_build & goto :end )
if "%CMD%"=="clean"   ( call :clean_all & goto :end )
if "%CMD%"=="help"    ( call :show_help & goto :end )

echo [ERROR] Unknown command: %CMD%
call :show_help

:end
endlocal