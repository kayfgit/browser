#Requires -Version 7
# Replace the bundled uBlock Origin Lite (crates/desktop/extensions/uBOLite.chromium)
# with a release from github.com/uBlockOrigin/uBOL-home. Its filter lists ship inside
# the extension, so a stale copy means stale YouTube/anti-adblock fixes: run this before
# each release. Defaults to the latest release; -Tag picks a specific one.
[CmdletBinding()]
param([string]$Tag)
$ErrorActionPreference = 'Stop'
$api = 'https://api.github.com/repos/uBlockOrigin/uBOL-home/releases/' + ($Tag ? "tags/$Tag" : 'latest')
$release = Invoke-RestMethod -Uri $api -Headers @{ 'User-Agent' = 'browser-update-ubol' }
$asset = $release.assets | Where-Object { $_.name -like '*.chromium.zip' } | Select-Object -First 1
if (-not $asset) { throw "Release $($release.tag_name) has no chromium build." }
if ($asset.digest -notmatch '^sha256:([0-9a-f]{64})$') {
    throw "Release asset $($asset.name) carries no SHA-256 digest to check against."
}
$expected = $Matches[1]

$work = Join-Path ([IO.Path]::GetTempPath()) "update-ubol-$PID"
$zip = Join-Path $work $asset.name
$unpacked = Join-Path $work 'uBOLite.chromium'
[void][IO.Directory]::CreateDirectory($work)
try {
    Write-Host "Downloading $($asset.name)..."
    Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $zip
    if ((Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash -ine $expected) {
        throw "Checksum mismatch for $($asset.name); nothing was replaced."
    }
    [IO.Compression.ZipFile]::ExtractToDirectory($zip, $unpacked)
    $manifest = Get-Content -LiteralPath (Join-Path $unpacked 'manifest.json') -Raw | ConvertFrom-Json
    if ($manifest.name -notmatch 'uBlock Origin Lite' -and $manifest.name -notmatch '__MSG_') {
        throw "The archive doesn't look like uBlock Origin Lite (manifest name '$($manifest.name)')."
    }
    # Chromium writes its own indexes under _metadata when it loads an unpacked extension;
    # they're machine-generated and never belong in the bundle.
    Remove-Item -LiteralPath (Join-Path $unpacked '_metadata') -Recurse -Force -ErrorAction SilentlyContinue

    $target = Join-Path $PSScriptRoot 'crates/desktop/extensions/uBOLite.chromium'
    if (Test-Path -LiteralPath $target) { Remove-Item -LiteralPath $target -Recurse -Force }
    Move-Item -LiteralPath $unpacked -Destination $target
    Write-Host "Bundled uBlock Origin Lite is now $($manifest.version) ($($release.tag_name))."
} finally {
    Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
}
