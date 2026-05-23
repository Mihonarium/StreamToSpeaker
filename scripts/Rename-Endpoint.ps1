# Forces the user-visible name of the "Stream To Speaker" audio endpoint
# in Sound Settings. Use this once after installing or upgrading the
# driver if you see "Internal AUX Jack — Stream To Speaker" instead of
# just "Stream To Speaker" — it writes the same registry value that
# Sound Settings' "Rename" button would.
#
# Must run elevated (the MMDevices registry tree is admin-only).
#
# Usage:
#   .\scripts\Rename-Endpoint.ps1                  # default name "Stream To Speaker"
#   .\scripts\Rename-Endpoint.ps1 -Name "Sonos"    # custom name
#
# Matches any render endpoint whose default device description starts
# with "Stream To Speaker" (i.e. ours, set from the INF FriendlyName).

[CmdletBinding()]
param(
    [string]$Name = "Stream To Speaker",
    [string]$Match = "Stream To Speaker"
)

# Property keys, in the {GUID},PID form the registry uses.
$pkeyFriendlyName = "{a45c254e-df1c-4efd-8020-67d146a850e0},14"   # user-renameable
$pkeyDeviceDesc   = "{a45c254e-df1c-4efd-8020-67d146a850e0},2"    # original DeviceDesc (read-only)

$base = "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render"

if (-not (Test-Path $base)) {
    Write-Error "MMDevices key not present — is the audio service running?"
    exit 1
}

$renamed = 0
Get-ChildItem $base | ForEach-Object {
    $propsPath = Join-Path $_.PSPath "Properties"
    if (-not (Test-Path $propsPath)) { return }

    $desc = (Get-ItemProperty -Path $propsPath -Name $pkeyDeviceDesc -ErrorAction SilentlyContinue).$pkeyDeviceDesc
    if ($desc -notlike "$Match*") { return }

    try {
        Set-ItemProperty -Path $propsPath -Name $pkeyFriendlyName -Value $Name -Type String
        Write-Host "Renamed endpoint $($_.PSChildName): '$desc' -> '$Name'"
        $renamed++
    }
    catch {
        Write-Warning "Failed to set name on $($_.PSChildName): $_"
    }
}

if ($renamed -eq 0) {
    Write-Warning "No endpoint matching '$Match*' found. Is the driver installed and the endpoint enrolled?"
    exit 2
}

# Nudge MMDevAPI to refresh — disable / enable the audio service.
# Without this, Sound Settings sometimes keeps showing the old name
# from its in-memory cache until next sign-in. Commented out by
# default because it briefly drops all audio; uncomment if needed.
#
# Restart-Service -Name Audiosrv -Force
