<# Build/run the main browser with its Servo provider.
   -Release builds the optimized `dist` profile that releases ship (slow, memory-hungry);
   the default is a fast unoptimized development build. #>
[CmdletBinding()]
param(
    [ValidateSet('Build','Run','Smoke')][string]$Action = 'Run',
    [switch]$UseLocalLinker,
    [string]$Url,
    [switch]$Scratch,
    [switch]$Release,
    [ValidateSet('Default','Split','Example','Crash','Visit')][string]$Scenario = 'Default',
    # Visit: load -Url in Servo and record its console, a probe and a screenshot.
    # -NoScripts loads it without the shell's injected scripts, for comparison.
    [switch]$NoScripts,
    # Visit: a script file to evaluate instead of the built-in probe.
    [string]$Probe
)
$ErrorActionPreference = 'Stop'
$repo = $PSScriptRoot
$target = Join-Path $repo $(if ($Release) { 'target/servo-release' } else { 'target/servo-lab' })
$profileDir = if ($Release) { 'dist' } else { 'debug' }
$localClang = Join-Path $repo 'target/servo-tools/clang/native'
$previousClang = $env:LIBCLANG_PATH
Push-Location $repo
try {
    if (-not $env:LIBCLANG_PATH -and (Test-Path (Join-Path $localClang 'libclang.dll'))) {
        $env:LIBCLANG_PATH = [IO.Path]::GetFullPath($localClang)
    }
    if ($Release) {
        $cargoArgs = @('rustc','-p','browser','--features','servo-engine','--locked','--target-dir',$target,'-j','2',
            '--profile','dist','--bin','browser')
    } else {
        # Reuse the lab's dev artifacts; normal desktop builds retain their optimization.
        $cargoArgs = @('rustc','-p','browser','--features','servo-engine','--locked','--target-dir',$target,'-j','1',
            '--config','profile.dev.opt-level=0','--config','profile.dev.debug=0','--config','profile.dev.incremental=false',
            '--config','profile.dev.package."*".opt-level=0',
            '--config','profile.dev.package.fontdue.opt-level=2',
            '--config','profile.dev.package.alacritty_terminal.opt-level=2',
            '--config','profile.dev.package.browser.opt-level=1','--bin','browser')
    }
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
    # @() keeps a one-item list an array; splatting a bare string passes its characters.
    $ptyProfile = @(if ($Release) { '--release' })
    & cargo build -p browser --bin browser-pty-host --locked @ptyProfile
    if ($LASTEXITCODE -ne 0) { throw 'PTY companion build failed.' }
    $ptyDir = if ($Release) { 'target/release' } else { 'target/debug' }
    Copy-Item -LiteralPath (Join-Path $repo "$ptyDir/browser-pty-host.exe") -Destination (Join-Path $target "$profileDir/browser-pty-host.exe") -Force
    $exe = Join-Path $target "$profileDir/browser.exe"
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
        foreach ($name in @('BROWSER_SERVO_DATA_DIR','BROWSER_WEBVIEW2_DATA_DIR','BROWSER_SERVO_SMOKE_LOG','BROWSER_SERVO_SMOKE_SCENARIO','BROWSER_TEST_QUIT_MS','BROWSER_SERVO_SMOKE_URL','BROWSER_SERVO_CONSOLE_LOG','BROWSER_SERVO_NO_SCRIPTS','BROWSER_SERVO_SMOKE_PROBE')) {
            $saved[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
        }
        try {
            $env:BROWSER_SERVO_DATA_DIR = Join-Path $runDir 'servo'
            $env:BROWSER_WEBVIEW2_DATA_DIR = Join-Path $runDir 'webview2'
            $env:BROWSER_SERVO_SMOKE_LOG = Join-Path $runDir 'result.log'
            $env:BROWSER_SERVO_SMOKE_SCENARIO = $Scenario
            if ($Scenario -eq 'Visit') {
                if (-not $Url) { throw '-Scenario Visit needs -Url.' }
                $env:BROWSER_SERVO_SMOKE_URL = $Url
                $env:BROWSER_SERVO_CONSOLE_LOG = Join-Path $runDir 'console.log'
                if ($NoScripts) { $env:BROWSER_SERVO_NO_SCRIPTS = '1' }
                if ($Probe) { $env:BROWSER_SERVO_SMOKE_PROBE = (Resolve-Path -LiteralPath $Probe).Path }
            }
            $env:BROWSER_TEST_QUIT_MS = '150000'
            $process = Start-Process -FilePath $exe -ArgumentList '--scratch' -PassThru -WindowStyle Hidden `
                -RedirectStandardOutput (Join-Path $runDir 'stdout.log') -RedirectStandardError (Join-Path $runDir 'stderr.log')
            $process.Id | Set-Content -LiteralPath (Join-Path $runDir 'process-id.txt')
            if (-not $process.WaitForExit(180000)) {
                Stop-Process -Id $process.Id -Force
                Start-Sleep -Milliseconds 1500
                $left = @(Get-CimInstance Win32_Process -Filter "ParentProcessId=$($process.Id) AND Name='browser.exe'")
                $left | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
                throw "Main browser exceeded its smoke deadline ($($left.Count) content process(es) outlived it): $runDir"
            }
            $process.Refresh()
            # Servo's content processes must end with the browser, however it exited.
            Start-Sleep -Milliseconds 1500
            $orphans = @(Get-CimInstance Win32_Process -Filter "ParentProcessId=$($process.Id) AND Name='browser.exe'")
            if ($orphans.Count) {
                $orphans | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
                throw "$($orphans.Count) Servo content process(es) outlived the browser: $runDir"
            }
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
