@echo off
PowerShell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File "%~dp0Launch-Sandbox.ps1" %*
if errorlevel 1 pause
