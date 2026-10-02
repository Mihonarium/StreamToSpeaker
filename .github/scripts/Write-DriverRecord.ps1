#requires -Version 5.1
<#
.SYNOPSIS
    Record which driver package an installer bundles.

.DESCRIPTION
    Writes <OutDir>\<Installer without .exe>.driver.json and appends a table
    to the step summary. The record names the driver-v* release and its
    signed_zip (with the SHA-256 the release manifest records) when the
    installer bundles a Microsoft-signed package, and always lists the
    SHA-256 of the .sys/.inf/.cat actually staged, plus whether a test
    certificate was staged. Anyone holding the installer can unpack it and
    compare against this record; driver-reproducibility.yml with
    `installer_tag` does exactly that against the release's signed_zip.

    -Kind is certified | attested | test-signed. For test-signed builds
    -Tag/-Zip are empty: the driver was built (or cache-restored) in CI.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$StagingDir,
    [Parameter(Mandatory)][string]$Installer,
    [Parameter(Mandatory)][ValidateSet("certified", "attested", "test-signed")][string]$Kind,
    [Parameter(Mandatory)][string]$OutDir,
    [string]$SourceHash = "",
    [string]$Tag = "",
    [string]$Version = "",
    [string]$Build = "",
    [string]$Zip = ""
)
$ErrorActionPreference = "Stop"

$files = [ordered]@{}
foreach ($name in "StreamToSpeaker.sys", "StreamToSpeaker.inf", "StreamToSpeaker.cat") {
    $p = Join-Path $StagingDir $name
    if (-not (Test-Path $p)) { throw "$name not staged in $StagingDir" }
    $files[$name] = (Get-FileHash $p -Algorithm SHA256).Hash.ToLower()
}
$cert = Test-Path (Join-Path $StagingDir "StreamToSpeaker.cer")
if ($Kind -ne "test-signed" -and $cert) { throw "a $Kind driver is staged together with a test certificate" }
if ($Kind -ne "test-signed" -and -not ($Tag -and $Zip)) { throw "-Tag and -Zip are required for a $Kind driver" }

$package = $null
if ($Zip) {
    $package = [ordered]@{
        name   = Split-Path $Zip -Leaf
        sha256 = (Get-FileHash $Zip -Algorithm SHA256).Hash.ToLower()
    }
}
$record = [ordered]@{
    schema         = 1
    installer      = $Installer
    commit         = $env:GITHUB_SHA
    run            = if ($env:GITHUB_RUN_ID) { "$env:GITHUB_SERVER_URL/$env:GITHUB_REPOSITORY/actions/runs/$env:GITHUB_RUN_ID" } else { $null }
    source_hash    = $SourceHash
    driver         = [ordered]@{
        kind    = $Kind
        release = if ($Tag) { $Tag } else { $null }
        version = if ($Version) { $Version } else { $null }
        build   = if ($Build) { [int]$Build } else { $null }
        package = $package
    }
    files          = $files
    test_cert      = $cert
}
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$out = Join-Path $OutDir (([IO.Path]::GetFileNameWithoutExtension($Installer)) + ".driver.json")
$record | ConvertTo-Json -Depth 5 | Set-Content -Path $out -Encoding utf8
Write-Host "Driver record: $out"
Get-Content $out | ForEach-Object { Write-Host "  $_" }

$s = @()
$s += "## Bundled driver: ``$Installer``"
$s += ""
$s += "| | |"
$s += "| --- | --- |"
if ($package) {
    $s += "| Driver | $Kind ``$Version`` from ``$Tag`` |"
    $s += "| Package | ``$($package.name)`` sha256 ``$($package.sha256)`` |"
} else {
    $s += "| Driver | **test-signed** (built by CI; needs testsigning mode) |"
}
foreach ($k in $files.Keys) { $s += "| $k | ``$($files[$k])`` |" }
$s += "| Test certificate | $(if ($cert) { 'shipped' } else { 'none' }) |"
$s += ""
if ($env:GITHUB_STEP_SUMMARY) { $s -join "`n" | Add-Content -Path $env:GITHUB_STEP_SUMMARY -Encoding utf8 }
if ($env:GITHUB_OUTPUT) { "record=$out" >> $env:GITHUB_OUTPUT }
