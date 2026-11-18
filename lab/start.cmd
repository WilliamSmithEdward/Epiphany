@echo off
rem Builds the server, starts it on the lab model and opens the lab page.
rem Put overrides such as EPIPHANY_ADDRESS=127.0.0.1:9090 in lab\.env.local (not committed).
setlocal
cd /d "%~dp0.."

set "EPIPHANY_ADDRESS=127.0.0.1:8080"
if exist "lab\.env.local" for /f "usebackq eol=# tokens=1,* delims==" %%a in ("lab\.env.local") do set "%%a=%%b"

cargo build --release -p epiphany-server
if errorlevel 1 (
  pause
  exit /b 1
)

start "" "http://%EPIPHANY_ADDRESS%/lab"
target\release\epiphany-server.exe lab lab %EPIPHANY_ADDRESS%
pause
