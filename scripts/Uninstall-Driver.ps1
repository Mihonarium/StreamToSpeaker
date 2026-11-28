# Removes Stream To Speaker's sound driver from this PC. The uninstaller
# runs it elevated, after closing the app, and reads two kinds of lines it
# prints: "left: ..." names what is still there afterwards (the uninstaller
# says so in its last message), and "removed-at-restart: ..." a device
# Windows removes only when it restarts (the uninstaller then asks for the
# restart).
#
# Moves Windows off the app's sound output (the app no longer runs, so the
# output would play nowhere), then removes every Stream To Speaker device
# and every Stream To Speaker package in the driver store. Windows' cached
# entries for the removed sound output are deleted where Windows allows it;
# one that stays does nothing. The loaded driver stays in memory until
# Windows restarts; the uninstaller asks for that restart when the driver
# was running.
#
# Exit code 0 when nothing is left that uninstalling again may remove, 1
# otherwise.
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
    try {
        # They read and change which output Windows plays to.
        Initialize-NativeHelpers
    } catch {
        Write-Step "Could not load the Windows helpers, so the output Windows plays to is left as it is: $($_.Exception.Message)"
    }
    $lines = @(Invoke-DriverRemoval)
    foreach ($line in $lines) { Write-Host $line }
    if (-not @($lines | Where-Object { $_.StartsWith('left: ') }).Count) { $exitCode = 0 }
} catch {
    Write-Host "left: the sound driver (removing it failed: $($_.Exception.Message))"
    Write-Host $_.ScriptStackTrace
}
exit $exitCode
