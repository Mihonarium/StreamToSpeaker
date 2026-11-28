# Removes the test-signing certificates a test-signed Stream To Speaker
# installer made Windows trust (LocalMachine\Root and
# LocalMachine\TrustedPublisher). The uninstaller runs it elevated and reads
# the "left:" lines it prints.
#
# Removed: every certificate the installer recorded before trusting it,
# and the one in -CerPath. Kept: the one in -KeepCerPath. Only our own test
# certificates are touched (matched by thumbprint, and by subject), so a
# certificate trusted for other reasons is never removed. Microsoft-signed
# builds ship and record none, so for them this does nothing.
#
# Keep this file ASCII (see InstallerFunctions.ps1).

param(
    [string]$CerPath = '',
    [string]$KeepCerPath = ''
)

$ErrorActionPreference = 'Continue'
# Modules load only from Windows PowerShell's own folder: the inherited
# module path starts with the profile of the user who approved the
# elevation, where that user's own programs could put a module.
$env:PSModulePath = Join-Path $PSHOME 'Modules'
$exitCode = 1
try {
    . ([ScriptBlock]::Create([IO.File]::ReadAllText((Join-Path $PSScriptRoot 'InstallerFunctions.ps1'))))
    $StsLogPrefix = '[test-certificate] '
    $keep = Get-TestCertificateThumbprint $KeepCerPath
    $candidates = @(Get-RecordedTestCertificates) + @(Get-TestCertificateThumbprint $CerPath)
    $remove = @(Select-CertificatesToRemove -Candidates $candidates -Keep $keep)
    if (-not $remove.Count) {
        Write-Step 'No Stream To Speaker test certificate to remove.'
    }
    $left = @()
    foreach ($t in $remove) {
        $null = Remove-TestCertificateFromStores $t
        foreach ($store in 'Root', 'TrustedPublisher') {
            $still = @(Get-ChildItem -LiteralPath "Cert:\LocalMachine\$store" -ErrorAction SilentlyContinue |
                Where-Object { $_.Thumbprint -eq $t -and $_.Subject -eq $StsTestCertificateSubject })
            if ($still.Count) { $left += $t }
        }
    }
    $left = @($left | Select-Object -Unique)
    foreach ($t in $left) { Write-Host "left: the test certificate $t, which Windows still trusts" }
    Set-RecordedTestCertificates -Thumbprints @(@($keep) + $left | Where-Object { $_ })
    if (-not $left.Count) { $exitCode = 0 }
} catch {
    Write-Host "left: a test certificate (removing it failed: $($_.Exception.Message))"
}
exit $exitCode
