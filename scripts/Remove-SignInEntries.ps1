# Removes the "start at sign-in" entry of every signed-in account that
# starts Stream To Speaker from this install folder. The uninstaller runs it
# elevated, which reaches the registry of every account that is signed in
# (an elevated process's own HKCU may belong to another account, the
# administrator who approved the uninstall).
#
# Keep this file ASCII (see InstallerFunctions.ps1).

$ErrorActionPreference = 'Continue'
$exitCode = 1
try {
    . ([ScriptBlock]::Create([IO.File]::ReadAllText((Join-Path $PSScriptRoot 'InstallerFunctions.ps1'))))
    $StsLogPrefix = '[sign-in] '
    $n = Remove-SignInEntries -InstallDir (Split-Path -Parent $PSScriptRoot)
    if (-not $n) { Write-Step 'No signed-in account starts Stream To Speaker at sign-in.' }
    $exitCode = 0
} catch {
    Write-Host "Removing the start-at-sign-in entries failed: $($_.Exception.Message)"
}
exit $exitCode
