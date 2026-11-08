<#
.SYNOPSIS
    Rijo Code CLI installer for Windows - by Rijoda Labs.

.DESCRIPTION
    The Windows counterpart of install.sh. Builds from this checkout and installs
    into %LOCALAPPDATA%\Programs\rijo (no admin, no system directories touched),
    then makes the binary runnable:

    - the ONNX Runtime DLL is placed next to rijo.exe. The crate is built with
      load-dynamic, so the runtime is loaded at start-up; without the DLL the
      brain and the router cannot load. On Linux the runtime is fetched at build
      time, on Windows it ships next to the binary - that is the trade the
      per-target feature split makes (see docs/GOAL-100.md, section Z).
    - the embedding model (~130 MB) is pre-downloaded so the first turn never
      waits for it.

    If script execution is restricted on this machine (it is the default on
    Windows), run it as:
        powershell -NoProfile -ExecutionPolicy Bypass -File .\install.ps1

.PARAMETER Prefix
    Install directory. Default: %LOCALAPPDATA%\Programs\rijo

.PARAMETER SkipModels
    Do not pre-download the embedding model (~130 MB).

.PARAMETER SkipOnnx
    Do not fetch onnxruntime.dll. The CLI still installs, but the brain and the
    router will report themselves as unavailable at run time.

.PARAMETER DryRun
    Show what would happen and touch nothing.

.EXAMPLE
    powershell -NoProfile -ExecutionPolicy Bypass -File .\install.ps1
    powershell -NoProfile -ExecutionPolicy Bypass -File .\install.ps1 -SkipModels
    powershell -NoProfile -ExecutionPolicy Bypass -File .\install.ps1 -DryRun
#>
[CmdletBinding()]
param(
    [string] $Prefix = "$env:LOCALAPPDATA\Programs\rijo",
    [switch] $SkipModels,
    [switch] $SkipOnnx,
    [switch] $DryRun
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# An installer must not be interactive. --download-models itself no longer
# triggers onboarding (see main.rs), but the trust prompt is a separate gate and
# this script must never wait on stdin: it sets both up front, for every call.
$env:RIJO_SKIP_ONBOARD = '1'
$env:RIJO_SKIP_TRUST = '1'

# Pinned: `ort` 2.0.0-rc.13 requires API 24 and rejects older runtimes with
# BadVersion (measured 2026-09-29: 1.20.1 is refused, 1.28.0 loads). Do not lower
# this without running `rijo --download-models`, which does a real embed.
$OnnxVersion = '1.28.0'

function Write-Info($msg) { Write-Host ":: $msg" -ForegroundColor Cyan }
function Write-Warn($msg) { Write-Host "!! $msg" -ForegroundColor Yellow }
function Write-Err($msg)  { Write-Host "error: $msg" -ForegroundColor Red; exit 1 }

function Test-Prereq($name, $probe) {
    if (-not (Get-Command $probe -ErrorAction SilentlyContinue)) {
        Write-Err "$name not found (looked for '$probe'). Install it first."
    }
}

# Windows locks a running executable, so replacing it fails with a bare
# IOException from Copy-Item. That is not a rare case here — the user is usually
# running rijo in another window while updating — so say what to do instead.
# Checked before the build (fail fast) and again before the copy (a process can
# start during a four-minute build).
function Assert-NotRunning {
    $running = @(Get-Process -Name 'rijo' -ErrorAction SilentlyContinue)
    if ($running.Count -gt 0) {
        $pids = ($running | ForEach-Object { $_.Id }) -join ', '
        Write-Err "rijo is running (PID $pids) and cannot be replaced. Close it and run the installer again."
    }
}

Assert-NotRunning

$binDir = Join-Path $Prefix 'bin'
$exe    = Join-Path $binDir 'rijo.exe'
$dll    = Join-Path $binDir 'onnxruntime.dll'

if ($DryRun) {
    Write-Info 'would: cargo build --release -p rijo'
    Write-Info "would: install  $exe"
    if (-not $SkipOnnx)   { Write-Info "would: fetch   onnxruntime.dll ($OnnxVersion)" }
    if (-not $SkipModels) { Write-Info 'would: run     rijo --download-models (~130 MB)' }
    Write-Info "would: add $binDir to the user PATH"
    exit 0
}

Write-Info 'Prerequisites'
Test-Prereq 'git'   'git'
Test-Prereq 'cargo' 'cargo'
Write-Info "  $(& cargo --version)"

Write-Info 'Building (release, LTO - the first build takes minutes)'
Push-Location $PSScriptRoot
try {
    & cargo build --release -p rijo
    if ($LASTEXITCODE -ne 0) { Write-Err 'build failed' }
} finally {
    Pop-Location
}

$built = Join-Path $PSScriptRoot 'target\release\rijo.exe'
if (-not (Test-Path $built)) { Write-Err "the build produced no $built" }

New-Item -ItemType Directory -Force -Path $binDir | Out-Null
Assert-NotRunning
Copy-Item $built $exe -Force
Write-Info "installed $exe"

# --- ONNX Runtime -----------------------------------------------------------
# Reuse a DLL that already sits next to the built binary (a developer checkout
# often has one), otherwise download the official release once.
$sourceDll = Join-Path $PSScriptRoot 'target\release\onnxruntime.dll'
if (Test-Path $sourceDll) {
    Copy-Item $sourceDll $dll -Force
    Write-Info 'onnxruntime.dll taken from the checkout'
} elseif (-not $SkipOnnx) {
    $zip = Join-Path $env:TEMP "onnxruntime-win-x64-$OnnxVersion.zip"
    $extract = Join-Path $env:TEMP "onnxruntime-$OnnxVersion"
    $url = "https://github.com/microsoft/onnxruntime/releases/download/v$OnnxVersion/onnxruntime-win-x64-$OnnxVersion.zip"
    Write-Info "Fetching ONNX Runtime $OnnxVersion (~60 MB, one-off)"
    try {
        Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing
        if (Test-Path $extract) { Remove-Item $extract -Recurse -Force }
        Expand-Archive -Path $zip -DestinationPath $extract -Force
        $found = Get-ChildItem $extract -Recurse -Filter 'onnxruntime.dll' |
            Select-Object -First 1
        if (-not $found) { throw "no onnxruntime.dll inside $zip" }
        Copy-Item $found.FullName $dll -Force
        Write-Info "installed onnxruntime.dll ($OnnxVersion)"
    } catch {
        Write-Warn "could not fetch the ONNX Runtime: $($_.Exception.Message)"
        Write-Warn 'the CLI works, but brain and router stay unavailable.'
        Write-Warn 'Place onnxruntime.dll next to rijo.exe, or set ORT_DYLIB_PATH.'
    }
} else {
    Write-Warn '-SkipOnnx: brain and router will report themselves unavailable'
}

# --- PATH -------------------------------------------------------------------
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (-not $userPath) { $userPath = '' }
if ($userPath -notlike "*$binDir*") {
    $joined = "$userPath;$binDir"
    [Environment]::SetEnvironmentVariable('Path', $joined.TrimStart(';'), 'User')
    Write-Info "added $binDir to the user PATH (new terminals see it)"
} else {
    Write-Info "$binDir is already on the user PATH"
}

# --- Embedding model --------------------------------------------------------
if (-not $SkipModels) {
    Write-Info 'Pre-downloading the embedding model (~130 MB, one-off)'
    & $exe --download-models
    # Not fatal: the installer stays usable offline, the download is retried on
    # first use - the same behaviour as install.sh.
    if ($LASTEXITCODE -ne 0) { Write-Warn 'model download failed - it retries on first use' }
}

# --- Verify, with the environment the installer set -------------------------
Write-Info 'Checking the installed binary'
$version = (& $exe --version 2>&1) -join ' '
if ($LASTEXITCODE -ne 0) { Write-Err "the installed binary did not start: $version" }
$tools = (& $exe --tools 2>&1 | Select-Object -First 1)
Write-Host ''
Write-Info "done - $version"
Write-Info "       $tools"
Write-Info 'start it with:  rijo'
if (-not (Test-Path $dll)) {
    Write-Warn 'onnxruntime.dll is missing - brain and router will be unavailable'
}
