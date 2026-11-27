#Requires -Version 7
# Replace the bundled ConPTY (crates/desktop/conpty: conpty.dll + OpenConsole.exe, x64)
# with a release of Microsoft's Microsoft.Windows.Console.ConPTY package from nuget.org,
# the console host Windows Terminal and VS Code ship. Both files must carry a valid
# Microsoft Authenticode signature or nothing is replaced. The update-conpty workflow
# runs this weekly and opens a PR; run it by hand to bump sooner. Defaults to the
# latest stable version; -Version picks a specific one. Windows only (signature check).
[CmdletBinding()]
param([string]$Version)
$ErrorActionPreference = 'Stop'
$id = 'microsoft.windows.console.conpty'
$feed = "https://api.nuget.org/v3-flatcontainer/$id"
if (-not $Version) {
    $versions = (Invoke-RestMethod -Uri "$feed/index.json").versions
    # Stable releases only: previews have a '-' suffix.
    $Version = $versions | Where-Object { $_ -notmatch '-' } | Select-Object -Last 1
    if (-not $Version) { throw 'nuget.org lists no stable ConPTY release.' }
}

$work = Join-Path ([IO.Path]::GetTempPath()) "update-conpty-$PID"
$pkg = Join-Path $work "$id.$Version.nupkg"
$unpacked = Join-Path $work 'pkg'
[void][IO.Directory]::CreateDirectory($work)
try {
    Write-Host "Downloading ConPTY $Version..."
    Invoke-WebRequest -Uri "$feed/$Version/$id.$Version.nupkg" -OutFile $pkg
    [IO.Compression.ZipFile]::ExtractToDirectory($pkg, $unpacked)
    $files = @{
        'conpty.dll'      = Join-Path $unpacked 'runtimes/win-x64/native/conpty.dll'
        'OpenConsole.exe' = Join-Path $unpacked 'build/native/runtimes/x64/OpenConsole.exe'
    }
    foreach ($name in $files.Keys) {
        $path = $files[$name]
        if (-not (Test-Path -LiteralPath $path)) {
            # The layout moved: look for the x64 file anywhere in the package.
            $found = Get-ChildItem -LiteralPath $unpacked -Recurse -File -Filter $name |
                Where-Object { $_.FullName -match '[\\/](win-)?x64[\\/]' } | Select-Object -First 1
            if (-not $found) { throw "The package has no x64 $name." }
            $files[$name] = $path = $found.FullName
        }
        $sig = Get-AuthenticodeSignature -LiteralPath $path
        if ($sig.Status -ne 'Valid' -or $sig.SignerCertificate.Subject -notmatch 'O=Microsoft Corporation') {
            throw "$name isn't validly signed by Microsoft ($($sig.Status), $($sig.SignerCertificate.Subject)); nothing was replaced."
        }
    }

    $target = Join-Path $PSScriptRoot 'crates/desktop/conpty'
    [void][IO.Directory]::CreateDirectory($target)
    foreach ($name in $files.Keys) {
        Copy-Item -LiteralPath $files[$name] -Destination (Join-Path $target $name) -Force
    }
    Set-Content -LiteralPath (Join-Path $target 'VERSION') -Value $Version -NoNewline
    Write-Host "Bundled ConPTY is now $Version."
} finally {
    Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
}
