#requires -Version 5.1
<#
.SYNOPSIS
    Build the Stream To Speaker installer end-to-end.

.DESCRIPTION
    Drives:
      1. msbuild on the driver solution (Release|x64)
      2. cargo build --release on the service crate
      3. ISCC.exe on the Inno Setup script

    Each step skipped with -SkipDriver / -SkipService / -SkipInstaller
    if you want to iterate on one part.

.PARAMETER Configuration
    Driver build configuration. Default: Release.

.PARAMETER Version
    Version string baked into the installer filename and AppVersion.

.EXAMPLE
    .\installer\build-installer.ps1

.EXAMPLE
    .\installer\build-installer.ps1 -Version 0.1.0-rc.1 -SkipDriver
#>
[CmdletBinding()]
param(
    [string]$Configuration = "Release",
    [string]$Version = "0.1.0",
    [switch]$SkipDriver,
    [switch]$SkipService,
    [switch]$SkipInstaller
)

$ErrorActionPreference = "Stop"
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
Set-Location $repoRoot

function Write-Step($msg) {
    Write-Host ""
    Write-Host "==> $msg" -ForegroundColor Cyan
}

# ---------------------------------------------------------------------------
# 1. Driver
# ---------------------------------------------------------------------------
if (-not $SkipDriver) {
    Write-Step "Building driver ($Configuration|x64)"
    $msbuild = Get-Command msbuild.exe -ErrorAction SilentlyContinue
    if (-not $msbuild) {
        $candidates = @(
            "${env:ProgramFiles}\Microsoft Visual Studio\2022\Enterprise\MSBuild\Current\Bin\MSBuild.exe",
            "${env:ProgramFiles}\Microsoft Visual Studio\2022\Professional\MSBuild\Current\Bin\MSBuild.exe",
            "${env:ProgramFiles}\Microsoft Visual Studio\2022\Community\MSBuild\Current\Bin\MSBuild.exe",
            "${env:ProgramFiles(x86)}\Microsoft Visual Studio\2022\BuildTools\MSBuild\Current\Bin\MSBuild.exe"
        )
        $msbuild = $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
        if (-not $msbuild) {
            throw "msbuild.exe not found on PATH or in standard VS2022 locations. Install VS 2022 Build Tools + WDK."
        }
        $msbuildExe = $msbuild
    } else {
        $msbuildExe = $msbuild.Source
    }
    & $msbuildExe (Join-Path $repoRoot "driver\StreamToSpeaker.sln") `
        "/p:Configuration=$Configuration" `
        "/p:Platform=x64" `
        "/nologo" `
        "/verbosity:minimal"
    if ($LASTEXITCODE -ne 0) { throw "Driver build failed (exit $LASTEXITCODE)" }
} else {
    Write-Step "Skipping driver build (-SkipDriver)"
}

# ---------------------------------------------------------------------------
# 2. Service
# ---------------------------------------------------------------------------
if (-not $SkipService) {
    Write-Step "Building service (cargo --release)"
    Push-Location (Join-Path $repoRoot "service")
    try {
        & cargo build --release
        if ($LASTEXITCODE -ne 0) { throw "Service build failed (exit $LASTEXITCODE)" }
    } finally {
        Pop-Location
    }
} else {
    Write-Step "Skipping service build (-SkipService)"
}

# ---------------------------------------------------------------------------
# 3. Stage artifacts into installer\staging\ so the .iss has a stable
#    path to source from (decouples it from WDK / Cargo output dirs).
# ---------------------------------------------------------------------------
Write-Step "Staging artifacts"
$staging = Join-Path $repoRoot "installer\staging"
if (Test-Path $staging) { Remove-Item -Recurse -Force $staging }
New-Item -ItemType Directory -Path $staging | Out-Null

# Service binary
$svcExe = Join-Path $repoRoot "service\target\release\stream-to-speaker.exe"
if (-not (Test-Path $svcExe)) { throw "Service binary not found at $svcExe" }
Copy-Item $svcExe (Join-Path $staging "stream-to-speaker.exe")

# Driver files — scan the build tree for .sys / .inf / .cat with the
# expected basenames so we tolerate WDK output-path variation.
$driverRoot = Join-Path $repoRoot "driver\x64\$Configuration"
$wanted = @("StreamToSpeaker.sys", "StreamToSpeaker.inf", "StreamToSpeaker.cat")
foreach ($name in $wanted) {
    $found = Get-ChildItem -Path $driverRoot -Recurse -Filter $name -ErrorAction SilentlyContinue `
             | Select-Object -First 1
    if ($found) {
        Copy-Item $found.FullName (Join-Path $staging $name) -Force
        Write-Host "  staged $($found.FullName)"
    } else {
        # Missing .cat is OK if the driver wasn't signed; missing .sys/.inf is fatal.
        if ($name -eq "StreamToSpeaker.cat") {
            Write-Host "  (skipping $name - not produced; the driver wasn't signed)"
        } else {
            throw "Required driver artifact $name not found under $driverRoot"
        }
    }
}

# ---------------------------------------------------------------------------
# 4. Installer
# ---------------------------------------------------------------------------
if (-not $SkipInstaller) {
    Write-Step "Building installer (Inno Setup)"
    $isccCandidates = @(
        "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
        "${env:ProgramFiles}\Inno Setup 6\ISCC.exe"
    )
    $iscc = $isccCandidates | Where-Object { Test-Path $_ } | Select-Object -First 1
    if (-not $iscc) {
        throw "ISCC.exe (Inno Setup 6) not found. Install from https://jrsoftware.org/isdl.php - or 'choco install innosetup'."
    }
    $iss = Join-Path $repoRoot "installer\StreamToSpeaker.iss"
    & $iscc "/DAppVersion=$Version" $iss
    if ($LASTEXITCODE -ne 0) { throw "Installer build failed (exit $LASTEXITCODE)" }

    $outDir = Join-Path $repoRoot "installer\out"
    $artifact = Get-ChildItem -Path $outDir -Filter "StreamToSpeakerSetup-*.exe" `
        | Sort-Object LastWriteTime -Descending | Select-Object -First 1
    if ($artifact) {
        Write-Host ""
        Write-Host "Installer: $($artifact.FullName)" -ForegroundColor Green
        Write-Host "Size: $([math]::Round($artifact.Length / 1MB, 2)) MB" -ForegroundColor Green
    }
} else {
    Write-Step "Skipping installer build (-SkipInstaller)"
}

Write-Host ""
Write-Host "All steps complete." -ForegroundColor Green
