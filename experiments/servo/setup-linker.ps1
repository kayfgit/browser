#Requires -Version 7
# Optional final-link workaround for an existing VS 2019 build environment.
# Prefer Servo's documented VS 2022 + current SDK setup for adapter development.
[CmdletBinding()]
param()
$ErrorActionPreference = 'Stop'
$tools = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../../target/servo-tools'))
[void][IO.Directory]::CreateDirectory($tools)
$packages = @(
    @{
        Name = 'msvc-linker'
        Hash = 'ee0baaa3a112d255f19f6c27dcc0ff6e496949eb9f1f37be0ac908c562a7076c'
        Url = 'https://download.visualstudio.microsoft.com/download/pr/bbc72d8e-2acd-4229-8f6a-85e23c5e3456/ee0baaa3a112d255f19f6c27dcc0ff6e496949eb9f1f37be0ac908c562a7076c/Microsoft.VC.14.44.17.14.Tools.HostX64.TargetX64.base.vsix'
    },
    @{
        Name = 'msvc-crt'
        Hash = 'f01f701a7bcd9587a340898c851424f6a52bb913a70c185ff0d5bf0288c5831a'
        Url = 'https://download.visualstudio.microsoft.com/download/pr/67cf767c-5e71-47c2-a54a-cd5631e28942/f01f701a7bcd9587a340898c851424f6a52bb913a70c185ff0d5bf0288c5831a/Microsoft.VC.14.44.17.14.CRT.x64.Desktop.base.vsix'
    },
    @{
        Name = 'msvc-onecore'
        Hash = '50d68caa9bba363d6209bf5fc8a572f013c9f370f6e203f36f04e3d84f5083ca'
        Url = 'https://download.visualstudio.microsoft.com/download/pr/67cf767c-5e71-47c2-a54a-cd5631e28942/50d68caa9bba363d6209bf5fc8a572f013c9f370f6e203f36f04e3d84f5083ca/Microsoft.VC.14.44.17.14.CRT.x64.OneCore.Desktop.base.vsix'
    }
)
foreach ($package in $packages) {
    $archive = Join-Path $tools ($package.Name + '.vsix')
    if (-not (Test-Path -LiteralPath $archive)) {
        Write-Host "Downloading $($package.Name) from Microsoft..."
        Invoke-WebRequest -Uri $package.Url -OutFile $archive
    }
    if ((Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash -ine $package.Hash) {
        throw "Checksum mismatch for $archive; no files from it were extracted."
    }
    [IO.Compression.ZipFile]::ExtractToDirectory($archive, (Join-Path $tools $package.Name), $true)
}
$linker = Join-Path $tools 'msvc-linker/Contents/VC/Tools/MSVC/14.44.35207/bin/Hostx64/x64/link.exe'
$signature = Get-AuthenticodeSignature -LiteralPath $linker
if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Subject -notmatch 'O=Microsoft Corporation') {
    throw 'The extracted linker does not have a valid Microsoft signature.'
}
Write-Host 'Verified local linker and C++ libraries. Use run.ps1 -UseLocalLinker.'
