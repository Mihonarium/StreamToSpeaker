# StreamToSpeaker — post-install diagnostic.
#
# Run this in an elevated PowerShell session AFTER installing. It
# prints six critical values that pinpoint where the audio endpoint
# classification is failing, plus the post-install log tail.
#
# Usage:
#   powershell -NoProfile -ExecutionPolicy Bypass -File Diagnose.ps1
#
# Paste the entire output into a GitHub issue or back to the
# developer. Non-destructive, runs in ~5 s.

$ErrorActionPreference = "Continue"

function Write-Header($t) { Write-Host ""; Write-Host "=== $t ===" -ForegroundColor Cyan }

Write-Header "0. System"
"OS         : $([System.Environment]::OSVersion.VersionString)"
$wv = Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion' -ErrorAction SilentlyContinue
"Build      : $($wv.CurrentBuildNumber).$($wv.UBR)  ($($wv.DisplayVersion))"
$bcd = & bcdedit /enum '{current}' 2>&1 | Out-String
if ($bcd -match 'testsigning\s+Yes') { "TestSigning: ON" } else { "TestSigning: OFF (driver will NOT load!)" }

Write-Header "1. Driver staged + loaded"
$sys32 = "$env:windir\System32\drivers\StreamToSpeaker.sys"
if (Test-Path $sys32) {
    $vi = (Get-Item $sys32).VersionInfo
    "System32\drivers\StreamToSpeaker.sys  ($($vi.FileVersion))"
    "  Modified  : $((Get-Item $sys32).LastWriteTime)"
    $sig = Get-AuthenticodeSignature -FilePath $sys32 -ErrorAction SilentlyContinue
    "  Signature : $($sig.Status)  ($($sig.SignerCertificate.Subject))"
} else {
    "System32\drivers\StreamToSpeaker.sys NOT FOUND — driver not active"
}
$store = Get-ChildItem "$env:windir\System32\DriverStore\FileRepository" -Filter 'streamtospeaker*' -Directory -ErrorAction SilentlyContinue
if ($store) { foreach ($d in $store) { "Driver store : $($d.FullName)" } }
else { "Driver store : (no StreamToSpeaker entry — pnputil /add-driver never completed)" }

# pnputil & devcon status (if the installer copied devcon under {app}\driver)
"-- pnputil /enum-devices /instanceid Root\StreamToSpeaker --"
& pnputil /enum-devices /instanceid 'Root\StreamToSpeaker' 2>&1 | ForEach-Object { "  $_" }

Write-Header "2. MMDevices registry — the 6 critical values"
$base = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render'
$ffNames = @{ 0='RemoteNetworkDevice'; 1='Speakers'; 2='LineLevel'; 3='Headphones';
              4='Microphone'; 5='Headset'; 6='Handset'; 7='UnknownDigitalPassthrough';
              8='SPDIF'; 9='DigitalAudioDisplayDevice'; 10='UnknownFormFactor' }
$dsNames = @{ 1='ACTIVE'; 2='DISABLED'; 4='NOTPRESENT'; 8='UNPLUGGED' }
if (Test-Path $base) {
    Get-ChildItem $base | ForEach-Object {
        $props = Get-ItemProperty -Path (Join-Path $_.PSPath 'Properties') -ErrorAction SilentlyContinue
        $desc  = $props.'{a45c254e-df1c-4efd-8020-67d146a850e0},2'
        $fname = $props.'{a45c254e-df1c-4efd-8020-67d146a850e0},14'
        if ($desc -notlike '*Stream To Speaker*' -and $fname -notlike '*Stream To Speaker*') { return }
        $top = Get-ItemProperty -Path $_.PSPath -ErrorAction SilentlyContinue
        $st  = [int]$top.DeviceState
        $stName = if ($dsNames.ContainsKey($st)) { $dsNames[$st] } else { '?' }
        $ff  = $props.'{1da5d803-d492-4edd-8c23-e0c0ffee7f0e},0'
        $ffName = if ($null -ne $ff -and $ffNames.ContainsKey([int]$ff)) { $ffNames[[int]$ff] } else { '?' }
        $assoc  = $props.'{1da5d803-d492-4edd-8c23-e0c0ffee7f0e},2'
        $enable = $props.'{f3e80bef-1723-4ff2-bcc4-7f83dc5e46d4},4'
        $jackSub = $props.'{1da5d803-d492-4edd-8c23-e0c0ffee7f0e},8'
        ""
        "Endpoint  : $($_.PSChildName)"
        ("  DeviceState               : 0x{0:X} ({1})" -f $st, $stName)
        ("  PKEY_Device_DeviceDesc    : {0}" -f $desc)
        ("  PKEY_Device_FriendlyName  : {0}" -f $fname)
        ("  AudioEndpoint_FormFactor  : {0} ({1})" -f $ff, $ffName)
        ("  AudioEndpoint_Association : {0}" -f $assoc)
        ("  AudioEndpoint_JackSubType : {0}" -f $jackSub)
        ("  EnableEndpointByDefault   : 0x{0:X8}" -f [int]$enable)
    }
} else {
    "$base does not exist"
}

Write-Header "3. install.log tail (Pre-Install + Rename-Endpoint output)"
$log = "$env:LOCALAPPDATA\StreamToSpeaker\install.log"
if (Test-Path $log) {
    Get-Content $log -Tail 60 -ErrorAction SilentlyContinue | ForEach-Object { "  $_" }
} else {
    "  install.log not at $log — the post-install scripts may have run under the SYSTEM"
    "  account; check: $env:SystemRoot\System32\config\systemprofile\AppData\Local\StreamToSpeaker\install.log"
}

Write-Header "4. setupapi.dev.log — last StreamToSpeaker entries"
$setup = "$env:windir\INF\setupapi.dev.log"
if (Test-Path $setup) {
    Select-String -Path $setup -Pattern 'StreamToSpeaker' -SimpleMatch |
        Select-Object -Last 15 | ForEach-Object { "  $($_.Line)" }
} else {
    "  setupapi.dev.log not found"
}

Write-Header "5. Decision tree — read top to bottom, stop at first match"
@'
- DeviceState=NOTPRESENT (4) or no entry at all:
    -> driver not loading (check section 1: TestSigning, signature, store entry)

- DeviceState=DISABLED (2) AND FormFactor=10 (UnknownFormFactor):
    -> bridge pin Category not respected; check JackSubType (should be a
       KSNODETYPE_* GUID, not all-zero / KSCATEGORY_AUDIO)

- DeviceState=DISABLED (2) AND FormFactor=0 (RemoteNetworkDevice):
    -> Win11 22H2+ privacy gate. Association probably wrong.

- DeviceState=DISABLED (2) AND FormFactor=1 (Speakers)
  AND EnableEndpointByDefault != 0x101:
    -> INF EP\0 properties not written. AEB re-derived from stale cache;
       run Reset-Install.ps1 + reboot + reinstall.

- DeviceState=ACTIVE (1) AND FriendlyName starts with "Internal AUX Jack":
    -> Rename-Endpoint.ps1 didn't run (cached DeviceDesc didn't match its
       filter). Try running it manually:
         powershell -File "C:\Program Files\Stream To Speaker\scripts\Rename-Endpoint.ps1"

- DeviceState=ACTIVE (1) AND FriendlyName="Stream To Speaker":
    -> endpoint is fine; the issue is elsewhere
'@
