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
    [ValidateSet("Off", "TestSign")]
    [string]$SignMode = "TestSign",
    # Override the auto-bumped driver build number. Useful for local
    # experiments where you want a stable build= line in the service log.
    [int]$DriverBuild = 0,
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
    # Stamp the driver build number — git commit count by default. The
    # SAME number ends up in:
    #   - driver.h's STREAM_TO_SPEAKER_DRIVER_BUILD (returned via IOCTL,
    #     logged by the service: 'driver opened build=N')
    #   - the INF's DriverVer (1.0.0.N — what Device Manager shows,
    #     what PnP uses to compare versions)
    # Override with -DriverBuild N for reproducible local repro.
    $buildNum = if ($DriverBuild -gt 0) { $DriverBuild } else { [int]((git rev-list --count HEAD).Trim()) }
    $driverHeader = Join-Path $repoRoot "driver\driver.h"
    $content = Get-Content $driverHeader -Raw
    $new = $content -replace '(STREAM_TO_SPEAKER_DRIVER_BUILD\s+)\d+u', "`${1}${buildNum}u"
    Set-Content -Path $driverHeader -Value $new -NoNewline
    Write-Step "Building driver ($Configuration|x64, SignMode=$SignMode, build=$buildNum, DriverVer=1.0.0.$buildNum)"
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
    if ($SignMode -eq "TestSign") {
        # vcxproj's TestSign target uses signtool /a — picks the first
        # code-signing cert it finds in CurrentUser\My. Warn if none.
        $certs = Get-ChildItem Cert:\CurrentUser\My `
            | Where-Object { $_.EnhancedKeyUsageList | Where-Object { $_.ObjectId -eq "1.3.6.1.5.5.7.3.3" } }
        if (-not $certs) {
            Write-Warning "No code-signing cert in CurrentUser\My; the TestSign step will fail. Create one with New-SelfSignedCertificate, or pass -SignMode Off to skip signing."
        }
    }
    & $msbuildExe (Join-Path $repoRoot "driver\StreamToSpeaker.sln") `
        "/p:Configuration=$Configuration" `
        "/p:Platform=x64" `
        "/p:SignMode=$SignMode" `
        "/p:DriverBuildNumber=$buildNum" `
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

# devcon.exe from the WDK — needed at install time to add the
# root-enumerated StreamToSpeaker device. Search common WDK locations
# and copy the x64 build into the staging dir.
$devconCandidates = Get-ChildItem `
    -Path "C:\Program Files (x86)\Windows Kits\10\Tools" `
    -Recurse `
    -Filter "devcon.exe" `
    -ErrorAction SilentlyContinue `
    | Where-Object { $_.FullName -match "\\x64\\" }
$devcon = $devconCandidates | Select-Object -First 1
if (-not $devcon) {
    throw "devcon.exe not found in the WDK install (looked under C:\Program Files (x86)\Windows Kits\10\Tools)."
}
Copy-Item $devcon.FullName (Join-Path $staging "devcon.exe") -Force
Write-Host "  staged $($devcon.FullName)"

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
    # Derive the strict X.Y.Z.W numeric version that the PE resource
    # format requires — Inno's VersionInfoVersion rejects pre-release
    # suffixes like "-rc.1" or "-abc1234".
    $prefix = ($Version -split "[-+]")[0]
    $parts = $prefix -split "\."
    while ($parts.Count -lt 4) { $parts += "0" }
    $parts = $parts | ForEach-Object { if ($_ -match "^\d+$") { $_ } else { "0" } }
    $viVersion = ($parts[0..3]) -join "."

    $iss = Join-Path $repoRoot "installer\StreamToSpeaker.iss"
    & $iscc "/DAppVersion=$Version" "/DVersionInfoVersion=$viVersion" $iss
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
