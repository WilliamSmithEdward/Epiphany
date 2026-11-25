@echo off
rem Builds the server and the GUI, starts both on the sample model and opens the GUI.
rem Put overrides such as EPIPHANY_SERVER=127.0.0.1:9090 or EPIPHANY_GUI=127.0.0.1:9091 in
rem sample\.env.local (not committed).
setlocal
cd /d "%~dp0"

set "EPIPHANY_SERVER=127.0.0.1:8080"
set "EPIPHANY_GUI=127.0.0.1:8090"
if exist "sample\.env.local" for /f "usebackq eol=# tokens=1,* delims==" %%a in ("sample\.env.local") do set "%%a=%%b"

cargo build --release -p epiphany-server -p epiphany-gui
if errorlevel 1 (
  pause
  exit /b 1
)

rem The server in a window of its own; the GUI in this one. Ctrl+C in each stops it.
start "Epiphany server" target\release\epiphany-server.exe serve sample %EPIPHANY_SERVER%
start "" "http://%EPIPHANY_GUI%"
target\release\epiphany-gui.exe --listen %EPIPHANY_GUI% --server http://%EPIPHANY_SERVER%
pause
