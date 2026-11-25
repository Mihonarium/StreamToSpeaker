#requires -Version 5.1
<#
.SYNOPSIS
    Stop unless a file carries a valid Authenticode signature by Microsoft.

.DESCRIPTION
    For a tool the installer bundles and runs elevated that nothing in this
    repository builds: devcon.exe, taken from the WDK (directly or through
    the driver cache). build.yml runs this before every installer is built.
    Prints the file's SHA-256 and signer, and adds them to the step summary.
#>
[CmdletBinding()]
param([Parameter(Mandatory)][string]$Path)
$ErrorActionPreference = "Stop"

$name = Split-Path $Path -Leaf
$sig = Get-AuthenticodeSignature -LiteralPath $Path
$subject = if ($sig.SignerCertificate) { $sig.SignerCertificate.Subject } else { "(none)" }
$sha = (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLower()
Write-Host "${name}: sha256 $sha, signature $($sig.Status), signer $subject"
if ($env:GITHUB_STEP_SUMMARY) {
    "| ``$name`` | ``$sha`` | $($sig.Status), $subject |" | Add-Content -Path $env:GITHUB_STEP_SUMMARY -Encoding utf8
}
if ($sig.Status -ne "Valid") {
    throw "$name carries no valid Authenticode signature (status $($sig.Status)); the installer would run it elevated"
}
if ($subject -notmatch '(^|,\s*)O=Microsoft Corporation(,|$)') {
    throw "$name is signed by $subject, not by Microsoft"
}
