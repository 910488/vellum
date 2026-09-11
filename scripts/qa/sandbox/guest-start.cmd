@echo off
set /a n=0
:waitmap
if exist C:\QaOutput\run-manifest.json goto ready
set /a n+=1
if %n% GEQ 180 goto ready
ping -n 2 127.0.0.1 >nul
goto waitmap
:ready
echo {"stage":"logon"}> C:\QaOutput\HEARTBEAT
powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File C:\SandboxKit\guest-bootstrap.ps1 >> C:\QaOutput\guest-bootstrap.log 2>&1
