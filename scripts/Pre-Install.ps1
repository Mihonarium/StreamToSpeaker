# Runs as the first step of the installer's [Run] section, every time.
# Cleans up any previous Stream To Speaker install so the new INF lands
# fresh - Windows otherwise caches per-endpoint state across reinstalls
# (jack association, FormFactor, friendly name, enable-by-default flag)
# and serves the stale version even after pnputil /add-driver and
# devcon install of a new INF.
#
# Idempotent: if nothing prior is installed, all the removes / wipes
# silently no-op.
#
# No reboot required - the new install creates a fresh endpoint ID
# (we wipe the MMDevices cache below) so MMDevAPI sees it as brand new
# rather than refreshing stale in-memory state.
#
# Keep this file ASCII. Windows PowerShell reads a script without a
# byte-order mark in the system's ANSI code page; in the single-byte
# Windows code pages a UTF-8 dash turns into three characters, one of
# them a curly quote that ends a string early, and the whole script
# fails to parse.

$ErrorActionPreference = "Continue"  # never abort the installer
$VerbosePreference = "SilentlyContinue"

# Capture all output to <install dir>\logs\install.log so failures are
# diagnosable after the installer's silent run. The installer runs this
# script elevated, so where the log goes follows from where the script is,
# never from the profile of the user who approved the install: that user's
# own programs can turn a folder in the profile into a link to anywhere,
# and an elevated write would follow it. In the install directory, beside
# the installed scripts' folder, the log is exactly as protected as the
# scripts are (Program Files by default, which only administrators can
# change). Rename-Endpoint.ps1 starts its transcript the same way; keep
# the two equal.
$logDir = Join-Path (Split-Path -Parent $PSScriptRoot) "logs"
$logFile = Join-Path $logDir "install.log"
$transcribing = $false
try {
    $null = New-Item -ItemType Directory -Force -Path $logDir -ErrorAction Stop
    $null = Start-Transcript -LiteralPath $logFile -Append -ErrorAction Stop
    $transcribing = $true
} catch {
    Write-Host "Not logging to $logFile ($($_.Exception.Message)); output goes to this console only."
}
"==== Pre-Install started at $(Get-Date -Format 'u') ====" | Write-Host

function Log($msg) {
    Write-Host "[pre-install] $msg"
}

# ----- 1. Remove the live device (if any) ------------------------------------
# devcon is staged alongside the driver by the installer; it has to
# exist by the time this script runs (Inno Setup copies [Files] before
# [Run]). If it's missing for some reason, skip - no live device to
# remove means there's no live device to remove.
$devcon = Join-Path $PSScriptRoot "..\driver\devcon.exe"
if (Test-Path $devcon) {
    Log "removing Root\StreamToSpeaker device (if present)..."
    & $devcon remove "Root\StreamToSpeaker" 2>&1 | Out-Null
} else {
    Log "devcon.exe not staged yet; skipping device removal"
}

# ----- 2. Unstage the old driver package from the driver store --------------
# pnputil /delete-driver takes the OEM-assigned name (oemNN.inf), which
# we don't know up front - look it up, then delete.

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

Log "scanning driver store for Root\StreamToSpeaker packages..."
$removed = 0
foreach ($oem in (Get-StreamToSpeakerDriverPackages)) {
    Log "  removing $oem ..."
    & pnputil /delete-driver $oem /uninstall /force 2>&1 | Out-Null
    $removed++
}
Log "$removed driver-store entries removed"

# ----- 3. Wipe cached MMDevices entries --------------------------------------
# This is the key step that the user manual-uninstall path misses. The
# MMDevices registry tree retains per-endpoint state (DeviceState, the
# user-set friendly name, jack association, etc.) keyed by a hash of
# the hardware ID. Reinstalling the same hardware reuses the cached
# entry, so the new INF's properties are ignored until the cache is
# nuked.
$base = "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render"
$pkeyDeviceDesc = "{a45c254e-df1c-4efd-8020-67d146a850e0},2"
# The endpoint's DeviceDesc is just "Speakers" until Rename-Endpoint.ps1 has run;
# the device-interface name carries "Stream To Speaker" from the INF from the start.
$pkeyInterfaceName = "{b3f8fa53-0004-438e-9003-51a46e139bfc},6"
$wiped = 0
if (Test-Path $base) {
    Get-ChildItem $base | ForEach-Object {
        $propsPath = Join-Path $_.PSPath "Properties"
        if (-not (Test-Path $propsPath)) { return }
        $desc = (Get-ItemProperty -Path $propsPath -Name $pkeyDeviceDesc -ErrorAction SilentlyContinue).$pkeyDeviceDesc

        $iface = (Get-ItemProperty -Path $propsPath -Name $pkeyInterfaceName -ErrorAction SilentlyContinue).$pkeyInterfaceName
        if ($desc -like "*Stream To Speaker*" -or $iface -like "*Stream To Speaker*") {
            Log "  wiping cached endpoint $($_.PSChildName) ($desc)"
            Remove-Item -Path $_.PSPath -Recurse -Force
            $wiped++
        }
    }
}
Log "$wiped cached MMDevices entries wiped"

# ----- 4. Restart AudioEndpointBuilder + AudioSrv ----------------------------
# audiosrv keeps an in-memory copy of every endpoint's metadata
# (form-factor, jack association, DeviceState). Wiping the registry
# alone isn't enough on Win11 24H2 - when devcon installs the new
# INF, the service can re-derive the endpoint from the stale in-mem
# state and ignore our INF properties. Bouncing the services forces
# a cold re-read. Causes ~2 s of audio dropout on any currently
# playing endpoints, but that's unavoidable for a clean re-enrolment.
Log "restarting AudioEndpointBuilder + AudioSrv to drop stale in-memory state..."
try {
    Stop-Service AudioEndpointBuilder -Force -ErrorAction Stop
    Log "  AudioEndpointBuilder stopped"
} catch {
    Log "  Stop-Service AudioEndpointBuilder: $_"
}
try {
    Start-Service AudioEndpointBuilder -ErrorAction Stop
    Log "  AudioEndpointBuilder restarted"
} catch {
    Log "  Start-Service AudioEndpointBuilder: $_"
}
# AudioSrv depends on AudioEndpointBuilder; stopping AEB above
# stopped AudioSrv too. Bring it back if it didn't auto-start.
try {
    Start-Service AudioSrv -ErrorAction SilentlyContinue
    Log "  AudioSrv restarted"
} catch {
    # Ignore - already running or will be lazily started when the
    # first audio client connects.
}

Log "pre-install cleanup complete"
"==== Pre-Install finished at $(Get-Date -Format 'u') ====" | Write-Host
if ($transcribing) { $null = Stop-Transcript }
exit 0
