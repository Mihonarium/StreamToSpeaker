# Removes the "start at sign-in" entry of every signed-in account that
# starts Stream To Speaker from this install folder. The uninstaller runs it
# elevated, which reaches the registry of every account that is signed in
# (an elevated process's own HKCU may belong to another account, the
# administrator who approved the uninstall). Prints a "left:" line for each
# entry that is still there afterwards, which the uninstaller names in its
# last message.
#
# Keep this file ASCII (see InstallerFunctions.ps1).

$ErrorActionPreference = 'Continue'
# Modules load only from Windows PowerShell's own folder: the inherited
# module path starts with the profile of the user who approved the
# elevation, where that user's own programs could put a module.
$env:PSModulePath = Join-Path $PSHOME 'Modules'
$exitCode = 1
try {
    . ([ScriptBlock]::Create([IO.File]::ReadAllText((Join-Path $PSScriptRoot 'InstallerFunctions.ps1'))))
    $StsLogPrefix = '[sign-in] '
    $dir = Split-Path -Parent $PSScriptRoot
    $n = Remove-SignInEntries -InstallDir $dir
    $left = @(Get-SignInAccounts -InstallDir $dir)
    if (-not $n -and -not $left.Count) { Write-Step 'No signed-in account starts Stream To Speaker at sign-in.' }
    foreach ($account in $left) { Write-Host "left: the start-at-sign-in entry of the Windows account $account" }
    if (-not $left.Count) { $exitCode = 0 }
} catch {
    Write-Host "left: the start-at-sign-in entries of signed-in accounts (removing them failed: $($_.Exception.Message))"
}
exit $exitCode
