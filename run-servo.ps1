<# Build/run the main browser with its experimental Servo provider. #>
[CmdletBinding()]
param(
    [ValidateSet('Build','Run','Smoke')][string]$Action = 'Run',
    [switch]$UseLocalLinker,
    [string]$Url,
    [switch]$Scratch,
    [ValidateSet('Default','Split','Example')][string]$Scenario = 'Default'
)
$ErrorActionPreference = 'Stop'
$repo = $PSScriptRoot
$target = Join-Path $repo 'target/servo-lab'
$localClang = Join-Path $repo 'target/servo-tools/clang/native'
$previousClang = $env:LIBCLANG_PATH
Push-Location $repo
try {
    if (-not $env:LIBCLANG_PATH -and (Test-Path (Join-Path $localClang 'libclang.dll'))) {
        $env:LIBCLANG_PATH = [IO.Path]::GetFullPath($localClang)
    }
    # Reuse the lab's dev artifacts; normal desktop builds retain their optimization.
    $cargoArgs = @('rustc','-p','browser','--features','servo-engine','--locked','--target-dir',$target,'-j','1',
        '--config','profile.dev.opt-level=0','--config','profile.dev.debug=0','--config','profile.dev.incremental=false',
        '--config','profile.dev.package."*".opt-level=0',
        '--config','profile.dev.package.fontdue.opt-level=2',
        '--config','profile.dev.package.alacritty_terminal.opt-level=2',
        '--config','profile.dev.package.browser.opt-level=1','--bin','browser')
    if ($UseLocalLinker) {
        $tools = Join-Path $repo 'target/servo-tools'
        $linker = Join-Path $tools 'msvc-linker/Contents/VC/Tools/MSVC/14.44.35207/bin/Hostx64/x64/link.exe'
        $cpp = Join-Path $tools 'msvc-onecore/Contents/VC/Tools/MSVC/14.44.35207/lib/onecore/x64'
        $crt = Join-Path $tools 'msvc-crt/Contents/VC/Tools/MSVC/14.44.35207/lib/x64'
        if (-not (Test-Path $linker)) { throw 'Run experiments/servo/setup-linker.ps1 first.' }
        $cargoArgs += @('--',"-Clinker=$linker",'-L',"native=$cpp",'-L',"native=$crt")
    }
    & cargo @cargoArgs
    if ($LASTEXITCODE -ne 0) { throw 'Servo desktop build failed.' }
    # Terminals need their existing companion next to the new browser binary.
    & cargo build -p browser --bin browser-pty-host --locked
    if ($LASTEXITCODE -ne 0) { throw 'PTY companion build failed.' }
    Copy-Item -LiteralPath (Join-Path $repo 'target/debug/browser-pty-host.exe') -Destination (Join-Path $target 'debug/browser-pty-host.exe') -Force
    $exe = Join-Path $target 'debug/browser.exe'
    if ($Action -eq 'Run') {
        $runArgs = @()
        if ($Scratch) { $runArgs += '--scratch' }
        if ($Url) { $runArgs += $Url }
        & $exe @runArgs
        if ($LASTEXITCODE -ne 0) { throw "Browser exited with $LASTEXITCODE" }
    } elseif ($Action -eq 'Smoke') {
        $runDir = Join-Path $target ('desktop-smoke/' + [guid]::NewGuid().ToString('N'))
        New-Item -ItemType Directory -Path $runDir | Out-Null
        $saved = @{}
        foreach ($name in @('BROWSER_SERVO_DATA_DIR','BROWSER_WEBVIEW2_DATA_DIR','BROWSER_SERVO_SMOKE_LOG','BROWSER_SERVO_SMOKE_SCENARIO','BROWSER_TEST_QUIT_MS')) {
            $saved[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
        }
        try {
            $env:BROWSER_SERVO_DATA_DIR = Join-Path $runDir 'servo'
            $env:BROWSER_WEBVIEW2_DATA_DIR = Join-Path $runDir 'webview2'
            $env:BROWSER_SERVO_SMOKE_LOG = Join-Path $runDir 'result.log'
            $env:BROWSER_SERVO_SMOKE_SCENARIO = $Scenario
            $env:BROWSER_TEST_QUIT_MS = '150000'
            $process = Start-Process -FilePath $exe -ArgumentList '--scratch' -PassThru -WindowStyle Hidden `
                -RedirectStandardOutput (Join-Path $runDir 'stdout.log') -RedirectStandardError (Join-Path $runDir 'stderr.log')
            $process.Id | Set-Content -LiteralPath (Join-Path $runDir 'process-id.txt')
            if (-not $process.WaitForExit(180000)) {
                Stop-Process -Id $process.Id -Force
                throw "Main browser exceeded its smoke deadline: $runDir"
            }
            $process.Refresh()
            $result = if (Test-Path $env:BROWSER_SERVO_SMOKE_LOG) { Get-Content $env:BROWSER_SERVO_SMOKE_LOG -Raw } else { '' }
            Write-Output $result
            if ($process.ExitCode -ne 0 -or $result -notmatch '(?m)^PASS ' -or $result -match '(?m)^FAIL ') {
                Get-Content (Join-Path $runDir 'stderr.log') -Tail 35
                throw "Main browser smoke failed: $runDir"
            }
            Write-Output "Smoke profiles and logs: $runDir"
        } finally {
            foreach ($name in $saved.Keys) { [Environment]::SetEnvironmentVariable($name, $saved[$name], 'Process') }
        }
    }
} finally {
    $env:LIBCLANG_PATH = $previousClang
    Pop-Location
}
