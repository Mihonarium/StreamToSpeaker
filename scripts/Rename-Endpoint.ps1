# Post-install: name + enable the Stream To Speaker audio endpoint.
#
# Direct registry writes to MMDevices entries (DeviceState, the
# friendly-name PKEY) used to be enough but on Windows 11 24H2+ the
# audio subsystem ignores them - audiosrv re-derives endpoint state
# from the INF + its own cache and overwrites what we set. The
# documented (well, semi-documented) escape hatch is IPolicyConfig,
# the COM interface the Sound Settings app uses internally. It
# RPCs into audiosrv (SYSTEM) which then applies state authoritatively.
#
# This script:
#   1. Finds the render endpoint whose DeviceDesc starts with our
#      product name.
#   2. Sets its friendly name via IPolicyConfig::SetPropertyValue
#      (PKEY_Device_FriendlyName) - same path Sound Settings'
#      Rename button writes to.
#   3. Makes it visible / enabled via
#      IPolicyConfig::SetEndpointVisibility(id, true) - same path
#      the Sound Settings Allow / Disable toggle uses.
#
# All output is captured to %LOCALAPPDATA%\StreamToSpeaker\
# install.log so failures are diagnosable after the installer's
# silent run.

[CmdletBinding()]
param(
    [string]$Name = "Stream To Speaker",
    [string]$Match = "Stream To Speaker"
)

$ErrorActionPreference = "Continue"

$logDir = Join-Path $env:LOCALAPPDATA "StreamToSpeaker"
$null = New-Item -ItemType Directory -Force -Path $logDir -ErrorAction SilentlyContinue
$logFile = Join-Path $logDir "install.log"
$null = Start-Transcript -Path $logFile -Append -ErrorAction SilentlyContinue
"==== Rename-Endpoint started at $(Get-Date -Format 'u') ====" | Write-Host

try {
    # -----------------------------------------------------------------------
    # 1. IPolicyConfig wrapper (inline C# via Add-Type).
    #    The interface is undocumented but stable since Windows Vista.
    #    Reference: github.com/frgnca/AudioDeviceCmdlets (MIT).
    # -----------------------------------------------------------------------
    if (-not ("StreamToSpeaker.PolicyConfigBridge" -as [type])) {
        Add-Type -ErrorAction Stop -Language CSharp -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

namespace StreamToSpeaker {

    [StructLayout(LayoutKind.Sequential, Pack = 4)]
    public struct PROPERTYKEY {
        public Guid fmtid;
        public uint pid;
    }

    [StructLayout(LayoutKind.Sequential)]
    public struct PROPVARIANT {
        public ushort vt;
        public ushort r1;
        public ushort r2;
        public ushort r3;
        public IntPtr p;
        public int    p2;
    }

    [Guid("f8679f50-850a-41cf-9c72-430f290290c8"),
     InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    public interface IPolicyConfig {
        [PreserveSig] int GetMixFormat(string pszDeviceName, IntPtr ppFormat);
        [PreserveSig] int GetDeviceFormat(string pszDeviceName, bool bDefault, IntPtr ppFormat);
        [PreserveSig] int ResetDeviceFormat(string pszDeviceName);
        [PreserveSig] int SetDeviceFormat(string pszDeviceName, IntPtr pEndpointFormat, IntPtr MixFormat);
        [PreserveSig] int GetProcessingPeriod(string pszDeviceName, bool bDefault, IntPtr pmftDefaultPeriod, IntPtr pmftMinimumPeriod);
        [PreserveSig] int SetProcessingPeriod(string pszDeviceName, IntPtr pmftPeriod);
        [PreserveSig] int GetShareMode(string pszDeviceName, IntPtr pMode);
        [PreserveSig] int SetShareMode(string pszDeviceName, IntPtr mode);
        [PreserveSig] int GetPropertyValue(string pszDeviceName, bool bFxStore, ref PROPERTYKEY key, out PROPVARIANT pv);
        [PreserveSig] int SetPropertyValue(string pszDeviceName, bool bFxStore, ref PROPERTYKEY key, ref PROPVARIANT pv);
        [PreserveSig] int SetDefaultEndpoint(string pszDeviceName, int role);
        [PreserveSig] int SetEndpointVisibility(string pszDeviceName, bool bVisible);
    }

    [ComImport, Guid("870af99c-171d-4f9e-af0d-e63df40c2bc9")]
    public class PolicyConfigClient { }

    public static class PolicyConfigBridge {
        // VT_LPWSTR = 31  (PROPVARIANT type tag for wide-string)
        private const ushort VT_LPWSTR = 31;

        public static IPolicyConfig CreateClient() {
            return (IPolicyConfig)(new PolicyConfigClient());
        }

        public static int SetVisible(string endpointId, bool visible) {
            return CreateClient().SetEndpointVisibility(endpointId, visible);
        }

        // Sets a string-valued PKEY. fmtid + pid identify the property,
        // value is the new string (will be marshaled as VT_LPWSTR).
        public static int SetStringProperty(string endpointId, Guid fmtid, uint pid, string value) {
            var key = new PROPERTYKEY { fmtid = fmtid, pid = pid };
            var pv = new PROPVARIANT { vt = VT_LPWSTR };
            pv.p = Marshal.StringToCoTaskMemUni(value);
            try {
                return CreateClient().SetPropertyValue(endpointId, false, ref key, ref pv);
            } finally {
                if (pv.p != IntPtr.Zero) {
                    Marshal.FreeCoTaskMem(pv.p);
                }
            }
        }
    }
}
'@
        Write-Host "Loaded IPolicyConfig wrapper."
    }

    # -----------------------------------------------------------------------
    # 2. Enumerate render endpoints, find ours by DeviceDesc.
    # -----------------------------------------------------------------------
    $base = "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render"
    if (-not (Test-Path $base)) {
        Write-Error "MMDevices Render key missing; is the audio service running?"
        exit 1
    }

    $pkeyFriendlyName_fmtid = [guid] "a45c254e-df1c-4efd-8020-67d146a850e0"
    $pkeyFriendlyName_pid   = 14
    $pkeyDeviceDescStr      = "{a45c254e-df1c-4efd-8020-67d146a850e0},2"

    # Find-our-endpoint with a retry loop. devcon install returns the
    # moment the PnP layer has accepted the device; AudioEndpointBuilder
    # then has to notice the new audio interface and enrol it in
    # MMDevices\Audio\Render. On a slow machine that can take a few
    # seconds, and our [Run] entries fire back-to-back with no pause.
    # Without this loop the script would silently exit-2 and the user
    # would still see the "Allow" toggle off the next time they opened
    # Sound Settings.
    function Find-OurEndpoints {
        $matches = @()
        Get-ChildItem $base -ErrorAction SilentlyContinue | ForEach-Object {
            $endpointGuid = $_.PSChildName
            $propsPath = Join-Path $_.PSPath "Properties"
            if (-not (Test-Path $propsPath)) { return }
            $desc = (Get-ItemProperty -Path $propsPath -Name $pkeyDeviceDescStr -ErrorAction SilentlyContinue).$pkeyDeviceDescStr
            if ($desc -and $desc -like "$Match*") {
                $matches += [pscustomobject]@{ Guid = $endpointGuid; Desc = $desc }
            }
        }
        $matches
    }

    $endpoints = @()
    $maxAttempts = 30  # ~30s total
    for ($attempt = 1; $attempt -le $maxAttempts; $attempt++) {
        $endpoints = @(Find-OurEndpoints)
        if ($endpoints.Count -gt 0) {
            if ($attempt -gt 1) {
                Write-Host "Endpoint appeared after $attempt second(s)."
            }
            break
        }
        Start-Sleep -Seconds 1
    }

    if ($endpoints.Count -eq 0) {
        Write-Warning "No render endpoint matched DeviceDesc starting with '$Match' after ${maxAttempts}s."
        Write-Warning "Either the driver isn't installed, or AudioEndpointBuilder hasn't enrolled the endpoint."
        Write-Warning "Try signing out and back in if you just installed."
        exit 2
    }

    $matched = 0
    foreach ($ep in $endpoints) {
        $matched++
        # Endpoint IDs are of the form "{0.0.0.00000000}.{<guid>}" for
        # render; .1.00000000 for capture. We're only doing render.
        $endpointId = "{0.0.0.00000000}.$($ep.Guid)"
        Write-Host "Matched $($ep.Guid)  (desc='$($ep.Desc)')"

        # --- Friendly name ---
        $hr = [StreamToSpeaker.PolicyConfigBridge]::SetStringProperty(
            $endpointId, $pkeyFriendlyName_fmtid, $pkeyFriendlyName_pid, $Name
        )
        if ($hr -eq 0) {
            Write-Host "  SetStringProperty(FriendlyName='$Name') OK"
        } else {
            Write-Warning "  SetStringProperty failed: HRESULT 0x$('{0:X8}' -f $hr)"
        }

        # --- Visibility (== "Allow apps to use this device") ---
        $hr = [StreamToSpeaker.PolicyConfigBridge]::SetVisible($endpointId, $true)
        if ($hr -eq 0) {
            Write-Host "  SetEndpointVisibility(true) OK"
        } else {
            Write-Warning "  SetEndpointVisibility failed: HRESULT 0x$('{0:X8}' -f $hr)"
        }

        # --- Verify DeviceState landed on ACTIVE (==1). If it stays at
        #     DISABLED (==2) or NOTPRESENT (==4) something fought us. ---
        $statePath = "$base\$($ep.Guid)"
        $state = (Get-ItemProperty -Path $statePath -Name "DeviceState" -ErrorAction SilentlyContinue).DeviceState
        if ($state -ne $null) {
            $stateName = switch ($state) {
                1 { "ACTIVE" }
                2 { "DISABLED" }
                4 { "NOTPRESENT" }
                8 { "UNPLUGGED" }
                default { "0x{0:X}" -f $state }
            }
            Write-Host "  DeviceState = $stateName ($state)"
            if ($state -ne 1) {
                Write-Warning "  Endpoint is not ACTIVE — user may still need to click Allow in Sound Settings."
            }
        }
    }

    Write-Host "Done - $matched endpoint(s) updated."
} catch {
    Write-Warning "Rename-Endpoint failed: $_"
    Write-Warning ($_.ScriptStackTrace)
} finally {
    "==== Rename-Endpoint finished at $(Get-Date -Format 'u') ====" | Write-Host
    $null = Stop-Transcript -ErrorAction SilentlyContinue
}
