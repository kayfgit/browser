<#
.SYNOPSIS
    Build and install `browser` (the desktop shell) for the current user.

.DESCRIPTION
    Builds the release binaries, copies them to a per-user install directory,
    creates a Start Menu shortcut named "browser", and adds the install dir to
    the user PATH so you can launch it by typing `browser`.

    Everything is per-user (no admin needed) and reversible via uninstall.ps1.

.PARAMETER InstallDir
    Where to install. Default: %LOCALAPPDATA%\Programs\browser

.PARAMETER NoBuild
    Skip `cargo build --release` and use the existing target\release binaries.

.PARAMETER NoPath
    Don't modify the user PATH.

.PARAMETER NoShortcut
    Don't create the Start Menu shortcut.

.EXAMPLE
    pwsh -File install.ps1
#>
[CmdletBinding()]
param(
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\browser'),
    [switch]$NoBuild,
    [switch]$NoPath,
    [switch]$NoShortcut
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $MyInvocation.MyCommand.Path

Write-Host "browser installer"
Write-Host "  repo:    $repo"
Write-Host "  install: $InstallDir"

if (-not $NoBuild) {
    Write-Host "`nBuilding release binaries..."
    Push-Location $repo
    try {
        cargo build --release -p browser-desktop -p browser-pty-host
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed (exit $LASTEXITCODE)" }
    } finally {
        Pop-Location
    }
}

$rel = Join-Path $repo 'target\release'
$desktop = Join-Path $rel 'browser-desktop.exe'
$ptyhost = Join-Path $rel 'browser-pty-host.exe'
foreach ($f in @($desktop, $ptyhost)) {
    if (-not (Test-Path $f)) { throw "missing build artifact: $f (drop -NoBuild to build it)" }
}

New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
# Install the GUI shell as 'browser.exe'. The companion keeps its name: the shell
# locates 'browser-pty-host.exe' next to its own executable.
Copy-Item $desktop (Join-Path $InstallDir 'browser.exe') -Force
Copy-Item $ptyhost (Join-Path $InstallDir 'browser-pty-host.exe') -Force
Write-Host "Copied browser.exe + browser-pty-host.exe -> $InstallDir"

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
