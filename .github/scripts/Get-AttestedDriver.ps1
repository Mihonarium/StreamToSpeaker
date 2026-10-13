#requires -Version 5.1
<#
.SYNOPSIS
    Find and download the newest Microsoft-signed driver package that
    matches a driver-source hash.

.DESCRIPTION
    Scans this repo's driver-v* releases (created by driver-submission.yml
    + driver-attested.yml for attestation, or driver-certified.yml for
    WHQL). A release qualifies when its manifest.json says attested=true
    or certified=true AND its source_hash equals -SourceHash (the
    hashFiles('driver/**','include/**') of the checkout being built) —
    i.e. the Microsoft-signed binaries were built from exactly the driver
    source that's being packaged now. Service-only commits keep the same
    driver source hash, so they keep matching the last signed driver.

    Preference among matches: a WHQL-certified package always beats an
    attestation-signed one, even with a lower driver_build (certification
    is per binary, and the certified build is the one the lab tested);
    within the same kind the highest driver_build wins.

    The chosen build's StreamToSpeaker-Driver-<ver>-Signed.zip is
    downloaded, verified against the manifest's recorded hash, and the
    driver files (inf/sys/cat) are extracted flat into -OutDir, picked by
    file name (exactly one of each; nothing is written to a path taken
    from the zip).

    Emits step outputs (GITHUB_OUTPUT): found, tag, version, build, zip,
    kind (certified|attested). found=false means exactly one thing: no
    certified or attested driver-v* release records this source hash.
    Anything that keeps the script from knowing that — the release list or
    a manifest that cannot be fetched or read, a certified/attested
    manifest with a missing or malformed field — stops it with an error,
    so a transient failure can never pass for "no Microsoft-signed driver"
    (which would send a release to the test-signed fallback).
    -WarnOnError turns such an error into a warning plus found=false; it is
    for builds where the package is only an extra (dev and PR builds).
    It does not cover a driver release that contradicts its own manifest:
    a signed_zip asset the manifest names but the release lacks, a zip that
    does not hash to signed_zip_sha256, or one without exactly one
    inf/sys/cat. Those always fail, since they mean the release was altered
    or broken.

.NOTES
    Requires the gh CLI with GH_TOKEN set (standard on Actions runners).
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$SourceHash,
    [Parameter(Mandatory)][string]$OutDir,
    [string]$Repo = $env:GITHUB_REPOSITORY,
    [switch]$WarnOnError
)
$ErrorActionPreference = "Stop"

$PackageFiles = "StreamToSpeaker.inf", "StreamToSpeaker.sys", "StreamToSpeaker.cat"

# A driver release that contradicts its own manifest. Never downgraded
# by -WarnOnError.
function New-IntegrityError([string]$message) {
    [System.IO.InvalidDataException]::new($message)
}

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
        if ($LASTEXITCODE -ne 0) { throw "downloading $name from $tag failed" }
        try {
            Get-Content -LiteralPath $tmp -Raw | ConvertFrom-Json
        } catch {
            throw "$name of $tag is not valid JSON"
        }
    } finally {
        Remove-Item -LiteralPath $tmp -ErrorAction SilentlyContinue
    }
}

# Exactly one entry per wanted file name, written flat under its canonical
# name.
function Expand-PackageFlat([string]$zipPath, [string]$dest) {
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [System.IO.Compression.ZipFile]::OpenRead($zipPath)
    try {
        foreach ($name in $PackageFiles) {
            $entries = @($zip.Entries | Where-Object { $_.Name -ieq $name })
            if ($entries.Count -ne 1) { throw (New-IntegrityError "$(Split-Path $zipPath -Leaf) holds $($entries.Count) entries named $name, expected exactly one") }
            [System.IO.Compression.ZipFileExtensions]::ExtractToFile($entries[0], (Join-Path $dest $name), $true)
        }
    } finally {
        $zip.Dispose()
    }
}

function Find-SignedDriver {
    if ($SourceHash -notmatch '^[0-9a-f]{64}$') { throw "-SourceHash must be 64 lowercase hex digits (hashFiles output)" }

    $lines = gh api --paginate "repos/$Repo/releases?per_page=100" --jq '.[] | {tag_name, draft, assets: [.assets[].name]} | @json'
    if ($LASTEXITCODE -ne 0) { throw "listing the releases of $Repo failed" }
    $rels = @($lines | Where-Object { $_ } | ForEach-Object { $_ | ConvertFrom-Json })

    $best = $null
    foreach ($r in @($rels | Where-Object { $_.tag_name -like "driver-v*" -and -not $_.draft })) {
        # Only a manifest can mark a release certified or attested: one
        # without it is not a candidate.
        if (@($r.assets) -notcontains "manifest.json") {
            Write-Host "  ($($r.tag_name): no manifest.json, not a Microsoft-signed driver release)"
            continue
        }
        $m = Get-JsonAsset $r.tag_name "manifest.json"
        $certified = $m.certified -eq $true
        if (-not ($certified -or $m.attested -eq $true)) { continue }
        $kind = if ($certified) { "certified" } else { "attested" }
        if ("$($m.source_hash)" -notmatch '^[0-9a-f]{64}$') {
            throw "$($r.tag_name): the manifest says $kind but records no valid source_hash, so it cannot be ruled out"
        }
        if ($m.source_hash -ne $SourceHash) { continue }
        # A match: everything that goes into step outputs and file names
        # must be well-formed.
        if ($r.tag_name -notmatch '^driver-v\d+\.\d+\.\d+\.\d+$') { throw "$($r.tag_name): unexpected tag shape for a $kind driver release" }
        if ("$($m.driver_version)" -notmatch '^\d+\.\d+\.\d+\.\d+$') { throw "$($r.tag_name): malformed driver_version in the manifest" }
        if ("$($m.driver_build)" -notmatch '^\d+$') { throw "$($r.tag_name): malformed driver_build in the manifest" }
        if ("$($m.signed_zip)" -notmatch '^[A-Za-z0-9][A-Za-z0-9._+-]*$') { throw "$($r.tag_name): malformed signed_zip in the manifest" }
        if ("$($m.signed_zip_sha256)" -notmatch '^[0-9a-fA-F]{64}$') { throw "$($r.tag_name): malformed signed_zip_sha256 in the manifest" }
        # Rank: certified first, then the highest driver_build.
        $rank = @([int]$certified, [int]$m.driver_build)
        if (-not $best -or $rank[0] -gt $best.Rank[0] -or
            ($rank[0] -eq $best.Rank[0] -and $rank[1] -gt $best.Rank[1])) {
            $best = @{ Release = $r; Manifest = $m; Rank = $rank; Kind = $kind }
        }
    }

    if (-not $best) {
        Write-Host "No Microsoft-signed (certified or attested) driver release matches driver source hash $SourceHash."
        Write-Host "(Driver source changed since the last signing round? A new HLK run + 'Driver certified', or the 'Driver submission' attestation flow - see docs/driver-signing.md.)"
        Out-StepOutput "found=false"
        return
    }

    $m = $best.Manifest
    $r = $best.Release
    $kind = $best.Kind
    if (@($r.assets) -notcontains $m.signed_zip) { throw (New-IntegrityError "$($r.tag_name): manifest says $kind but asset $($m.signed_zip) is missing") }

    New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
    $zipPath = Join-Path (Resolve-Path $OutDir).Path $m.signed_zip
    gh release download $r.tag_name --repo $Repo --pattern $m.signed_zip --output $zipPath --clobber
    if ($LASTEXITCODE -ne 0) { throw "gh release download of $($m.signed_zip) failed" }
    $zipSha = (Get-FileHash -LiteralPath $zipPath -Algorithm SHA256).Hash.ToLower()
    if ($zipSha -ne $m.signed_zip_sha256.ToLower()) {
        throw (New-IntegrityError "$($m.signed_zip) hashes to $zipSha but the manifest records $($m.signed_zip_sha256) - refusing to use it")
    }
    Expand-PackageFlat $zipPath $OutDir

    Write-Host "Microsoft-signed driver ($kind): $($r.tag_name) (DriverVer $($m.driver_version), driver source hash matches)"
    Out-StepOutput "found=true"
    Out-StepOutput "kind=$kind"
    Out-StepOutput "tag=$($r.tag_name)"
    Out-StepOutput "version=$($m.driver_version)"
    Out-StepOutput "build=$($m.driver_build)"
    Out-StepOutput "zip=$zipPath"
}

try {
    Find-SignedDriver
} catch {
    # ZipFile.OpenRead throws InvalidDataException (wrapped by PowerShell)
    # for a zip that is not one: after the hash matched, that too is the
    # release's content.
    $integrity = $false
    for ($e = $_.Exception; $e; $e = $e.InnerException) {
        if ($e -is [System.IO.InvalidDataException]) { $integrity = $true }
    }
    if (-not $WarnOnError -or $integrity) { throw }
    Write-Host "::warning::Could not determine the Microsoft-signed driver for driver source hash $SourceHash ($($_.Exception.Message)); no production-driver package for this build."
    Out-StepOutput "found=false"
}
# Handled: a gh failure above must not become the step's exit code.
exit 0
