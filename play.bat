@echo off
rem Build from source and play. Put this folder inside the game folder (the one with REKO3.COM).
setlocal
title Sangokushi Eiketsuden
cd /d "%~dp0"
if not exist "..\REKO3.COM" (
  echo [ERROR] Game files not found.
  echo Put this eiketsu-rt folder inside the game folder ^(the folder that has REKO3.COM^).
  pause
  exit /b 1
)
where cargo >nul 2>nul
if errorlevel 1 (
  echo [ERROR] cargo not found. Install Rust first: https://rustup.rs
  echo Or download the ready-made eiketsu-rt.exe from the Releases page instead.
  pause
  exit /b 1
)
echo Building (the first run compiles; please wait)...
cargo build --release
if errorlevel 1 (
  pause
  exit /b 1
)
target\release\eiketsu-rt.exe --game ..
if errorlevel 1 pause
