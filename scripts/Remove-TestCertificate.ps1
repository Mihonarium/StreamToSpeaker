# Removes the test-signing certificates a test-signed Stream To Speaker
# installer made Windows trust (LocalMachine\Root and
# LocalMachine\TrustedPublisher). The uninstaller runs it elevated.
#
# Removed: every certificate the installer recorded when it trusted it,
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
    foreach ($t in $remove) {
        if (-not (Remove-TestCertificateFromStores $t)) {
            Write-Step "The test certificate $t is not in the machine's stores."
        }
    }
    Set-RecordedTestCertificates -Thumbprints @($keep | Where-Object { $_ })
    $exitCode = 0
} catch {
    Write-Host "Removing the test certificate failed: $($_.Exception.Message)"
}
exit $exitCode
