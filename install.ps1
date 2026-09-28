<#
.SYNOPSIS
    Build `browser` from this checkout and install it for the current user.

.DESCRIPTION
    For development: end users should use the installer from GitHub Releases.

    Builds an optimized WebView2 release by default, copies browser.exe and its
    browser-pty-host.exe companion to a per-user install directory, creates a
    Start Menu shortcut named "browser", and adds the install dir to the user PATH
    so you can launch it by typing `browser`.

    -Servo instead installs the experimental WebView2 + Servo development build
    produced by run-servo.ps1 (it needs the Servo lab's native tools).

    Everything is per-user (no admin needed) and reversible via uninstall.ps1.

.PARAMETER InstallDir
    Where to install. Default: %LOCALAPPDATA%\Programs\browser

.PARAMETER Servo
    Install the WebView2 + Servo development build instead of the release build.

.PARAMETER WebView2Only
    Compatibility flag: the release build is now the default.

.PARAMETER NoBuild
    Use existing binaries: target\release normally, target\servo-lab\debug with -Servo.

.PARAMETER NoPath
    Don't modify the user PATH.

.PARAMETER NoShortcut
    Don't create the Start Menu shortcut.

.EXAMPLE
    pwsh -File install.ps1

.EXAMPLE
    pwsh -File install.ps1 -Servo
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

Write-Host "browser installer"
Write-Host "  repo:    $repo"
Write-Host "  install: $InstallDir"

$rel = Join-Path $repo $(if ($Servo) { 'target\servo-lab\debug' } else { 'target\release' })
Write-Host "  engines: $(if ($Servo) { 'WebView2 + Servo (development build)' } else { 'WebView2 (release build)' })"

if ($Servo -and -not $NoBuild) {
    Write-Host "`nBuilding WebView2 + Servo..."
    $localLinker = Join-Path $repo 'target/servo-tools/msvc-linker/Contents/VC/Tools/MSVC/14.44.35207/bin/Hostx64/x64/link.exe'
    & (Join-Path $repo 'run-servo.ps1') -Action Build -UseLocalLinker:(Test-Path -LiteralPath $localLinker)
} elseif (-not $NoBuild) {
    Write-Host "`nBuilding release binaries..."
    Push-Location $repo
    try {
        cargo build --release --locked -p browser
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed (exit $LASTEXITCODE)" }
    } finally {
        Pop-Location
    }
}

$browser = Join-Path $rel 'browser.exe'
$ptyhost = Join-Path $rel 'browser-pty-host.exe'
foreach ($f in @($browser, $ptyhost)) {
    if (-not (Test-Path $f)) { throw "missing build artifact: $f (drop -NoBuild to build it)" }
}

$installedExe = [IO.Path]::GetFullPath((Join-Path $InstallDir 'browser.exe'))
$running = Get-Process -Name browser -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $installedExe }
if ($running) { throw "Close the installed browser before updating it, then rerun this command: $installedExe" }

New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
# The browser locates 'browser-pty-host.exe' next to its own executable.
Copy-Item $browser (Join-Path $InstallDir 'browser.exe') -Force
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
