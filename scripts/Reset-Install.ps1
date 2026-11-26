# Clears Stream To Speaker's driver, so that the next run of Setup starts
# from nothing: a new device, which gets a new sound output. For a PC where
# running Setup again didn't make the sound output work.
#
# It removes the Stream To Speaker device and every Stream To Speaker
# package in the driver store, and deletes Windows' cached entries of the
# removed sound output where Windows allows it (one that stays does
# nothing). The app, its settings and its speakers stay. Afterwards: restart
# the PC, then run StreamToSpeakerSetup.exe again.
#
# Run it from an elevated PowerShell (right-click Windows PowerShell, Run as
# administrator):
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File "C:\Program Files\Stream To Speaker\scripts\Reset-Install.ps1"
#
# Keep this file ASCII (see InstallerFunctions.ps1).

$ErrorActionPreference = 'Continue'
# Modules load only from Windows PowerShell's own folder: the inherited
# module path starts with the profile of the user who approved the
# elevation, where that user's own programs could put a module.
$env:PSModulePath = Join-Path $PSHOME 'Modules'

$principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Write-Host 'This needs an elevated PowerShell: nothing was changed.' -ForegroundColor Yellow
    Write-Host 'Right-click Windows PowerShell, choose Run as administrator, and run:'
    Write-Host "  powershell -NoProfile -ExecutionPolicy Bypass -File `"$PSCommandPath`""
    exit 1
}

. ([ScriptBlock]::Create([IO.File]::ReadAllText((Join-Path $PSScriptRoot 'InstallerFunctions.ps1'))))
$StsLogPrefix = '  '
$StsDevcon = Join-Path (Split-Path -Parent $PSScriptRoot) 'driver\devcon.exe'

if (Get-Process -Name 'stream-to-speaker' -ErrorAction SilentlyContinue) {
    Write-Host 'Stream To Speaker is running: nothing was changed.' -ForegroundColor Yellow
    Write-Host 'Quit it (right-click its icon in the notification area, then Quit), then run this again.'
    exit 1
}

$problems = @()

Write-Host '1/3  Removing the Stream To Speaker device...' -ForegroundColor Cyan
$devices = @(Get-OurDevices)
if (-not $devices.Count) { Write-Step 'There is none.' }
$removedNow = 0
$staying = 0
foreach ($d in $devices) {
    $withDevcon = [IO.File]::Exists($StsDevcon)
    $code = if ($withDevcon) {
        Remove-Device $d.InstanceId
    } else {
        # devcon ships with the app; without it, pnputil can remove devices
        # on Windows 10 version 2004 and later.
        Invoke-Tool -FilePath $StsPnputil -Arguments @('/remove-device', $d.InstanceId)
    }
    # 0: the device is removed. Windows removes it when it restarts: 1 from
    # devcon, 3010 from pnputil. Anything else: it could not be removed.
    $atRestartCode = if ($withDevcon) { 1 } else { 3010 }
    if ($code -eq 0) { $removedNow++ } else { $staying++ }
    $why = if ($withDevcon -or [IO.File]::Exists($StsPnputil)) { "exit code $code" } else { 'pnputil.exe is missing' }
    if ($code -ne 0 -and $code -ne $atRestartCode) { $problems += "the device $($d.InstanceId) could not be removed ($why)" }
}

Write-Host '2/3  Deleting the driver from the driver store...' -ForegroundColor Cyan
$packages = @(Get-OurDriverPackageNames)
if (-not $packages.Count) { Write-Step 'There is none.' }
foreach ($p in $packages) {
    $code = Invoke-Tool -FilePath $StsPnputil -Arguments @('/delete-driver', $p, '/uninstall', '/force')
    $why = if ([IO.File]::Exists($StsPnputil)) { "exit code $code" } else { 'pnputil.exe is missing' }
    if ($code -ne 0 -and $code -ne 3010) { $problems += "the driver package $p could not be deleted ($why)" }
}

Write-Host "3/3  Clearing Windows' cached entries for the sound output..." -ForegroundColor Cyan
# Only of outputs whose device is gone: the output of a device Windows
# removes when it restarts is still there. Windows may keep an entry; one
# that stays does nothing, because Setup's new device gets a new sound
# output.
# The output of a device that just went can take a moment to show as gone.
# While a device stays, so does its output: nothing to wait for.
$all = @(Get-OurEndpoints)
for ($i = 0; $i -lt 5 -and $removedNow -and -not $staying -and @(Select-PresentEndpoints $all).Count; $i++) {
    Start-Sleep -Seconds 1
    $all = @(Get-OurEndpoints)
}
$entries = @(Select-OrphanedEndpoints $all)
if (-not $entries.Count) { Write-Step 'There are none to clear.' }
if ($entries.Count) {
    $gone = Remove-EndpointEntries $entries
    if (@($gone.Failed).Count) { Write-Step "Windows keeps $(@($gone.Failed).Count) of them. They do nothing without their device." }
}

Write-Host ''
if ($problems.Count) {
    Write-Host 'Not everything could be cleared:' -ForegroundColor Yellow
    foreach ($p in $problems) { Write-Host "  - $p" }
    Write-Host 'Restart the PC and run this again. If the same items stay, run Diagnose.ps1 (in this folder) and include its output when you report the problem.'
    exit 1
}
Write-Host 'Cleared.' -ForegroundColor Green
Write-Host 'Next: restart the PC, then run StreamToSpeakerSetup.exe again (the latest is at'
Write-Host 'https://github.com/Mihonarium/StreamToSpeaker/releases/latest). Your settings and speakers are kept.'
exit 0
