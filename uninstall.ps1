<#
.SYNOPSIS
    Remove a per-user `browser` install made by install.ps1.

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

# Files.
if (Test-Path $InstallDir) {
    Remove-Item $InstallDir -Recurse -Force
    Write-Host "Removed $InstallDir"
}

Write-Host "browser uninstalled."
