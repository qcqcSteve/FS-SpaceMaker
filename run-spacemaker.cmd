@echo off
rem Launches run-spacemaker.ps1 without changing the PowerShell execution policy.
rem Usage: double-click (asks for the folders), or: run-spacemaker.cmd <source> <destination> [options]
rem Uses PowerShell 7 (pwsh) when installed, else Windows PowerShell. Arguments are passed through.

set "RUNNER=%~dp0run-spacemaker.ps1"
set "SHELL_EXE=powershell"
where pwsh >nul 2>&1 && set "SHELL_EXE=pwsh"

"%SHELL_EXE%" -NoProfile -ExecutionPolicy Bypass -File "%RUNNER%" %*
set "RUNNER_EXIT=%ERRORLEVEL%"

rem Without arguments the window was most likely opened by a double-click: keep it open to read the result.
if "%~1"=="" pause
exit /b %RUNNER_EXIT%
