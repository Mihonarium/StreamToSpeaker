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
    Write-Warning "  devcon.exe not found at $devcon — install may have already removed it."
}

Write-Host "2/3  Removing the driver-store entry..." -ForegroundColor Cyan
$enum = & pnputil /enum-drivers 2>&1 | Out-String
$stanzas = $enum -split "(?ms)(?=Published Name:)"
$matched = 0
foreach ($s in $stanzas) {
    if ($s -match "Original Name:\s*StreamToSpeaker\.inf" -and
        $s -match "Published Name:\s*(oem\d+\.inf)") {
        $oem = $matches[1]
        Write-Host "  removing $oem ..."
        & pnputil /delete-driver $oem /uninstall /force | Out-Null
        $matched++
    }
}
if ($matched -eq 0) {
    Write-Host "  (no StreamToSpeaker driver in the store)"
}

Write-Host "3/3  Wiping cached MMDevices endpoint entries..." -ForegroundColor Cyan
$base = "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render"
$pkeyDeviceDesc = "{a45c254e-df1c-4efd-8020-67d146a850e0},2"
$wiped = 0
Get-ChildItem $base | ForEach-Object {
    $propsPath = Join-Path $_.PSPath "Properties"
    if (-not (Test-Path $propsPath)) { return }
    $desc = (Get-ItemProperty -Path $propsPath -Name $pkeyDeviceDesc -ErrorAction SilentlyContinue).$pkeyDeviceDesc
    if ($desc -like "*Stream To Speaker*") {
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
Write-Host "  1. Reboot (required — MMDevAPI caches in-memory state that"
Write-Host "     won't fully release until next session)."
Write-Host "  2. Re-run StreamToSpeakerSetup-<version>.exe."
Write-Host "  3. After install, the device should appear as 'Stream To Speaker'"
Write-Host "     (without the 'Internal AUX Jack' prefix). You'll still need"
Write-Host "     to click 'Allow' once in Sound Settings — that's a Windows 11"
Write-Host "     per-device privacy gate that requires a manual click."
