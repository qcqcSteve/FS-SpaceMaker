<#
.SYNOPSIS
    Guided FS SpaceMaker run: builds the executable when needed, scans, opens the report, runs the dry run,
    then applies the plan only after you type its confirmation code.
.DESCRIPTION
    Usage: double-click run-spacemaker.cmd (asks for both folders, the last ones are offered), or
           .\run-spacemaker.cmd K:\ S:\Archive [-Mode content|same-path|relocated|merge] [-RemoveSvnMetadata]
           [-IgnoreStream 'com.dropbox.*'] [-NoBuild] [-NoOpen]
    Requires: Rust stable (cargo) to build, unless target\release\fs-spacemaker.exe already exists (-NoBuild).
    Nothing is deleted before the last step, and only when the typed code matches the plan.
#>
param(
    [Parameter(Position = 0)][string]$Source,
    [Parameter(Position = 1)][string]$Destination,
    [ValidateSet('content', 'same-path', 'relocated', 'merge')][string]$Mode,
    [string]$Config,
    [string]$ReportsDir,
    [string]$WorkDir,
    [ValidateRange(1, 256)][int]$Threads,
    [string[]]$IgnoreStream,
    [switch]$IncludeEmptyFiles,
    [switch]$DeleteReadOnly,
    [switch]$RemoveSvnMetadata,
    [switch]$NoBuild,
    [switch]$NoOpen
)

# ------------------------------------------------------------------------------------------------
# Configuration
# ------------------------------------------------------------------------------------------------

$ErrorActionPreference = 'Stop'
$ProjectDir = $PSScriptRoot
$Executable = Join-Path $ProjectDir 'target\release\fs-spacemaker.exe'
$SourceDirs = @('src', 'Cargo.toml', 'Cargo.lock')
# Line of the scan summary that names the plan file (see print_scan_summary in src/cli.rs).
$PlanLinePattern = '^\s*plan:\s+(?<path>.+?)\s*$'
$ConfirmationLength = 8
$ScanReportName = 'report.html'
$DryRunReportName = 'dry-run-report.html'
$ApplyReportName = 'apply-report.html'
# Last folders used, offered as defaults when a folder is not given (kept out of git).
$LastRunFile = Join-Path $ProjectDir 'run-spacemaker.last.json'
$DriveWithoutBackslashPattern = '^[A-Za-z]:$'
$ExitSuccess = 0
$ExitStopped = 1

# ------------------------------------------------------------------------------------------------
# Helpers
# ------------------------------------------------------------------------------------------------

function Write-Step {
    <# Prints a section banner. #>
    param([string]$Title)
    Write-Host "--- $Title ---" -ForegroundColor Cyan
}

function Stop-Run {
    <# Prints why the run stops and exits with the given code. #>
    param([string]$Message, [int]$Code = $ExitStopped)
    Write-Host $Message -ForegroundColor Yellow
    exit $Code
}

function Confirm-Choice {
    <# Asks a yes/no question and returns $true for yes (default answer: yes). #>
    param([string]$Question)
    $answer = Read-Host "$Question [Y/n]"
    return [string]::IsNullOrWhiteSpace($answer) -or $answer.Trim() -match '^(y|yes)$'
}

function Open-Report {
    <# Opens an existing HTML report in the default browser unless -NoOpen was given. #>
    param([string]$Path)
    if (-not (Test-Path -LiteralPath $Path)) { return }
    Write-Host "  opening: $Path"
    if (-not $NoOpen) { Invoke-Item -LiteralPath $Path }
}

# ------------------------------------------------------------------------------------------------
# Folders
# ------------------------------------------------------------------------------------------------

function Read-LastRun {
    <# Returns the folders of the previous run, or empty values when none was saved. #>
    if (-not (Test-Path -LiteralPath $LastRunFile)) { return @{ Source = ''; Destination = '' } }
    $saved = Get-Content -LiteralPath $LastRunFile -Raw -Encoding UTF8 | ConvertFrom-Json
    return @{ Source = [string]$saved.source; Destination = [string]$saved.destination }
}

function Save-LastRun {
    <# Saves both folders so the next run offers them as defaults. #>
    param([string]$SourceFolder, [string]$DestinationFolder)
    $json = @{ source = $SourceFolder; destination = $DestinationFolder } | ConvertTo-Json
    Set-Content -LiteralPath $LastRunFile -Value $json -Encoding UTF8
}

function ConvertTo-FolderPath {
    <# Cleans a typed or pasted folder: strips quotes and spaces, turns "S:" into the drive root "S:\". #>
    param([string]$Text)
    $folder = $Text.Trim().Trim('"').Trim()
    if ($folder -match $DriveWithoutBackslashPattern) { $folder += '\' }
    return $folder
}

function Resolve-Folder {
    <# Returns the given folder, or asks for it (Enter keeps the last one); stops when none is known. #>
    param([string]$Label, [string]$Given, [string]$Last)
    if ($Given) { return ConvertTo-FolderPath $Given }
    $hint = if ($Last) { " [$Last]" } else { '' }
    $typed = Read-Host "$Label folder$hint"
    $folder = if ([string]::IsNullOrWhiteSpace($typed)) { $Last } else { ConvertTo-FolderPath $typed }
    if (-not $folder) { Stop-Run "No $($Label.ToLower()) folder given: type or paste its path." }
    return $folder
}

# ------------------------------------------------------------------------------------------------
# Build
# ------------------------------------------------------------------------------------------------

function Test-BuildNeeded {
    <# Returns $true when the release executable is missing or older than a source file. #>
    if (-not (Test-Path -LiteralPath $Executable)) { return $true }
    $builtAt = (Get-Item -LiteralPath $Executable).LastWriteTimeUtc
    $newest = $SourceDirs |
        ForEach-Object { Get-ChildItem -LiteralPath (Join-Path $ProjectDir $_) -Recurse -File } |
        Sort-Object LastWriteTimeUtc -Descending | Select-Object -First 1
    return $newest.LastWriteTimeUtc -gt $builtAt
}

function Build-Executable {
    <# Builds the release executable when needed; fails with a hint when it cannot. #>
    Write-Step 'Build'
    if ($NoBuild -or -not (Test-BuildNeeded)) {
        if (-not (Test-Path -LiteralPath $Executable)) {
            Stop-Run "Executable missing: $Executable (drop -NoBuild to build it)."
        }
        Write-Host "  up to date: $Executable"
        return
    }
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        Stop-Run 'cargo not found: install Rust from https://rustup.rs, or pass -NoBuild with a build.'
    }
    & cargo build --release --manifest-path (Join-Path $ProjectDir 'Cargo.toml')
    if ($LASTEXITCODE -ne 0) { Stop-Run 'Build failed: fix the errors above, then run again.' }
}

# ------------------------------------------------------------------------------------------------
# Scan
# ------------------------------------------------------------------------------------------------

function Get-ScanArguments {
    <# Returns the scan command-line arguments built from this script's parameters. #>
    $arguments = @('scan', '--source', $script:Source, '--destination', $script:Destination)
    if ($Mode) { $arguments += @('--mode', $Mode) }
    if ($Config) { $arguments += @('--config', $Config) }
    if ($ReportsDir) { $arguments += @('--reports-dir', $ReportsDir) }
    if ($WorkDir) { $arguments += @('--work-dir', $WorkDir) }
    if ($Threads) { $arguments += @('--threads', $Threads) }
    foreach ($stream in $IgnoreStream) { $arguments += @('--ignore-stream', $stream) }
    if ($IncludeEmptyFiles) { $arguments += '--include-empty-files' }
    if ($DeleteReadOnly) { $arguments += '--delete-read-only' }
    if ($RemoveSvnMetadata) { $arguments += '--remove-svn-metadata' }
    return $arguments
}

function Invoke-Scan {
    <# Runs the scan, shows its output and returns the plan path it printed. #>
    Write-Step 'Scan (nothing deleted)'
    & $Executable @(Get-ScanArguments) | Tee-Object -Variable scanLines | Out-Host
    if ($LASTEXITCODE -ne 0) { Stop-Run 'Scan failed: see the error above.' }
    $planLine = $scanLines | Where-Object { $_ -match $PlanLinePattern } | Select-Object -Last 1
    if (-not $planLine) { Stop-Run 'Scan output names no plan file: run the scan by hand to investigate.' }
    $null = $planLine -match $PlanLinePattern
    return $Matches['path']
}

function Read-Plan {
    <# Reads plan.json and returns its confirmation code and planned item count (deletions, moves, metadata). #>
    param([string]$PlanPath)
    $plan = Get-Content -LiteralPath $PlanPath -Raw -Encoding UTF8 | ConvertFrom-Json
    $deletionCount = @($plan.body.actions).Where({ $_ }).Count
    $moveCount = @($plan.body.moves).Where({ $_ }).Count
    $metadataCount = @($plan.body.metadata_removals).Where({ $_ }).Count
    return @{
        Code      = $plan.plan_id.Substring(0, $ConfirmationLength)
        ItemCount = $deletionCount + $moveCount + $metadataCount
    }
}

# ------------------------------------------------------------------------------------------------
# Apply
# ------------------------------------------------------------------------------------------------

function Invoke-DryRun {
    <# Runs every check without deleting and opens the dry-run report. #>
    param([string]$PlanPath)
    Write-Step 'Dry run (nothing deleted)'
    & $Executable apply --plan $PlanPath --dry-run
    if ($LASTEXITCODE -ne 0) { Stop-Run 'Dry run failed: see the error above.' }
    Open-Report (Join-Path (Split-Path -Parent $PlanPath) $DryRunReportName)
}

function Invoke-Apply {
    <# Applies the plan (deletions, and moves in merge mode) after the user types its confirmation code. #>
    param([string]$PlanPath, [string]$Code)
    Write-Step 'Apply (permanent deletions, no Recycle Bin; moves in merge mode)'
    $typed = Read-Host "Type the confirmation code $Code to apply, or press Enter to stop"
    if ([string]::IsNullOrWhiteSpace($typed)) { Stop-Run 'Stopped: nothing deleted or moved.' $ExitSuccess }
    & $Executable apply --plan $PlanPath --confirm $typed.Trim()
    $applyExitCode = $LASTEXITCODE
    Open-Report (Join-Path (Split-Path -Parent $PlanPath) $ApplyReportName)
    exit $applyExitCode
}

# ------------------------------------------------------------------------------------------------
# Entry point
# ------------------------------------------------------------------------------------------------

function Main {
    $lastRun = Read-LastRun
    $script:Source = Resolve-Folder 'Source' $Source $lastRun.Source
    $script:Destination = Resolve-Folder 'Destination' $Destination $lastRun.Destination
    Save-LastRun $script:Source $script:Destination
    Build-Executable
    $planPath = Invoke-Scan
    $plan = Read-Plan $planPath
    if ($plan.ItemCount -eq 0) { Stop-Run 'Nothing to delete or move: the plan is empty.' $ExitSuccess }
    Open-Report (Join-Path (Split-Path -Parent $planPath) $ScanReportName)
    if (-not (Confirm-Choice 'Report reviewed. Run the dry run?')) {
        Stop-Run 'Stopped: nothing deleted.' $ExitSuccess
    }
    Invoke-DryRun $planPath
    Invoke-Apply $planPath $plan.Code
}

Main
