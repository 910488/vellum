@echo off
PowerShell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -Command "Start-Process PowerShell.exe -Verb RunAs -ArgumentList '-NoLogo -NoProfile -ExecutionPolicy Bypass -File ""%~dp0Enable-Windows-Sandbox.ps1""'"
