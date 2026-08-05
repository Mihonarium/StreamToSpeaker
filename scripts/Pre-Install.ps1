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

$ErrorActionPreference = "Continue"  # never abort the installer
$VerbosePreference = "SilentlyContinue"

# Capture all output to %LOCALAPPDATA%\StreamToSpeaker\install.log so
# failures are diagnosable after the installer's silent run.
$logDir = Join-Path $env:LOCALAPPDATA "StreamToSpeaker"
$null = New-Item -ItemType Directory -Force -Path $logDir -ErrorAction SilentlyContinue
$logFile = Join-Path $logDir "install.log"
$null = Start-Transcript -Path $logFile -Append -ErrorAction SilentlyContinue
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
# we don't know up front - enumerate, match on Original Name, delete.
Log "scanning driver store for StreamToSpeaker.inf entries..."
$enum = & pnputil /enum-drivers 2>&1 | Out-String
$stanzas = $enum -split "(?ms)(?=Published Name:)"
$removed = 0
foreach ($s in $stanzas) {
    if ($s -match "Original Name:\s*StreamToSpeaker\.inf" -and
        $s -match "Published Name:\s*(oem\d+\.inf)") {
        $oem = $matches[1]
        Log "  removing $oem ..."
        & pnputil /delete-driver $oem /uninstall /force 2>&1 | Out-Null
        $removed++
    }
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
$wiped = 0
if (Test-Path $base) {
    Get-ChildItem $base | ForEach-Object {
        $propsPath = Join-Path $_.PSPath "Properties"
        if (-not (Test-Path $propsPath)) { return }
        $desc = (Get-ItemProperty -Path $propsPath -Name $pkeyDeviceDesc -ErrorAction SilentlyContinue).$pkeyDeviceDesc
        if ($desc -like "*Stream To Speaker*") {
            Log "  wiping cached endpoint $($_.PSChildName) ($desc)"
            Remove-Item -Path $_.PSPath -Recurse -Force
            $wiped++
        }
    }
}
Log "$wiped cached MMDevices entries wiped"

# ----- 3b. Remove stale test-signing certificates ---------------------------
# Older installers imported a per-build self-signed cert into the
# machine's Root + TrustedPublisher stores so the test-signed driver
# would install cleanly. Attested (Microsoft-signed) builds don't need
# any cert trust, so clean these up on every install. Harmless for
# test-signed dev builds: the installer re-imports its own .cer right
# after this script runs.
$certsRemoved = 0
foreach ($store in "Root", "TrustedPublisher") {
    Get-ChildItem "Cert:\LocalMachine\$store" -ErrorAction SilentlyContinue |
        Where-Object { $_.Subject -match "Stream To Speaker \(test sign\)" } |
        ForEach-Object {
            Log "  removing stale test cert $($_.Thumbprint) from $store"
            Remove-Item -Path "Cert:\LocalMachine\$store\$($_.Thumbprint)" -Force -ErrorAction SilentlyContinue
            $certsRemoved++
        }
}
Log "$certsRemoved stale test-signing certs removed"

# ----- 4. Restart AudioEndpointBuilder + AudioSrv ----------------------------
# audiosrv keeps an in-memory copy of every endpoint's metadata
# (form-factor, jack association, DeviceState). Wiping the registry
# alone isn't enough on Win11 24H2 — when devcon installs the new
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
    # Ignore — already running or will be lazily started when the
    # first audio client connects.
}

Log "pre-install cleanup complete"
"==== Pre-Install finished at $(Get-Date -Format 'u') ====" | Write-Host
$null = Stop-Transcript -ErrorAction SilentlyContinue
exit 0
