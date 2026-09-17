[CmdletBinding()]
param(
    [ValidateSet('Check', 'Test', 'Build', 'Run', 'Smoke')]
    [string]$Action = 'Run',
    [string]$Url,
    [switch]$UseLocalLinker,
    [ValidateRange(1, 64)]
    [int]$Jobs = 1,
    [string]$TargetDirectory = (Join-Path $PSScriptRoot '../../target/servo-lab')
)
$ErrorActionPreference = 'Stop'
$target = [IO.Path]::GetFullPath($TargetDirectory)
$manifest = Join-Path $PSScriptRoot 'Cargo.toml'
$localClang = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../../target/servo-tools/clang/native'))
$previousClang = $env:LIBCLANG_PATH
try {
    if (-not $env:LIBCLANG_PATH -and (Test-Path (Join-Path $localClang 'libclang.dll'))) {
        $env:LIBCLANG_PATH = $localClang
    }
    if ($Url -and $Action -ne 'Run') { throw '-Url is only valid with -Action Run.' }
    $verb = if ($Action -eq 'Check') { 'check' } else { 'build' }
    if ($UseLocalLinker -and $Action -notin @('Check', 'Test')) {
        $tools = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../../target/servo-tools'))
        $linker = Join-Path $tools 'msvc-linker/Contents/VC/Tools/MSVC/14.44.35207/bin/Hostx64/x64/link.exe'
        $cpp = Join-Path $tools 'msvc-onecore/Contents/VC/Tools/MSVC/14.44.35207/lib/onecore/x64'
        $crt = Join-Path $tools 'msvc-crt/Contents/VC/Tools/MSVC/14.44.35207/lib/x64'
        foreach ($required in @($linker, (Join-Path $cpp 'msvcprt.lib'), (Join-Path $crt 'libcmt.lib'))) {
            if (-not (Test-Path -LiteralPath $required)) { throw 'Run setup-linker.ps1 first.' }
        }
        # These flags affect the final lab executable only, not every Rust dependency.
        & cargo rustc --manifest-path $manifest --target-dir $target --locked -j $Jobs `
            --bin browser-servo-lab -- "-Clinker=$linker" '-L' "native=$cpp" '-L' "native=$crt"
    } elseif ($Action -eq 'Test') {
        # Protocol, input ownership and policy tests don't link Servo into their executable.
        & cargo test --lib --manifest-path $manifest --target-dir $target --locked -j $Jobs
    } else {
        & cargo $verb --manifest-path $manifest --target-dir $target --locked -j $Jobs
    }
    if ($LASTEXITCODE -ne 0) { throw "Cargo $verb failed ($LASTEXITCODE)." }
    $exe = Join-Path $target 'debug/browser-servo-lab.exe'
    if ($Action -eq 'Run') {
        if ($Url) { & $exe $Url } else { & $exe }
        if ($LASTEXITCODE -ne 0) { throw "Lab failed ($LASTEXITCODE)." }
    } elseif ($Action -eq 'Smoke') {
        $stdout = Join-Path $target 'smoke.stdout.log'
        $stderr = Join-Path $target 'smoke.stderr.log'
        $process = Start-Process -FilePath $exe -ArgumentList '--smoke' -PassThru -WindowStyle Hidden `
            -RedirectStandardOutput $stdout -RedirectStandardError $stderr
        # External deadline also catches hangs during runtime initialization or Drop,
        # when the lab cannot service its own event-loop timer.
        if (-not $process.WaitForExit(90000)) {
            Stop-Process -Id $process.Id -Force
            throw "Lab exceeded 90 seconds, including teardown. See $stderr"
        }
        $process.Refresh()
        Get-Content -LiteralPath $stdout
        if ($process.ExitCode -ne 0) {
            Get-Content -LiteralPath $stderr -Tail 40
            throw "Smoke failed ($($process.ExitCode))."
        }
    }
} finally {
    $env:LIBCLANG_PATH = $previousClang
}
