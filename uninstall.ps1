<#
.SYNOPSIS
    Remove a per-user `browser` install made by install.ps1.

.DESCRIPTION
    Removes the Start Menu shortcut, the PATH entry and the executables. Browser data
    (profile, sessions, settings) is kept.

.PARAMETER InstallDir
    The install directory to remove. Must match what install.ps1 used.
    Default: %LOCALAPPDATA%\Programs\browser
#>
[CmdletBinding()]
param(
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\browser')
)

$ErrorActionPreference = 'Stop'

# Start Menu shortcut.
$lnk = Join-Path ([Environment]::GetFolderPath('Programs')) 'browser.lnk'
if (Test-Path $lnk) {
    Remove-Item $lnk -Force
    Write-Host "Removed shortcut: $lnk"
}

# User PATH entry.
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if ($userPath) {
    $kept = $userPath -split ';' | Where-Object { $_ -and $_ -ne $InstallDir }
    [Environment]::SetEnvironmentVariable('Path', ($kept -join ';'), 'User')
    Write-Host "Removed from user PATH."
}

# Files. Older builds kept the browser profile (cookies, logins) next to the exe as
# browser.exe.WebView2; never delete it. Move it to where current builds look for it
# (a newer browser moves it there itself on first run), or leave it in place.
if (Test-Path $InstallDir) {
    foreach ($exe in 'browser.exe', 'browser-pty-host.exe') {
        $f = Join-Path $InstallDir $exe
        if (Test-Path $f) { Remove-Item $f -Force }
    }
    $legacy = Join-Path $InstallDir 'browser.exe.WebView2'
    $profile = Join-Path $env:LOCALAPPDATA 'browser\data\WebView2'
    if (Test-Path $legacy) {
        if (-not (Test-Path $profile)) {
            New-Item -ItemType Directory -Force -Path (Split-Path $profile) | Out-Null
            Move-Item $legacy $profile
            Write-Host "Moved your browser profile to $profile"
        } else {
            Write-Host "Kept an older browser profile at $legacy (a newer one exists at $profile)."
        }
    }
    if (-not (Get-ChildItem -Force $InstallDir)) {
        Remove-Item $InstallDir -Force
    }
    Write-Host "Removed the browser from $InstallDir"
}

Write-Host "browser uninstalled."
