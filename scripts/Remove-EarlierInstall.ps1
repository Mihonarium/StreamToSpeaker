# Removes what an earlier install put in a folder outside Program Files,
# after Setup has installed Stream To Speaker into Program Files. Setup runs
# it elevated, from Program Files; nothing in the earlier folder is run.
#
# Every signed-in account that starts the app at sign-in from the earlier
# folder starts the new one instead; a desktop shortcut that still starts
# the earlier copy goes (Setup has made a new one if it was asked to);
# firewall rules for the earlier copy go; then only the files releases put
# in that folder are deleted, by name, and the folder when it is empty.
# When the folder, or a folder above it, is a link or junction, no file in
# it is deleted; and a file is deleted only where Windows says the opened
# file really is at its path, so that a link put in the path's place while
# this runs leads no delete to another file (this install's, for one).
#
# Prints, last:
#   earlier-install-result: removed|not-empty|left-alone|gone
#
# Keep this file ASCII (see InstallerFunctions.ps1).

param(
    [Parameter(Mandatory = $true)][string]$Folder
)

$ErrorActionPreference = 'Continue'
# Modules load only from Windows PowerShell's own folder: the inherited
# module path starts with the profile of the user who approved the
# elevation, where that user's own programs could put a module.
$env:PSModulePath = Join-Path $PSHOME 'Modules'
$line = 'earlier-install-result: left-alone'
try {
    . ([ScriptBlock]::Create([IO.File]::ReadAllText((Join-Path $PSScriptRoot 'InstallerFunctions.ps1'))))
    $StsLogPrefix = '[earlier-install] '
    $newExe = Join-Path (Split-Path -Parent $PSScriptRoot) $StsExeName
    $old = $Folder.TrimEnd('\')
    if (Test-InFolder $newExe $old) {
        Write-Step "$old holds this install; nothing to remove."
        $line = 'earlier-install-result: gone'
    } else {
        $null = Move-SignInEntries -FromFolder $old -ToExe $newExe
        $null = Remove-ShortcutsInto -Links @(Get-EarlierShortcuts) -Folder $old
        $null = Remove-AppFirewallRules (Join-Path $old $StsExeName) -OnlyForProgram
        $r = Remove-EarlierInstall -Folder $old -Keep (Split-Path -Parent $PSScriptRoot)
        $line = "earlier-install-result: $($r.Result)"
    }
} catch {
    Write-Host "Remove-EarlierInstall failed: $($_.Exception.Message)"
}
Write-Host $line
exit 0
