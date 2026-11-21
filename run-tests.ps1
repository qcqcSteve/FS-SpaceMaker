<#
.SYNOPSIS
    Runs the complete FS SpaceMaker verification on Windows: formatting, static analysis, every
    test suite (unit, integration, safety, Windows, properties, end-to-end), the load test and
    the release build.
.DESCRIPTION
    Usage: powershell -ExecutionPolicy Bypass -File .\run-tests.ps1 [-SkipLoadTest]
    Requires: Rust stable (cargo) with the clippy and rustfmt components.
    Note: the file symbolic link test runs only in an elevated session or with Developer Mode on.
#>
param(
    [switch]$SkipLoadTest
)

# ------------------------------------------------------------------------------------------------
# Configuration
# ------------------------------------------------------------------------------------------------

$ErrorActionPreference = 'Stop'
$ProjectDir = $PSScriptRoot
$ReleaseExecutable = Join-Path $ProjectDir 'target\release\fs-spacemaker.exe'
$Steps = @(
    @{ Name = 'Formatting';           Arguments = @('fmt', '--check') },
    @{ Name = 'Static analysis';      Arguments = @('clippy', '--all-targets', '--', '-D', 'warnings') },
    @{ Name = 'All test suites';      Arguments = @('test') },
    @{ Name = 'Release build';        Arguments = @('build', '--release') }
)
$LoadTestStep = @{
    Name      = 'Load test (10 000 files)'
    Arguments = @('test', '--release', '--test', 'stress', '--', '--ignored', '--nocapture')
}

# ------------------------------------------------------------------------------------------------
# Functions
# ------------------------------------------------------------------------------------------------

function Invoke-CargoStep {
    <# Runs one cargo step and returns $true when it succeeded. #>
    param([hashtable]$Step)
    Write-Host "--- $($Step.Name) ---" -ForegroundColor Cyan
    & cargo @($Step.Arguments)
    $isSuccess = $LASTEXITCODE -eq 0
    $color = if ($isSuccess) { 'Green' } else { 'Red' }
    Write-Host ("  {0}: {1}" -f $Step.Name, $(if ($isSuccess) { 'OK' } else { 'FAILED' })) -ForegroundColor $color
    return $isSuccess
}

function Test-SymbolicLinkPrivilege {
    <# Returns $true when this session can create file symbolic links. #>
    $principal = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
    $isElevated = $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    $unlockKey = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\AppModelUnlock'
    $developerMode = (Get-ItemProperty $unlockKey -ErrorAction SilentlyContinue).AllowDevelopmentWithoutDevLicense
    return $isElevated -or ($developerMode -eq 1)
}

function Main {
    Set-Location $ProjectDir
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        throw 'cargo not found: install Rust from https://rustup.rs, then run: rustup component add clippy rustfmt'
    }
    if (-not (Test-SymbolicLinkPrivilege)) {
        Write-Host 'Note: file symbolic link test will be skipped (run elevated or enable Developer Mode).' -ForegroundColor Yellow
    }
    $selectedSteps = $Steps
    if (-not $SkipLoadTest) { $selectedSteps = $Steps + $LoadTestStep }
    $failures = @($selectedSteps | Where-Object { -not (Invoke-CargoStep $_) } | ForEach-Object { $_.Name })
    Write-Host '--- Summary ---' -ForegroundColor Cyan
    if ($failures.Count -gt 0) {
        Write-Host ("FAILED: {0}" -f ($failures -join ', ')) -ForegroundColor Red
        exit 1
    }
    Write-Host 'All checks passed.' -ForegroundColor Green
    Write-Host "Executable: $ReleaseExecutable"
}

Main
