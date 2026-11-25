# Removes Stream To Speaker's sound driver from this PC. The uninstaller
# runs it elevated, after devcon has removed the device; its output goes to
# the uninstall log.
#
# Removes any Stream To Speaker device still there, every Stream To Speaker
# package in the driver store, and Windows' cached entries for the sound
# output (which would otherwise keep the last speaker's name in Sound
# settings). The loaded driver stays in memory until Windows restarts; the
# uninstaller asks for that restart when the driver was running.
#
# Exit code 0 when everything is gone, 1 when something is left (listed in
# the output).
#
# Keep this file ASCII (see InstallerFunctions.ps1).

$ErrorActionPreference = 'Continue'
$exitCode = 1
try {
    . ([ScriptBlock]::Create([IO.File]::ReadAllText((Join-Path $PSScriptRoot 'InstallerFunctions.ps1'))))
    $StsLogPrefix = '[uninstall-driver] '
    $StsDevcon = Join-Path (Split-Path -Parent $PSScriptRoot) 'driver\devcon.exe'
    $left = @()

    foreach ($d in @(Get-OurDevices)) {
        Write-Step "Removing the device $($d.InstanceId)..."
        $code = Remove-Device $d.InstanceId
        if ($code -ne 0 -and $code -ne 1) { $left += "device $($d.InstanceId)" }
    }

    $packages = @(Get-OurDriverPackageNames)
    if (-not $packages.Count) { Write-Step 'No Stream To Speaker driver package in the driver store.' }
    foreach ($p in $packages) {
        Write-Step "Deleting the driver package $p..."
        $code = Invoke-Tool -FilePath $StsPnputil -Arguments @('/delete-driver', $p, '/uninstall', '/force')
        if ($code -ne 0 -and $code -ne 3010) { $left += "driver package $p" }
    }

    if (-not @(Get-OurDevices).Count) {
        $entries = @(Get-OurEndpoints)
        if ($entries.Count) {
            $gone = Remove-EndpointEntries $entries
            foreach ($ep in $gone.Failed) { $left += "cached sound output entry $($ep.Guid)" }
        }
    }

    if ($left.Count) {
        Write-Step "Left on this PC: $($left -join '; ')."
    } else {
        Write-Step 'The driver, its device and its cached sound output entries are removed.'
        $exitCode = 0
    }
} catch {
    Write-Host "Uninstall-Driver failed: $($_.Exception.Message)"
    Write-Host $_.ScriptStackTrace
}
exit $exitCode
