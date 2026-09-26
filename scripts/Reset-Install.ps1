# Nuke ALL cached Stream To Speaker state, so the next install picks
# up the current INF cleanly. Windows caches the endpoint name + jack
# association in HKLM\...\MMDevices\Audio\Render the first time a
# device is enrolled, and re-installing the SAME hardware reuses the
# cached entry instead of re-reading the new INF. That's why the
# Sound Settings name stays "Internal AUX Jack" even though our
# current INF uses KSNODETYPE_SPEAKER.
#
# What this does:
#   1. Removes the live device   (devcon remove Root\StreamToSpeaker)
#   2. Deletes the driver store entry (pnputil /delete-driver)
#   3. Wipes the cached MMDevices endpoint entries
#   4. Tells the user to reboot, then run the installer fresh
#
# Run elevated.

$ErrorActionPreference = "Continue"

# Published names (oemNN.inf) of every Stream To Speaker package in the
# driver store. pnputil /enum-drivers is not parsed: its labels
# ("Original Name:", "Published Name:") are translated on non-English
# Windows. Instead: every published package has its INF copied to
# %windir%\INF\oemNN.inf, and ours is the one declaring our hardware ID.
# The same function is in installer/Uninstall-Driver.ps1,
# scripts/Pre-Install.ps1 and scripts/Reset-Install.ps1; keep them equal.
function Get-StreamToSpeakerDriverPackages {
    $infDir = Join-Path $env:windir 'INF'
    @(Get-ChildItem -Path $infDir -Filter 'oem*.inf' -ErrorAction SilentlyContinue |
        Where-Object { Select-String -Path $_.FullName -Pattern 'Root\StreamToSpeaker' -SimpleMatch -Quiet -ErrorAction SilentlyContinue } |
        ForEach-Object { $_.Name } |
        Sort-Object)
}

Write-Host "1/3  Removing the live device node..." -ForegroundColor Cyan
$devcon = Join-Path $PSScriptRoot "..\driver\devcon.exe"
if (-not (Test-Path $devcon)) {
    # Try the typical install-time location
    $devcon = "C:\Program Files\Stream To Speaker\driver\devcon.exe"
}
if (Test-Path $devcon) {
    & $devcon remove "Root\StreamToSpeaker" | Out-Null
    Write-Host "  device removed."
} else {
    Write-Warning "  devcon.exe not found at $devcon - install may have already removed it."
}

Write-Host "2/3  Removing the driver-store entry..." -ForegroundColor Cyan
$matched = 0
foreach ($oem in (Get-StreamToSpeakerDriverPackages)) {
    Write-Host "  removing $oem ..."
    & pnputil /delete-driver $oem /uninstall /force | Out-Null
    $matched++
}
if ($matched -eq 0) {
    Write-Host "  (no StreamToSpeaker driver in the store)"
}

Write-Host "3/3  Wiping cached MMDevices endpoint entries..." -ForegroundColor Cyan
$base = "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render"
$pkeyDeviceDesc = "{a45c254e-df1c-4efd-8020-67d146a850e0},2"
# The endpoint's DeviceDesc is just "Speakers" until Rename-Endpoint.ps1 has run;
# the device-interface name carries "Stream To Speaker" from the INF from the start.
$pkeyInterfaceName = "{b3f8fa53-0004-438e-9003-51a46e139bfc},6"
$wiped = 0
Get-ChildItem $base | ForEach-Object {
    $propsPath = Join-Path $_.PSPath "Properties"
    if (-not (Test-Path $propsPath)) { return }
    $desc = (Get-ItemProperty -Path $propsPath -Name $pkeyDeviceDesc -ErrorAction SilentlyContinue).$pkeyDeviceDesc
    $iface = (Get-ItemProperty -Path $propsPath -Name $pkeyInterfaceName -ErrorAction SilentlyContinue).$pkeyInterfaceName
    if ($desc -like "*Stream To Speaker*" -or $iface -like "*Stream To Speaker*") {
        Write-Host "  wiping $($_.PSChildName) ($desc)"
        Remove-Item -Path $_.PSPath -Recurse -Force
        $wiped++
    }
}
if ($wiped -eq 0) {
    Write-Host "  (no cached Stream To Speaker endpoints)"
}

Write-Host ""
Write-Host "Clean done." -ForegroundColor Green
Write-Host ""
Write-Host "Next steps:"
Write-Host "  1. Reboot (required - MMDevAPI caches in-memory state that"
Write-Host "     won't fully release until next session)."
Write-Host "  2. Re-run StreamToSpeakerSetup-<version>.exe."
Write-Host "  3. After install, the device should appear as 'Stream To Speaker'"
Write-Host "     and be enabled by default. If it shows up disabled and Sound"
Write-Host "     Settings still asks you to click 'Allow', check"
Write-Host "     %LOCALAPPDATA%\StreamToSpeaker\install.log for the post-install"
Write-Host "     SetEndpointVisibility HRESULT and the final DeviceState line."
