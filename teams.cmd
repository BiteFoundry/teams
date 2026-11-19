@echo off
chcp 65001 >nul
setlocal
set "ROOT=%~dp0"

if exist "%ROOT%target\release\teams.exe" goto run

where gh >nul 2>nul
if %errorlevel% equ 0 (
    echo [teams] downloading prebuilt binary from GitHub Release ^(BiteFoundry/teams^)... >&2
    if not exist "%ROOT%target\release" mkdir "%ROOT%target\release"
    gh release download -R BiteFoundry/teams --pattern "teams.exe" --dir "%ROOT%target\release" --clobber >nul 2>&1
    if exist "%ROOT%target\release\teams.exe" goto run
)

echo [teams] prebuilt binary not found - building from source with cargo... >&2
cargo build --release --manifest-path "%ROOT%Cargo.toml"
if errorlevel 1 exit /b 1

:run
"%ROOT%target\release\teams.exe" %*
exit /b %errorlevel%
