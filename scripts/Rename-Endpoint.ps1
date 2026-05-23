# Post-install cleanup for the Stream To Speaker audio endpoint.
# Runs from the installer (Inno Setup [Run] section) or by hand if you
# upgrade over a pre-existing install.
#
# Two registry tweaks, both on whichever Render endpoint(s) match our
# DeviceDesc ("Stream To Speaker"):
#
#  1. PKEY_Device_FriendlyName ({a45c254e-...},14)  →  the user-visible
#     name shown in Sound Settings. Without this the cached
#     "Internal AUX Jack — Stream To Speaker" string survives reinstalls.
#
#  2. DeviceState  →  1 (DEVICE_STATE_ACTIVE). Windows occasionally
#     enrols a new endpoint in DEVICE_STATE_DISABLED (= 2), surfacing it
#     in Sound Settings as a disabled "Allow" toggle the user has to
#     click. Force-active here so the device works out of the box.
#
# Must run elevated (HKLM writes).
#
# Usage:
#   .\scripts\Rename-Endpoint.ps1
#   .\scripts\Rename-Endpoint.ps1 -Name "Picture Frame Sonos"
#   .\scripts\Rename-Endpoint.ps1 -Name "Sonos" -Match "Stream To Speaker"

[CmdletBinding()]
param(
    [string]$Name = "Stream To Speaker",
    [string]$Match = "Stream To Speaker"
)

# PKEY-formatted names of the registry properties on each endpoint's
# Properties subkey.
$pkeyFriendlyName = "{a45c254e-df1c-4efd-8020-67d146a850e0},14"   # user-renameable
$pkeyDeviceDesc   = "{a45c254e-df1c-4efd-8020-67d146a850e0},2"    # original DeviceDesc

# DeviceState values (mmdeviceapi.h).
$DEVICE_STATE_ACTIVE = 1

$base = "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render"

if (-not (Test-Path $base)) {
    Write-Error "MMDevices Render key not present — is the audio service running?"
    exit 1
}

$renamed = 0
$activated = 0
Get-ChildItem $base | ForEach-Object {
    $endpointPath = $_.PSPath
    $propsPath = Join-Path $endpointPath "Properties"
    if (-not (Test-Path $propsPath)) { return }

    $desc = (Get-ItemProperty -Path $propsPath -Name $pkeyDeviceDesc -ErrorAction SilentlyContinue).$pkeyDeviceDesc
    if ($desc -notlike "$Match*") { return }

    # 1. Friendly name.
    try {
        Set-ItemProperty -Path $propsPath -Name $pkeyFriendlyName -Value $Name -Type String
        Write-Host "Renamed endpoint $($_.PSChildName): '$desc' -> '$Name'"
        $renamed++
    }
    catch {
        Write-Warning "Failed to set name on $($_.PSChildName): $_"
    }

    # 2. DeviceState = Active. Only flip if it's currently disabled or
    # unplugged — leave alone if it's already active (so we don't
    # override a user who deliberately disabled it later and re-runs us).
    try {
        $state = (Get-ItemProperty -Path $endpointPath -Name "DeviceState" -ErrorAction SilentlyContinue).DeviceState
        if ($state -ne $DEVICE_STATE_ACTIVE) {
            Set-ItemProperty -Path $endpointPath -Name "DeviceState" -Value $DEVICE_STATE_ACTIVE -Type DWord
            Write-Host "Activated endpoint $($_.PSChildName): DeviceState $state -> $DEVICE_STATE_ACTIVE"
            $activated++
        }
    }
    catch {
        Write-Warning "Failed to set DeviceState on $($_.PSChildName): $_"
    }
}

if ($renamed -eq 0 -and $activated -eq 0) {
    Write-Warning "No endpoint matching '$Match*' found. Is the driver installed and the endpoint enrolled? Try restarting the Windows Audio service if you just installed."
    exit 2
}

# Nudge MMDevAPI to pick up the new state. Without this, Sound Settings
# can keep showing the old name and the disabled "Allow" toggle until
# the next sign-in. Only restart if we actually flipped something, so
# re-runs of the script for a no-op don't churn audio.
if ($activated -gt 0) {
    Write-Host "Restarting Windows Audio service so the activation takes effect immediately..."
    try {
        # AudioEndpointBuilder is the dependency that caches endpoint
        # state; Audiosrv comes back up automatically as a dependent.
        Restart-Service -Name AudioEndpointBuilder -Force
        Write-Host "Audio service restarted."
    }
    catch {
        Write-Warning "Couldn't restart the audio service automatically: $_"
        Write-Warning "Sign out and back in (or reboot) to make Sound Settings reflect the new state."
    }
}
