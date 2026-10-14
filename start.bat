@echo off
rem Builds (if needed) and starts RL Tracker. Run login.bat once first.
cd /d "%~dp0"

echo Building sidecar...
pushd sidecar
go build -o rlsidecar.exe . || ( echo Sidecar build failed - is Go installed? & popd & pause & exit /b 1 )
popd

echo Starting tracker...
cargo run --release || ( echo. & echo Tracker failed to build or crashed. & pause & exit /b 1 )
