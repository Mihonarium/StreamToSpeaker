# Removes Stream To Speaker's sound driver from this PC. The uninstaller
# runs it elevated, after closing the app, and reads the "left:" and
# "stays:" lines it prints: it names what is still there afterwards in its
# last message.
#
# Removes every Stream To Speaker device, every Stream To Speaker package in
# the driver store, and Windows' cached entries for the sound output (which
# would otherwise keep the last speaker's name in Sound settings). The
# loaded driver stays in memory until Windows restarts; the uninstaller
# asks for that restart when the driver was running.
#
# Exit code 0 when everything is gone, 1 when something is left.
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
    $StsLogPrefix = '[uninstall-driver] '
    $StsDevcon = Join-Path (Split-Path -Parent $PSScriptRoot) 'driver\devcon.exe'
    $left = @(Invoke-DriverRemoval)
    foreach ($item in $left) { Write-Host $item }
    if (-not $left.Count) {
        Write-Step 'The driver, its device and its cached sound output entries are removed.'
        $exitCode = 0
    }
} catch {
    Write-Host "left: the sound driver (removing it failed: $($_.Exception.Message))"
    Write-Host $_.ScriptStackTrace
}
exit $exitCode
