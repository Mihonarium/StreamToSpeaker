# Make-SubmissionCab.ps1 -- build + EV-sign the Partner Center attestation
# submission CAB for the StreamToSpeaker driver.
#
# Kernel drivers cannot be made loadable by EV-signing the .sys directly
# (signtool verify /kp: "Signing Cert does not chain to a Microsoft Root
# Cert"). The production flow is attestation signing: pack .sys + .inf +
# .pdb into a CAB, sign the CAB with the EV certificate, submit it at
# https://partner.microsoft.com/dashboard/hardware, and Microsoft returns
# the driver package embedded-signed with their certificate plus a fresh
# Microsoft-signed .cat (any .cat we submit is discarded and regenerated).
#
# Usage:
#   .\Make-SubmissionCab.ps1 -BinariesDir C:\path\to\binaries
# where BinariesDir contains StreamToSpeaker.sys / .inf / .pdb from ONE CI
# run (the "binaries-<version>" workflow artifact). Mixing files from
# different runs breaks the .pdb <-> .sys pairing and can desync INF
# DriverVer from the .sys.

param(
    [Parameter(Mandatory = $true)]
    [string]$BinariesDir,

    # SHA1 thumbprint of the EV code-signing cert. Defaults to auto-detecting
    # a Certum EV cert with a private key in the CurrentUser\My store.
    [string]$Thumbprint,

    # RFC3161 timestamp server. Certum's own is the default; DigiCert's
    # (http://timestamp.digicert.com) works with any cert as a fallback.
    [string]$TimestampUrl = "http://time.certum.pl",

    [string]$OutDir
)

$ErrorActionPreference = "Stop"
$BinariesDir = (Resolve-Path $BinariesDir).Path
if (-not $OutDir) { $OutDir = Join-Path $BinariesDir "submission" }

# --- gather inputs ---------------------------------------------------------
$sys = Join-Path $BinariesDir "StreamToSpeaker.sys"
$inf = Join-Path $BinariesDir "StreamToSpeaker.inf"
$pdb = Join-Path $BinariesDir "StreamToSpeaker.pdb"
foreach ($f in $sys, $inf) {
    if (-not (Test-Path $f)) { throw "Required file missing: $f" }
}
$havePdb = Test-Path $pdb
if (-not $havePdb) {
    Write-Warning ".pdb not found -- Partner Center wants the symbol file in the CAB (used by Microsoft's crash-analysis tooling). Older CI runs didn't upload it; re-run CI or proceed without it at your own risk."
}

# Report the driver version from the INF so the submission is traceable.
$driverVer = (Select-String -Path $inf -Pattern 'DriverVer\s*=\s*[^,]+,\s*([\d.]+)').Matches |
    Select-Object -First 1
$version = if ($driverVer) { $driverVer.Groups[1].Value } else { "unknown" }
Write-Host "Driver version (INF DriverVer): $version"

# --- locate signtool -------------------------------------------------------
$signtool = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin" -Recurse -Filter signtool.exe -ErrorAction SilentlyContinue |
    Where-Object { $_.FullName -match '\\x64\\' } |
    Sort-Object FullName -Descending | Select-Object -First 1 -ExpandProperty FullName
if (-not $signtool) { throw "signtool.exe not found under Windows Kits -- install a Windows SDK." }

# --- locate the EV cert ----------------------------------------------------
if (-not $Thumbprint) {
    $cert = Get-ChildItem Cert:\CurrentUser\My |
        Where-Object { $_.HasPrivateKey -and $_.Issuer -match 'Certum Extended Validation' } |
        Sort-Object NotAfter -Descending | Select-Object -First 1
    if (-not $cert) { throw "No Certum EV code-signing cert with a private key found in CurrentUser\My -- pass -Thumbprint explicitly." }
    $Thumbprint = $cert.Thumbprint
    Write-Host "Using EV cert: $($cert.Subject) ($Thumbprint, expires $($cert.NotAfter))"
}

# --- build the CAB ---------------------------------------------------------
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$cabName = "StreamToSpeaker-$version.cab"
$ddf = Join-Path $OutDir "StreamToSpeaker.ddf"

# Files must live in a subdirectory inside the CAB (one folder per driver
# package, nothing at the root) and the folder name must be <40 chars with
# no special characters -- Partner Center rejects the CAB otherwise.
$lines = @(
    '.OPTION EXPLICIT'
    '.Set CabinetFileCountThreshold=0'
    '.Set FolderFileCountThreshold=0'
    '.Set FolderSizeThreshold=0'
    '.Set MaxCabinetSize=0'
    '.Set MaxDiskFileCount=0'
    '.Set MaxDiskSize=0'
    '.Set CompressionType=MSZIP'
    '.Set Cabinet=on'
    '.Set Compress=on'
    ".Set CabinetNameTemplate=$cabName"
    ".Set DiskDirectoryTemplate=`"$OutDir`""
    '.Set DestinationDir=StreamToSpeaker'
    "`"$sys`""
    "`"$inf`""
)
if ($havePdb) { $lines += "`"$pdb`"" }
Set-Content -Path $ddf -Value $lines -Encoding ascii

& makecab /f $ddf | Write-Host
if ($LASTEXITCODE -ne 0) { throw "makecab failed with exit $LASTEXITCODE" }
$cab = Join-Path $OutDir $cabName
if (-not (Test-Path $cab)) { throw "Expected CAB not produced: $cab" }
# makecab drops setup.inf/setup.rpt working files next to the cab
Remove-Item (Join-Path $OutDir "setup.inf"), (Join-Path $OutDir "setup.rpt") -ErrorAction SilentlyContinue

# --- sign + verify ---------------------------------------------------------
# May pop the Certum card / SimplySign PIN prompt.
& $signtool sign /sha1 $Thumbprint /fd SHA256 /tr $TimestampUrl /td SHA256 /v $cab
if ($LASTEXITCODE -ne 0) { throw "signtool sign failed with exit $LASTEXITCODE" }

& $signtool verify /pa $cab
if ($LASTEXITCODE -ne 0) { throw "signature verification failed" }

Write-Host ""
Write-Host "Submission CAB ready: $cab"
Write-Host "Next: https://partner.microsoft.com/dashboard/hardware -> Submit new hardware"
Write-Host "  - leave both test-signing checkboxes UNCHECKED"
Write-Host "  - request signatures for Windows 10 1809+ and Windows 11 x64 (match the INF's NTamd64.10.0...17763 target)"
Write-Host "  - download the Microsoft-signed package when processing completes"
