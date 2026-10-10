# Removes the Stream To Speaker test-signing certificate that a
# test-signed installer imported into LocalMachine\Root and
# LocalMachine\TrustedPublisher. Called on uninstall, and on upgrade
# for the certificate of the build being replaced.
#
# Only the exact certificate in -CerPath is removed (matched by
# thumbprint), and only if it is one of our CI test-signing
# certificates (subject below), so a certificate the user trusts for
# other reasons is never touched. -KeepCerPath names a certificate that
# must stay (the one the new build has just imported, or is about to);
# when both files hold the same certificate nothing is removed.
# Microsoft-signed builds ship no .cer, so for them this is a no-op.

param(
    [Parameter(Mandatory = $true)][string]$CerPath,
    [string]$KeepCerPath = ""
)

$ErrorActionPreference = "Stop"
$OurSubject = "CN=Stream To Speaker (test sign)"

function Get-Thumbprint([string]$Path) {
    if (-not $Path -or -not (Test-Path -LiteralPath $Path)) { return $null }
    $c = New-Object System.Security.Cryptography.X509Certificates.X509Certificate2 $Path
    if ($c.Subject -ne $OurSubject) {
        Write-Output "Leaving certificate alone: unexpected subject '$($c.Subject)'."
        return $null
    }
    return $c.Thumbprint
}

try {
    $thumb = Get-Thumbprint $CerPath
    if (-not $thumb) {
        Write-Output "No Stream To Speaker test certificate to remove."
        exit 0
    }
    $keep = $null
    if ($KeepCerPath -and (Test-Path -LiteralPath $KeepCerPath)) {
        $keep = (New-Object System.Security.Cryptography.X509Certificates.X509Certificate2 $KeepCerPath).Thumbprint
    }
    if ($keep -and $keep -eq $thumb) {
        Write-Output "Test certificate $thumb is still in use; keeping it."
        exit 0
    }
    foreach ($storeName in @("Root", "TrustedPublisher")) {
        $store = New-Object System.Security.Cryptography.X509Certificates.X509Store($storeName, "LocalMachine")
        $store.Open("ReadWrite")
        try {
            $found = @($store.Certificates.Find("FindByThumbprint", $thumb, $false))
            foreach ($cert in $found) {
                $store.Remove($cert)
                Write-Output "Removed test certificate $thumb from LocalMachine\$storeName."
            }
        } finally {
            $store.Close()
        }
    }
} catch {
    Write-Output "Removing the test certificate failed: $_"
}
exit 0
