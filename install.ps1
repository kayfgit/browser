<#
.SYNOPSIS
    Build and install `browser` (the desktop shell) for the current user.

.DESCRIPTION
    Builds the tested WebView2 + Servo development binaries by default,
    copies them to a per-user install directory,
    creates a Start Menu shortcut named "browser", and adds the install dir to
    the user PATH so you can launch it by typing `browser`.

    Uses run-servo.ps1 and automatically selects the local native linker when
    installed by the Servo lab setup. Both variants update the same executable
    and Start Menu shortcut.

    Everything is per-user (no admin needed) and reversible via uninstall.ps1.

.PARAMETER InstallDir
    Where to install. Default: %LOCALAPPDATA%\Programs\browser

.PARAMETER NoBuild
    Use existing binaries: target\servo-lab\debug normally, target\release with -WebView2Only.

.PARAMETER Servo
    Compatibility flag: Servo is now included by default.

.PARAMETER WebView2Only
    Omit Servo and install a WebView2-only release build instead.

.PARAMETER NoPath
    Don't modify the user PATH.

.PARAMETER NoShortcut
    Don't create the Start Menu shortcut.

.EXAMPLE
    pwsh -File install.ps1

.EXAMPLE
    pwsh -File install.ps1 -WebView2Only
#>
[CmdletBinding()]
param(
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\browser'),
    [switch]$Servo,
    [switch]$WebView2Only,
    [switch]$NoBuild,
    [switch]$NoPath,
    [switch]$NoShortcut
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $MyInvocation.MyCommand.Path
if ($Servo -and $WebView2Only) { throw 'Choose either -Servo or -WebView2Only, not both.' }
$includeServo = -not $WebView2Only

Write-Host "browser installer"
Write-Host "  repo:    $repo"
Write-Host "  install: $InstallDir"

$rel = Join-Path $repo $(if ($includeServo) { 'target\servo-lab\debug' } else { 'target\release' })
Write-Host "  engines: $(if ($includeServo) { 'WebView2 + Servo (tested development build)' } else { 'WebView2 (release build)' })"

if ($includeServo -and -not $NoBuild) {
    Write-Host "`nBuilding WebView2 + Servo..."
    $localLinker = Join-Path $repo 'target/servo-tools/msvc-linker/Contents/VC/Tools/MSVC/14.44.35207/bin/Hostx64/x64/link.exe'
    & (Join-Path $repo 'run-servo.ps1') -Action Build -UseLocalLinker:(Test-Path -LiteralPath $localLinker)
} elseif (-not $NoBuild) {
    Write-Host "`nBuilding release binaries..."
    Push-Location $repo
    try {
        cargo build --release -p browser-desktop -p browser-pty-host
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed (exit $LASTEXITCODE)" }
    } finally {
        Pop-Location
    }
}

$desktop = Join-Path $rel 'browser-desktop.exe'
$ptyhost = Join-Path $rel 'browser-pty-host.exe'
foreach ($f in @($desktop, $ptyhost)) {
    if (-not (Test-Path $f)) { throw "missing build artifact: $f (drop -NoBuild to build it)" }
}

$installedExe = [IO.Path]::GetFullPath((Join-Path $InstallDir 'browser.exe'))
$running = Get-Process -Name browser -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $installedExe }
if ($running) { throw "Close the installed browser before updating it, then rerun this command: $installedExe" }

New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
# Install the GUI shell as 'browser.exe'. The companion keeps its name: the shell
# locates 'browser-pty-host.exe' next to its own executable.
Copy-Item $desktop (Join-Path $InstallDir 'browser.exe') -Force
Copy-Item $ptyhost (Join-Path $InstallDir 'browser-pty-host.exe') -Force
Write-Host "Copied browser.exe + browser-pty-host.exe -> $InstallDir"
if ($includeServo) { Write-Host 'For future dual-engine updates, use: pwsh -File install.ps1' }

if (-not $NoShortcut) {
    $programs = [Environment]::GetFolderPath('Programs')
    $lnk = Join-Path $programs 'browser.lnk'
    $ws = New-Object -ComObject WScript.Shell
    $sc = $ws.CreateShortcut($lnk)
    $sc.TargetPath = (Join-Path $InstallDir 'browser.exe')
    $sc.WorkingDirectory = $InstallDir
    $sc.Description = 'browser'
    $sc.Save()
    Write-Host "Created Start Menu shortcut: $lnk"
}

if (-not $NoPath) {
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $parts = if ($userPath) { $userPath -split ';' } else { @() }
    if ($parts -notcontains $InstallDir) {
        $newPath = (@($parts | Where-Object { $_ }) + $InstallDir) -join ';'
        [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
        Write-Host "Added to user PATH (open a new terminal to use 'browser')."
    } else {
        Write-Host "Already on user PATH."
    }
}

Write-Host "`nDone. Launch from the Start Menu ('browser') or run 'browser <url>' in a new terminal."
Write-Host "Uninstall: pwsh -File `"$(Join-Path $repo 'uninstall.ps1')`""
