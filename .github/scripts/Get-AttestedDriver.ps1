#requires -Version 5.1
<#
.SYNOPSIS
    Find and download the newest Microsoft-attested driver package that
    matches a driver-source hash.

.DESCRIPTION
    Scans this repo's driver-v* releases (created by driver-submission.yml,
    finalized by driver-attested.yml). A release qualifies when its
    manifest.json says attested=true AND its source_hash equals -SourceHash
    (the hashFiles('driver/**','include/**') of the checkout being built) —
    i.e. the attested binaries were built from exactly the driver source
    that's being packaged now. Service-only commits keep the same driver
    source hash, so they keep matching the last attested driver.

    The newest qualifying build's StreamToSpeaker-Driver-<ver>-Signed.zip is
    downloaded, verified against the manifest's recorded hash, and the
    driver files (inf/sys/cat) are extracted flat into -OutDir.

    Emits step outputs (GITHUB_OUTPUT): found, tag, version, build, zip.
    Exits 0 with found=false when nothing matches — callers decide whether
    that is fatal.

.NOTES
    Requires the gh CLI with GH_TOKEN set (standard on Actions runners).
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$SourceHash,
    [Parameter(Mandatory)][string]$OutDir,
    [string]$Repo = $env:GITHUB_REPOSITORY
)
$ErrorActionPreference = "Stop"

function Out-StepOutput([string]$line) {
    if ($env:GITHUB_OUTPUT) { $line >> $env:GITHUB_OUTPUT }
}

# Authenticated fetch via gh — unauthenticated release-asset URLs 404 on a
# private repo (and assets can be served as octet-stream anyway, so we
# download then parse rather than Invoke-RestMethod).
function Get-JsonAsset([string]$tag, [string]$name) {
    $tmp = Join-Path ([IO.Path]::GetTempPath()) ([guid]::NewGuid().ToString("N") + ".json")
    try {
        gh release download $tag --repo $Repo --pattern $name --output $tmp --clobber
        if ($LASTEXITCODE -ne 0) { throw "gh release download of $name from $tag failed" }
        Get-Content $tmp -Raw | ConvertFrom-Json
    } finally {
        Remove-Item $tmp -ErrorAction SilentlyContinue
    }
}

$rels = gh api "repos/$Repo/releases?per_page=100" | ConvertFrom-Json
if ($LASTEXITCODE -ne 0) { throw "gh api releases failed" }

$best = $null
foreach ($r in @($rels | Where-Object { $_.tag_name -like "driver-v*" -and -not $_.draft })) {
    $manAsset = $r.assets | Where-Object { $_.name -eq "manifest.json" } | Select-Object -First 1
    if (-not $manAsset) { continue }
    try {
        $m = Get-JsonAsset $r.tag_name "manifest.json"
    } catch {
        Write-Host "  ($($r.tag_name): manifest fetch failed, skipping)"
        continue
    }
    if (-not $m.attested) { continue }
    if ($m.source_hash -ne $SourceHash) { continue }
    if (-not $best -or [int]$m.driver_build -gt [int]$best.Manifest.driver_build) {
        $best = @{ Release = $r; Manifest = $m }
    }
}

if (-not $best) {
    Write-Host "No Microsoft-attested driver release matches driver source hash $SourceHash."
    Write-Host "(Driver source changed since the last attestation? Run the 'Driver submission' workflow and the Partner Center flow - see docs/driver-signing.md.)"
    Out-StepOutput "found=false"
    exit 0
}

$m = $best.Manifest
$r = $best.Release
$zipAsset = $r.assets | Where-Object { $_.name -eq $m.signed_zip } | Select-Object -First 1
if (-not $zipAsset) { throw "$($r.tag_name): manifest says attested=true but asset $($m.signed_zip) is missing" }

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$zipPath = Join-Path (Resolve-Path $OutDir).Path $m.signed_zip
gh release download $r.tag_name --repo $Repo --pattern $m.signed_zip --output $zipPath --clobber
if ($LASTEXITCODE -ne 0) { throw "gh release download of $($m.signed_zip) failed" }
$zipSha = (Get-FileHash $zipPath -Algorithm SHA256).Hash.ToLower()
if ($zipSha -ne $m.signed_zip_sha256.ToLower()) {
    throw "$($m.signed_zip) hashes to $zipSha but the manifest records $($m.signed_zip_sha256) - refusing to use it"
}

$extract = Join-Path $OutDir "extracted"
Expand-Archive $zipPath -DestinationPath $extract -Force
foreach ($name in "StreamToSpeaker.inf", "StreamToSpeaker.sys", "StreamToSpeaker.cat") {
    $f = Get-ChildItem -Path $extract -Recurse -Filter $name | Select-Object -First 1
    if (-not $f) { throw "$($m.signed_zip) does not contain $name" }
    Copy-Item $f.FullName (Join-Path $OutDir $name) -Force
}

Write-Host "Attested driver: $($r.tag_name) (DriverVer $($m.driver_version), driver source hash matches)"
Out-StepOutput "found=true"
Out-StepOutput "tag=$($r.tag_name)"
Out-StepOutput "version=$($m.driver_version)"
Out-StepOutput "build=$($m.driver_build)"
Out-StepOutput "zip=$zipPath"
