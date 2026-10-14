@echo off
rem One-time Epic login for RL Tracker. Opens the Epic page with your code filled in;
rem sign in, approve, and the token is saved. Run this once, then start the tracker.
cd /d "%~dp0sidecar"
if not exist rlsidecar.exe (
    echo Building sidecar...
    go build -o rlsidecar.exe . || ( echo Build failed - is Go installed? & pause & exit /b 1 )
)
rlsidecar.exe -login-only
echo.
pause
