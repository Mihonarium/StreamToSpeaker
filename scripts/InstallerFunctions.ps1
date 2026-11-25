# Functions shared by the scripts the installer runs (Install-Driver.ps1,
# Uninstall-Driver.ps1, Rename-Endpoint.ps1, Reset-Install.ps1,
# Set-FirewallRule.ps1, Remove-SignInEntries.ps1, Remove-TestCertificate.ps1).
#
# Each of them loads this file with
#
#   . ([ScriptBlock]::Create([IO.File]::ReadAllText((Join-Path $PSScriptRoot 'InstallerFunctions.ps1'))))
#
# and not by dot-sourcing its path: Windows PowerShell reads [ and ] in a
# command path as wildcards, so a script installed in a folder such as
# "D:\Apps [x64]\Stream To Speaker" could not load it by name. For the same
# reason every file and registry test below uses -LiteralPath, and programs
# are started through Invoke-Tool, never with & and a path.
#
# What the scripts decide is in plain functions that take what they decide
# on as parameters; scripts\tests\InstallerFunctions.Tests.ps1 calls them
# with made-up devices, packages, endpoints and registry trees. The parts
# that touch Windows are kept separate and thin.
#
# Keep this file ASCII. Windows PowerShell reads a script without a
# byte-order mark in the system's ANSI code page; in the single-byte
# Windows code pages a UTF-8 dash turns into three characters, one of them
# a curly quote that ends a string early, and the whole script fails to
# parse.

$StsProductName = 'Stream To Speaker'
$StsHardwareId = 'Root\StreamToSpeaker'
$StsExeName = 'stream-to-speaker.exe'
$StsDriverFiles = @('StreamToSpeaker.sys', 'StreamToSpeaker.inf', 'StreamToSpeaker.cat')
$StsRenderKey = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render'
$StsServiceKey = 'HKLM:\SYSTEM\CurrentControlSet\Services\StreamToSpeaker'
# Where the installer keeps what it must know again at uninstall (the test
# certificates it made Windows trust).
$StsInstallerKey = 'HKLM:\SOFTWARE\StreamToSpeaker\Installer'
$StsTestCertificateSubject = 'CN=Stream To Speaker (test sign)'
$StsFirewallRuleName = 'StreamToSpeaker-Inbound'
$StsPnputil = Join-Path $env:windir 'System32\pnputil.exe'
# Set by the script that loads this file (it knows where the driver folder is).
$StsDevcon = ''
# Put in front of every line Write-Step prints.
$StsLogPrefix = ''

# Endpoint properties in MMDevices\Audio\Render\<guid>\Properties.
$StsPkey = @{
    DeviceDesc      = '{a45c254e-df1c-4efd-8020-67d146a850e0},2'
    FriendlyName    = '{a45c254e-df1c-4efd-8020-67d146a850e0},14'
    # The device interface's name: "Stream To Speaker" from the INF from the
    # start, and not something the person can rename.
    InterfaceName   = '{b3f8fa53-0004-438e-9003-51a46e139bfc},6'
    # "{1}." followed by the instance ID of the device the endpoint belongs to.
    DeviceInstance  = '{b3f8fa53-0004-438e-9003-51a46e139bfc},2'
    FormFactor      = '{1da5d803-d492-4edd-8c23-e0c0ffee7f0e},0'
    Association     = '{1da5d803-d492-4edd-8c23-e0c0ffee7f0e},2'
    EnableByDefault = '{f3e80bef-1723-4ff2-bcc4-7f83dc5e46d4},4'
}
$StsKsNodeTypeSpeaker = '{DFF21CE1-F70F-11D0-B917-00A0C9223196}'
# Associations that early builds left in Windows' cache: the desktop
# microphone GUID (one digit off the speaker's) and the line connector,
# which made Windows call the output "Internal AUX Jack". An endpoint that
# still carries one is rebuilt from scratch.
$StsLegacyAssociations = @(
    '{DFF21BE2-F70F-11D0-B917-00A0C9223196}',
    '{DFF21FE3-F70F-11D0-B917-00A0C9223196}'
)
# DeviceState, low bits.
$StsEndpointActive = 1
$StsEndpointDisabled = 2
$StsEndpointNotPresent = 4

# Device Manager problem codes that installing the same driver files again
# doesn't change: a restart that is still due (14, 38), the device turned
# off by someone (22), a policy (48), a signature Windows doesn't accept
# (52).
$StsProblemsReinstallDoesNotFix = @(14, 22, 38, 48, 52)
# Of those, the ones a restart clears.
$StsProblemsRestartFixes = @(14, 38)

function Write-Step([string]$Message) {
    Write-Host "$StsLogPrefix$Message"
}

# ---------------------------------------------------------------------------
# Plain helpers
# ---------------------------------------------------------------------------

# One argument as the Windows command-line parser (CommandLineToArgvW and
# the C runtime) reads it back.
function ConvertTo-CommandLineArgument([string]$Value) {
    if ($Value -and $Value -notmatch '[\s"]') { return $Value }
    $out = '"'
    $slashes = 0
    foreach ($ch in $Value.ToCharArray()) {
        if ($ch -eq [char]'\') {
            $slashes++
            continue
        }
        if ($ch -eq [char]'"') {
            $out += ('\' * ($slashes * 2 + 1)) + '"'
        } else {
            $out += ('\' * $slashes) + $ch
        }
        $slashes = 0
    }
    $out + ('\' * ($slashes * 2)) + '"'
}

# Runs a program by its literal path, logs what it printed and returns its
# exit code.
function Invoke-Tool {
    param(
        [Parameter(Mandatory = $true)][string]$FilePath,
        [string[]]$Arguments = @()
    )
    $line = ($Arguments | ForEach-Object { ConvertTo-CommandLineArgument $_ }) -join ' '
    Write-Step "> $([IO.Path]::GetFileName($FilePath)) $line"
    if (-not [IO.File]::Exists($FilePath)) {
        Write-Step "  $FilePath is missing"
        return -1
    }
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $FilePath
    $psi.Arguments = $line
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $p = [System.Diagnostics.Process]::Start($psi)
    $stdout = $p.StandardOutput.ReadToEndAsync()
    $stderr = $p.StandardError.ReadToEndAsync()
    $p.WaitForExit()
    foreach ($text in @($stdout.Result, $stderr.Result)) {
        foreach ($l in ($text -split "`r?`n")) {
            if ($l.Trim()) { Write-Step "  $l" }
        }
    }
    Write-Step "  exit code $($p.ExitCode)"
    $p.ExitCode
}

# A path as Windows writes it in a service's ImagePath or a loaded driver's
# file name, as a normal Win32 path.
function ConvertTo-WindowsPath {
    param([string]$Path, [string]$WindowsDirectory = $env:windir)
    if (-not $Path) { return '' }
    $p = $Path.Trim().Trim('"')
    $win = $WindowsDirectory.TrimEnd('\')
    if ($p.StartsWith('\??\') -or $p.StartsWith('\\?\')) { return $p.Substring(4) }
    if ($p -match '^\\SystemRoot\\(.*)$') { return "$win\$($Matches[1])" }
    if ($p -match '^%SystemRoot%\\(.*)$') { return "$win\$($Matches[1])" }
    if ($p -match '^System32\\') { return "$win\$p" }
    $p
}

# SHA-256 of each driver file in a folder; $null for a missing one.
function Get-DriverFileHashes([string]$Folder) {
    $hashes = [ordered]@{}
    foreach ($name in $StsDriverFiles) {
        $hashes[$name] = $null
        if (-not $Folder) { continue }
        $file = Join-Path $Folder $name
        if (Test-Path -LiteralPath $file -PathType Leaf) {
            $hashes[$name] = (Get-FileHash -LiteralPath $file -Algorithm SHA256).Hash.ToLowerInvariant()
        }
    }
    $hashes
}

# Whether two sets of driver file hashes are the same package: the .sys and
# the .inf present in both, and every file equal (a build without a catalog
# has none on either side).
function Test-SameDriverFiles($A, $B) {
    if (-not $A -or -not $B) { return $false }
    foreach ($name in $StsDriverFiles) {
        if ($name -ne 'StreamToSpeaker.cat' -and -not $A[$name]) { return $false }
        if ($A[$name] -ne $B[$name]) { return $false }
    }
    $true
}

# A registry value as a number; $null when it isn't one (missing, text,
# binary).
function ConvertTo-Number($Value) {
    if ($Value -is [int] -or $Value -is [long] -or $Value -is [uint32] -or $Value -is [uint64]) { return [int64]$Value }
    $null
}

# What a Device Manager problem code means, in a few words, for the log.
function Get-DeviceProblemText([int]$Code) {
    switch ($Code) {
        0 { 'working'; break }
        1 { 'not configured'; break }
        10 { 'cannot start'; break }
        14 { 'needs a restart'; break }
        18 { 'drivers need reinstalling'; break }
        22 { 'turned off'; break }
        24 { 'not present'; break }
        28 { 'no driver installed'; break }
        31 { 'not working properly'; break }
        37 { 'driver failed to initialise'; break }
        38 { 'a previous copy of the driver is still loaded'; break }
        39 { 'driver could not be loaded'; break }
        48 { 'driver blocked by a policy'; break }
        52 { 'driver signature not accepted'; break }
        default { "problem code $Code" }
    }
}

# ---------------------------------------------------------------------------
# What is installed now
# ---------------------------------------------------------------------------

# Published names (oemNN.inf) of every Stream To Speaker package in the
# driver store. pnputil /enum-drivers is not parsed: its labels ("Original
# Name:", "Published Name:") are translated on non-English Windows. Every
# published package has its INF copied to %windir%\INF\oemNN.inf, and ours
# is the one declaring our hardware ID.
function Get-OurDriverPackageNames([string]$InfDirectory = (Join-Path $env:windir 'INF')) {
    if (-not (Test-Path -LiteralPath $InfDirectory -PathType Container)) { return @() }
    @(Get-ChildItem -LiteralPath $InfDirectory -Filter 'oem*.inf' -File -ErrorAction SilentlyContinue |
        Where-Object { Select-String -LiteralPath $_.FullName -Pattern $StsHardwareId -SimpleMatch -Quiet -ErrorAction SilentlyContinue } |
        ForEach-Object { $_.Name } |
        Sort-Object)
}

# Every device with our hardware ID: its instance ID, Device Manager problem
# code (not translated, unlike the status text) and the published name of
# the driver package installed on it ($null when Windows doesn't say).
function Get-OurDevices {
    @(Get-CimInstance -ClassName Win32_PnPEntity -ErrorAction Stop |
        Where-Object { $_.HardwareID -and (@($_.HardwareID) -contains $StsHardwareId) } |
        ForEach-Object {
            $inf = $null
            try {
                $inf = (Get-PnpDeviceProperty -InstanceId $_.PNPDeviceID -KeyName 'DEVPKEY_Device_DriverInfPath' -ErrorAction Stop).Data
            } catch { }
            [pscustomobject]@{
                InstanceId  = [string]$_.PNPDeviceID
                ProblemCode = [int]$_.ConfigManagerErrorCode
                InfName     = if ($inf) { [string]$inf } else { $null }
            }
        })
}

# The DriverStore folder of the driver package the kernel service points at
# (the INF installs the .sys with DIRID 13, so ImagePath names the package's
# own folder there); '' when the service isn't installed.
function Get-BoundDriverFolder([string]$ServiceKey = $StsServiceKey) {
    $image = (Get-ItemProperty -LiteralPath $ServiceKey -Name ImagePath -ErrorAction SilentlyContinue).ImagePath
    if (-not $image) { return '' }
    Split-Path -Parent (ConvertTo-WindowsPath $image)
}

# Every render endpoint Windows keeps for us, present or not: matched on any
# of the three names an endpoint has (DeviceDesc is only "Speakers" on a
# healthy install; the friendly name may have been renamed by the person or
# by the app; the interface name always carries ours).
function Get-OurEndpoints {
    param([string]$RenderKey = $StsRenderKey, [string]$Match = $StsProductName)
    if (-not (Test-Path -LiteralPath $RenderKey)) { return @() }
    @(Get-ChildItem -LiteralPath $RenderKey -ErrorAction SilentlyContinue | ForEach-Object {
        $props = Get-ItemProperty -LiteralPath (Join-Path $_.PSPath 'Properties') -ErrorAction SilentlyContinue
        if (-not $props) { return }
        $names = @($props.($StsPkey.FriendlyName), $props.($StsPkey.InterfaceName), $props.($StsPkey.DeviceDesc))
        $ours = @($names | Where-Object { $_ -and ([string]$_).IndexOf($Match, [StringComparison]::OrdinalIgnoreCase) -ge 0 })
        if (-not $ours.Count) { return }
        $state = (Get-ItemProperty -LiteralPath $_.PSPath -Name DeviceState -ErrorAction SilentlyContinue).DeviceState
        [pscustomobject]@{
            Guid            = $_.PSChildName
            Id              = "{0.0.0.00000000}.$($_.PSChildName)"
            KeyPath         = $_.PSPath
            State           = if ($null -ne $state) { ([int64]$state) -band 0xF } else { $null }
            FriendlyName    = $props.($StsPkey.FriendlyName)
            DeviceDesc      = $props.($StsPkey.DeviceDesc)
            InterfaceName   = $props.($StsPkey.InterfaceName)
            DeviceInstance  = $props.($StsPkey.DeviceInstance)
            FormFactor      = $props.($StsPkey.FormFactor)
            Association     = $props.($StsPkey.Association)
            EnableByDefault = $props.($StsPkey.EnableByDefault)
        }
    })
}

# The endpoints that are there now (active or turned off, not "not
# present") and belong to one of these devices. An endpoint that doesn't say
# which device it belongs to counts when it is present.
function Select-PresentEndpoints {
    param([object[]]$Endpoints = @(), [string[]]$InstanceIds = @())
    @($Endpoints | Where-Object {
        $state = $_.State
        if ($state -ne $StsEndpointActive -and $state -ne $StsEndpointDisabled) { return $false }
        if (-not $_.DeviceInstance) { return $true }
        $device = [string]$_.DeviceInstance
        foreach ($id in $InstanceIds) {
            if ($id -and $device.EndsWith($id, [StringComparison]::OrdinalIgnoreCase)) { return $true }
        }
        $false
    })
}

# Endpoints of ours whose device is gone ("not present").
function Select-OrphanedEndpoints([object[]]$Endpoints = @()) {
    @($Endpoints | Where-Object { $_.State -eq $StsEndpointNotPresent })
}

# Whether Windows' cached copy of an endpoint still has what early builds
# left there, which only a fresh endpoint clears.
function Test-LegacyEndpointCache($Endpoint) {
    if ($Endpoint.Association -and ($StsLegacyAssociations -contains ([string]$Endpoint.Association).Trim())) { return $true }
    foreach ($name in @($Endpoint.FriendlyName, $Endpoint.DeviceDesc)) {
        if ($name -and ([string]$name).IndexOf('Internal AUX Jack', [StringComparison]::OrdinalIgnoreCase) -ge 0) { return $true }
    }
    $false
}

# Whether an endpoint's name is one Windows made up ("Speakers (Stream To
# Speaker)", the old "Internal AUX Jack" one, or none), as opposed to ours
# ("Stream To Speaker", or the app's "Stream To Speaker -> Kitchen") or one
# the person chose.
function Test-ComposedEndpointName([string]$Name) {
    if (-not $Name -or -not $Name.Trim()) { return $true }
    if ($Name.IndexOf('Internal AUX Jack', [StringComparison]::OrdinalIgnoreCase) -ge 0) { return $true }
    $Name.Trim().EndsWith("($StsProductName)", [StringComparison]::OrdinalIgnoreCase)
}

# What has to be written to an endpoint, in the order it is written:
# visibility first (Windows 11 recomposes a name written while the
# visibility change is still pending), the name last. -MakeVisible: turn the
# endpoint on if it is off (a new or rebuilt endpoint; a kept one stays as
# the person left it). -Name: the name to give it; without it, only a name
# Windows made up is replaced.
function Get-EndpointChanges {
    param($Endpoint, [bool]$MakeVisible, [string]$Name = '')
    $changes = @()
    if ($MakeVisible -and $Endpoint.State -ne $StsEndpointActive) { $changes += 'Visible' }
    if ((ConvertTo-Number $Endpoint.FormFactor) -ne 1) { $changes += 'FormFactor' }
    if (-not $Endpoint.Association -or ([string]$Endpoint.Association).Trim() -ne $StsKsNodeTypeSpeaker) { $changes += 'Association' }
    if ((ConvertTo-Number $Endpoint.EnableByDefault) -ne 0x101) { $changes += 'EnableByDefault' }
    if ($Name) {
        if ($Endpoint.FriendlyName -ne $Name) { $changes += 'Name' }
    } elseif (Test-ComposedEndpointName $Endpoint.FriendlyName) {
        $changes += 'Name'
    }
    $changes
}

# ---------------------------------------------------------------------------
# What to do with the driver
# ---------------------------------------------------------------------------

# Keep:    the device has the driver Setup brings and nothing a reinstall
#          would fix: leave the device, its endpoint and Windows' audio alone.
# Update:  install the new driver on the existing device, which keeps its
#          endpoint, so Windows keeps it as the sound output it was.
# Create:  there is no device: create one.
# Rebuild: the endpoint carries an early build's cache, which only a new
#          endpoint clears: remove the device, clear the cache, create it
#          again (the caller puts back the default-output roles it had).
# Devices beyond the first are removed in every case (one output only).
function Get-DriverPlan {
    param(
        [object[]]$Devices = @(),
        [bool]$BoundFilesAreNew,
        [bool]$LegacyEndpointCache
    )
    $sorted = @($Devices | Sort-Object @{ Expression = { [int]($_.ProblemCode -ne 0) } }, InstanceId)
    $device = if ($sorted.Count) { $sorted[0] } else { $null }
    if (-not $device) {
        $action = 'Create'
    } elseif ($LegacyEndpointCache) {
        $action = 'Rebuild'
    } elseif ($BoundFilesAreNew -and ($device.ProblemCode -eq 0 -or $StsProblemsReinstallDoesNotFix -contains $device.ProblemCode)) {
        $action = 'Keep'
    } else {
        $action = 'Update'
    }
    $extra = if ($action -eq 'Rebuild') { $sorted } else { @($sorted | Select-Object -Skip 1) }
    [pscustomobject]@{
        Action        = $action
        Device        = $device
        RemoveDevices = @($extra)
    }
}

# Whether Windows must restart before the new driver runs. The driver's
# control device keeps the image it loaded until Windows restarts, so what
# counts is the image that is loaded now, not the file on disk.
#   Running:         the driver is loaded.
#   LoadedKnown:     the list of loaded drivers could be read.
#   LoadedHash:      SHA-256 of the loaded image's file; $null when that file
#                    is gone (its package was deleted).
#   PreviousHash:    what the service pointed at before this install, used
#                    only when the loaded list can't be read.
function Get-RestartDecision {
    param(
        [bool]$Running,
        [bool]$LoadedKnown,
        [string]$LoadedHash,
        [string]$PreviousHash,
        [string]$NewHash
    )
    if (-not $Running) {
        return [pscustomobject]@{ Needed = $false; Reason = 'the driver is not loaded' }
    }
    if ($LoadedKnown) {
        if ($LoadedHash -and $LoadedHash -eq $NewHash) {
            return [pscustomobject]@{ Needed = $false; Reason = 'the loaded driver is this build''s' }
        }
        $why = if ($LoadedHash) { 'a different driver is loaded' } else { 'the loaded driver''s file is gone' }
        return [pscustomobject]@{ Needed = $true; Reason = $why }
    }
    if ($PreviousHash -and $PreviousHash -eq $NewHash) {
        return [pscustomobject]@{ Needed = $false; Reason = 'the driver installed before is this build''s (the loaded list could not be read)' }
    }
    [pscustomobject]@{ Needed = $true; Reason = 'the loaded driver may be an earlier one (the loaded list could not be read)' }
}

# ---------------------------------------------------------------------------
# Windows helpers compiled once per run
# ---------------------------------------------------------------------------

$StsNativeSource = @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;

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

    // IPolicyConfig: undocumented but stable since Windows Vista; the COM
    // interface Sound settings uses. Reference: github.com/frgnca/AudioDeviceCmdlets.
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

    [Guid("A95664D2-9614-4F35-A746-DE8DB63617E6"),
     InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    public interface IMMDeviceEnumerator {
        [PreserveSig] int EnumAudioEndpoints(int dataFlow, int stateMask, out IntPtr devices);
        [PreserveSig] int GetDefaultAudioEndpoint(int dataFlow, int role, out IMMDevice endpoint);
    }

    [Guid("D666063F-1587-4E43-81F1-B948E807363F"),
     InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    public interface IMMDevice {
        [PreserveSig] int Activate(ref Guid iid, int clsCtx, IntPtr activationParams, out IntPtr iface);
        [PreserveSig] int OpenPropertyStore(int access, out IntPtr store);
        [PreserveSig] int GetId([MarshalAs(UnmanagedType.LPWStr)] out string id);
    }

    [ComImport, Guid("BCDE0395-E52F-467C-8E3D-C4579291692E")]
    public class MMDeviceEnumeratorClient { }

    public static class SetupNative {
        private const ushort VT_UI4    = 19;
        private const ushort VT_LPWSTR = 31;

        private static IPolicyConfig Policy() {
            return (IPolicyConfig)(new PolicyConfigClient());
        }

        public static int SetVisible(string endpointId, bool visible) {
            return Policy().SetEndpointVisibility(endpointId, visible);
        }

        // role: 0 console, 1 multimedia, 2 communications.
        public static int SetDefault(string endpointId, int role) {
            return Policy().SetDefaultEndpoint(endpointId, role);
        }

        public static int SetStringProperty(string endpointId, Guid fmtid, uint pid, string value) {
            var key = new PROPERTYKEY { fmtid = fmtid, pid = pid };
            var pv = new PROPVARIANT { vt = VT_LPWSTR };
            pv.p = Marshal.StringToCoTaskMemUni(value);
            try {
                return Policy().SetPropertyValue(endpointId, false, ref key, ref pv);
            } finally {
                if (pv.p != IntPtr.Zero) {
                    Marshal.FreeCoTaskMem(pv.p);
                }
            }
        }

        // The 32-bit value goes in the IntPtr slot itself.
        public static int SetUInt32Property(string endpointId, Guid fmtid, uint pid, uint value) {
            var key = new PROPERTYKEY { fmtid = fmtid, pid = pid };
            var pv = new PROPVARIANT { vt = VT_UI4 };
            pv.p = (IntPtr)(int)value;
            return Policy().SetPropertyValue(endpointId, false, ref key, ref pv);
        }

        // The endpoint ID Windows plays to for a role; null when there is none.
        public static string DefaultRenderEndpoint(int role) {
            var enumerator = (IMMDeviceEnumerator)(new MMDeviceEnumeratorClient());
            IMMDevice device;
            if (enumerator.GetDefaultAudioEndpoint(0, role, out device) != 0 || device == null) {
                return null;
            }
            string id;
            return device.GetId(out id) == 0 ? id : null;
        }

        [DllImport("psapi.dll", SetLastError = true)]
        private static extern bool EnumDeviceDrivers([Out] IntPtr[] imageBases, uint cb, out uint needed);

        [DllImport("psapi.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        private static extern uint GetDeviceDriverFileNameW(IntPtr imageBase, StringBuilder name, uint size);

        // The files of every loaded kernel driver; empty when Windows doesn't
        // give them out.
        public static string[] LoadedDriverFiles() {
            var bases = new IntPtr[1024];
            uint needed;
            if (!EnumDeviceDrivers(bases, (uint)(bases.Length * IntPtr.Size), out needed)) {
                return new string[0];
            }
            if (needed > bases.Length * IntPtr.Size) {
                bases = new IntPtr[needed / IntPtr.Size + 64];
                if (!EnumDeviceDrivers(bases, (uint)(bases.Length * IntPtr.Size), out needed)) {
                    return new string[0];
                }
            }
            var files = new List<string>();
            var name = new StringBuilder(1024);
            int count = (int)(needed / IntPtr.Size);
            for (int i = 0; i < count && i < bases.Length; i++) {
                if (bases[i] == IntPtr.Zero) {
                    continue;
                }
                name.Length = 0;
                if (GetDeviceDriverFileNameW(bases[i], name, (uint)name.Capacity) > 0) {
                    files.Add(name.ToString());
                }
            }
            return files.ToArray();
        }
    }
}
'@

# Compiles the helpers above. Windows PowerShell's Add-Type compiles through
# the temp folder: it writes the C# there, csc.exe writes the DLL next to it,
# and this process then loads that DLL. Setup runs these scripts elevated,
# but the temp folder they inherit is in the profile of the user who
# approved the install, where that user's unelevated programs could change
# those files between the steps. So the compile uses Windows' own temp
# folder, located through Windows itself (the inherited environment is that
# user's too). Unelevated programs can add files of their own there, but
# can't change the files an elevated process puts there and by default can't
# even list the folder; a run by hand without elevation can still compile
# there. The inherited values come back afterwards. The installer lifecycle
# test checks the "Compiling" and "Loaded" lines.
function Initialize-NativeHelpers {
    if ('StreamToSpeaker.SetupNative' -as [type]) { return }
    $inherited = $env:TEMP, $env:TMP
    $env:TEMP = $env:TMP = Join-Path ([Environment]::GetFolderPath('Windows')) 'Temp'
    try {
        Write-Host "Compiling the Windows helpers in $([IO.Path]::GetTempPath())"
        Add-Type -ErrorAction Stop -Language CSharp -TypeDefinition $StsNativeSource
    } finally {
        $env:TEMP, $env:TMP = $inherited
    }
    Write-Host 'Loaded the Windows helpers.'
}

# The loaded driver: Running, whether the list of loaded drivers could be
# read (Known), and the file of our loaded image (Image; '' when none).
function Get-LoadedDriverState {
    $files = @()
    try {
        # Windows 11 gives the addresses of loaded drivers only to a process
        # with the debug privilege turned on (administrators hold it).
        [System.Diagnostics.Process]::EnterDebugMode()
    } catch { }
    try {
        $files = @([StreamToSpeaker.SetupNative]::LoadedDriverFiles())
    } catch {
        Write-Step "  could not list the loaded drivers: $($_.Exception.Message)"
    }
    if ($files.Count) {
        $ours = @($files | Where-Object { [IO.Path]::GetFileName($_) -ieq 'StreamToSpeaker.sys' })
        $image = if ($ours.Count) { ConvertTo-WindowsPath $ours[0] } else { '' }
        return [pscustomobject]@{ Running = [bool]$image; Known = $true; Image = $image }
    }
    $svc = Get-CimInstance -ClassName Win32_SystemDriver -Filter "Name='StreamToSpeaker'" -ErrorAction SilentlyContinue
    [pscustomobject]@{ Running = ($svc -and $svc.State -eq 'Running'); Known = $false; Image = '' }
}

# The endpoint IDs Windows plays to by default, by role (0 console,
# 1 multimedia, 2 communications), among these endpoints.
function Get-DefaultRoles([string[]]$EndpointIds) {
    $roles = @()
    foreach ($role in 0, 1, 2) {
        $id = $null
        try { $id = [StreamToSpeaker.SetupNative]::DefaultRenderEndpoint($role) } catch { }
        if ($id -and ($EndpointIds -contains $id)) { $roles += $role }
    }
    $roles
}

function Set-EndpointState {
    param($Endpoint, [string[]]$Changes, [string]$Name)
    $endpointFmtid = [guid]'1DA5D803-D492-4EDD-8C23-E0C0FFEE7F0E'
    $deviceFmtid = [guid]'F3E80BEF-1723-4FF2-BCC4-7F83DC5E46D4'
    $nameFmtid = [guid]'a45c254e-df1c-4efd-8020-67d146a850e0'
    foreach ($change in $Changes) {
        $hr = switch ($change) {
            'Visible' { [StreamToSpeaker.SetupNative]::SetVisible($Endpoint.Id, $true) }
            'FormFactor' { [StreamToSpeaker.SetupNative]::SetUInt32Property($Endpoint.Id, $endpointFmtid, 0, 1) }
            'Association' { [StreamToSpeaker.SetupNative]::SetStringProperty($Endpoint.Id, $endpointFmtid, 2, $StsKsNodeTypeSpeaker) }
            'EnableByDefault' { [StreamToSpeaker.SetupNative]::SetUInt32Property($Endpoint.Id, $deviceFmtid, 4, 0x101) }
            'Name' { [StreamToSpeaker.SetupNative]::SetStringProperty($Endpoint.Id, $nameFmtid, 14, $Name) }
        }
        Write-Step ("  {0}: HRESULT 0x{1:X8}" -f $change, $hr)
    }
}

# ---------------------------------------------------------------------------
# Changing things
# ---------------------------------------------------------------------------

function Add-DriverPackage([string]$Inf) { Invoke-Tool -FilePath $StsPnputil -Arguments @('/add-driver', $Inf) }
function Update-DeviceDriver([string]$Inf) { Invoke-Tool -FilePath $StsDevcon -Arguments @('update', $Inf, $StsHardwareId) }
function New-Device([string]$Inf) { Invoke-Tool -FilePath $StsDevcon -Arguments @('install', $Inf, $StsHardwareId) }
function Remove-Device([string]$InstanceId) { Invoke-Tool -FilePath $StsDevcon -Arguments @('remove', "@$InstanceId") }
function Remove-DriverPackage([string]$Name) { Invoke-Tool -FilePath $StsPnputil -Arguments @('/delete-driver', $Name) }

# Deletes endpoint entries from Windows' cache. Counts only what is gone
# afterwards: Windows can refuse the delete.
function Remove-EndpointEntries([object[]]$Endpoints = @()) {
    $removed = 0
    $failed = @()
    foreach ($ep in $Endpoints) {
        try {
            Remove-Item -LiteralPath $ep.KeyPath -Recurse -Force -ErrorAction Stop
        } catch {
            Write-Step "  Windows refused to delete the cached entry $($ep.Guid): $($_.Exception.Message)"
        }
        if (Test-Path -LiteralPath $ep.KeyPath) {
            $failed += $ep
        } else {
            $removed++
            Write-Step "  deleted the cached entry $($ep.Guid)"
        }
    }
    [pscustomobject]@{ Removed = $removed; Failed = $failed }
}

# Restarts Windows' audio services, and every service that depends on them
# and was running, so they read the cache again. Sound on the PC stops for a
# moment; only the rebuild of an early build's endpoint needs it.
function Restart-AudioServices {
    $builder = Get-Service -Name AudioEndpointBuilder -ErrorAction SilentlyContinue
    if (-not $builder -or $builder.Status -ne 'Running') {
        Write-Step "Windows audio isn't running; nothing to restart."
        return
    }
    $dependents = @($builder.DependentServices | Where-Object { $_.Status -eq 'Running' } | ForEach-Object { $_.Name })
    # DependentServices lists them in the order they stop; they start the other way round.
    [array]::Reverse($dependents)
    Write-Step 'Restarting Windows audio (sound on this PC stops for a moment)...'
    try {
        Stop-Service -Name AudioEndpointBuilder -Force -ErrorAction Stop
    } catch {
        Write-Step "  stopping AudioEndpointBuilder: $($_.Exception.Message)"
    }
    foreach ($name in @('AudioEndpointBuilder') + $dependents) {
        try {
            Start-Service -Name $name -ErrorAction Stop
            Write-Step "  started $name"
        } catch {
            Write-Step "  starting ${name}: $($_.Exception.Message)"
        }
    }
}

# ---------------------------------------------------------------------------
# Test certificates (test-signed builds only)
# ---------------------------------------------------------------------------

# The thumbprint of a .cer file if it holds one of our test certificates;
# $null otherwise, so a certificate the person trusts for other reasons is
# never touched.
function Get-TestCertificateThumbprint([string]$Path) {
    if (-not $Path -or -not (Test-Path -LiteralPath $Path -PathType Leaf)) { return $null }
    try {
        $c = New-Object System.Security.Cryptography.X509Certificates.X509Certificate2 -ArgumentList $Path
    } catch {
        Write-Step "Could not read the certificate $Path`: $($_.Exception.Message)"
        return $null
    }
    if ($c.Subject -ne $StsTestCertificateSubject) {
        Write-Step "Leaving a certificate alone: unexpected subject '$($c.Subject)'."
        return $null
    }
    $c.Thumbprint.ToUpperInvariant()
}

function Get-RecordedTestCertificates([string]$Key = $StsInstallerKey) {
    $values = (Get-ItemProperty -LiteralPath $Key -Name TestCertificates -ErrorAction SilentlyContinue).TestCertificates
    @($values | Where-Object { $_ -match '^[0-9A-Fa-f]{40}$' } | ForEach-Object { $_.ToUpperInvariant() } | Select-Object -Unique)
}

function Set-RecordedTestCertificates {
    param([string[]]$Thumbprints = @(), [string]$Key = $StsInstallerKey)
    $list = @($Thumbprints | Where-Object { $_ } | Select-Object -Unique)
    if (-not $list.Count) {
        if (Test-Path -LiteralPath $Key) { Remove-Item -LiteralPath $Key -Recurse -Force -ErrorAction SilentlyContinue }
        $parent = Split-Path -Parent $Key
        if ((Test-Path -LiteralPath $parent) -and -not (Get-ChildItem -LiteralPath $parent -ErrorAction SilentlyContinue) -and
            -not (Get-Item -LiteralPath $parent).Property) {
            Remove-Item -LiteralPath $parent -Force -ErrorAction SilentlyContinue
        }
        return
    }
    if (-not (Test-Path -LiteralPath $Key)) { $null = New-Item -Path $Key -Force }
    $null = New-ItemProperty -LiteralPath $Key -Name TestCertificates -PropertyType MultiString -Value $list -Force
}

# Which recorded or earlier certificates to remove: every one but the one
# this build still needs.
function Select-CertificatesToRemove {
    param([string[]]$Candidates = @(), [string]$Keep = '')
    @($Candidates | Where-Object { $_ -and $_ -ne $Keep } | Select-Object -Unique)
}

function Add-TestCertificate([string]$Path) {
    $c = New-Object System.Security.Cryptography.X509Certificates.X509Certificate2 -ArgumentList $Path
    foreach ($storeName in 'Root', 'TrustedPublisher') {
        $store = New-Object System.Security.Cryptography.X509Certificates.X509Store -ArgumentList $storeName, 'LocalMachine'
        $store.Open('ReadWrite')
        try {
            $store.Add($c)
            Write-Step "Trusted the test certificate $($c.Thumbprint) in LocalMachine\$storeName."
        } finally {
            $store.Close()
        }
    }
}

# Removes one of our test certificates from both machine stores; returns how
# many copies went.
function Remove-TestCertificateFromStores([string]$Thumbprint) {
    $removed = 0
    foreach ($storeName in 'Root', 'TrustedPublisher') {
        $store = New-Object System.Security.Cryptography.X509Certificates.X509Store -ArgumentList $storeName, 'LocalMachine'
        $store.Open('ReadWrite')
        try {
            foreach ($cert in @($store.Certificates.Find('FindByThumbprint', $Thumbprint, $false))) {
                if ($cert.Subject -ne $StsTestCertificateSubject) {
                    Write-Step "Leaving $Thumbprint in LocalMachine\$storeName alone: unexpected subject '$($cert.Subject)'."
                    continue
                }
                $store.Remove($cert)
                $removed++
                Write-Step "Removed the test certificate $Thumbprint from LocalMachine\$storeName."
            }
        } finally {
            $store.Close()
        }
    }
    $removed
}

# ---------------------------------------------------------------------------
# Windows Firewall
# ---------------------------------------------------------------------------

# The rule Setup adds. The app accepts connections from speakers: Sonos and
# other UPnP speakers fetch the sound from its HTTP port and send it events
# there, AirPlay receivers send timing and resend requests to its UDP ports,
# AirPlay 2 speakers send clock messages, Music Assistant connects to its
# Sendspin port, and replies to its discovery come back to it. The ports
# can be changed and some are picked at random, so the rule is for the
# program. It covers private and work (domain) networks, from any address,
# because a speaker or Music Assistant can sit on another subnet of the same
# home or office; it doesn't cover networks Windows treats as public, where
# anyone nearby could fetch the PC's sound.
function Get-FirewallRuleSpec([string]$Program) {
    @{
        Name        = $StsFirewallRuleName
        DisplayName = $StsProductName
        Group       = $StsProductName
        Description = 'Lets speakers on your private or work network reach Stream To Speaker: they fetch the sound it plays and answer it. Added by its installer, removed when it is uninstalled.'
        Direction   = 'Inbound'
        Action      = 'Allow'
        Program     = $Program
        Profile     = 'Domain, Private'
        Enabled     = 'True'
    }
}

function Set-AppFirewallRule([string]$Program) {
    $old = @(Get-NetFirewallRule -Name $StsFirewallRuleName -ErrorAction SilentlyContinue)
    if ($old.Count) { $old | Remove-NetFirewallRule -ErrorAction Stop }
    $spec = Get-FirewallRuleSpec $Program
    $null = New-NetFirewallRule @spec -ErrorAction Stop
    Write-Step "Added the Windows Firewall rule '$StsProductName' for $Program (private and work networks)."
}

# Whether a firewall rule's program is this one (paths compared as Windows
# does: any case, environment variables expanded).
function Test-SameProgram([string]$RuleProgram, [string]$Program) {
    if (-not $RuleProgram -or -not $Program) { return $false }
    [Environment]::ExpandEnvironmentVariables($RuleProgram).Trim().Trim('"') -ieq $Program
}

# Removes Setup's rule and every other rule for the program (the ones
# Windows' first-run prompt made); returns how many went.
function Remove-AppFirewallRules([string]$Program) {
    $rules = @(Get-NetFirewallRule -Name $StsFirewallRuleName -ErrorAction SilentlyContinue)
    $rules += @(Get-NetFirewallApplicationFilter -ErrorAction SilentlyContinue |
        Where-Object { Test-SameProgram $_.Program $Program } |
        Get-NetFirewallRule -ErrorAction SilentlyContinue)
    $removed = 0
    foreach ($rule in @($rules | Sort-Object Name -Unique)) {
        try {
            $rule | Remove-NetFirewallRule -ErrorAction Stop
            $removed++
            Write-Step "Removed the Windows Firewall rule '$($rule.DisplayName)' ($($rule.Action), $($rule.Direction))."
        } catch {
            Write-Step "Could not remove the Windows Firewall rule '$($rule.DisplayName)': $($_.Exception.Message)"
        }
    }
    $removed
}

# ---------------------------------------------------------------------------
# Start at sign-in
# ---------------------------------------------------------------------------

# The program a Run value starts: quoted, or up to the first space (the
# same reading as the app's autostart module).
function Get-RunValueProgram([string]$Value) {
    $v = "$Value".TrimStart()
    if ($v.StartsWith('"')) {
        $end = $v.IndexOf('"', 1)
        if ($end -lt 0) { return $v.Substring(1) }
        return $v.Substring(1, $end - 1)
    }
    ($v -split ' ', 2)[0]
}

# Whether a Run value starts a program in this folder.
function Test-RunValueInFolder([string]$Value, [string]$Folder) {
    $program = Get-RunValueProgram $Value
    if (-not $program -or -not $Folder) { return $false }
    $cut = $program.LastIndexOfAny([char[]]@('\', '/'))
    if ($cut -lt 0) { return $false }
    $program.Substring(0, $cut).TrimEnd('\', '/') -ieq $Folder.TrimEnd('\', '/')
}

# Removes the start-at-sign-in entry of every account whose registry is
# loaded (signed in, or in use by a service) when it starts the app from
# this folder; returns how many went. An account that isn't signed in keeps
# its entry, which points at a program that is gone and does nothing.
function Remove-SignInEntries {
    param(
        [Parameter(Mandatory = $true)][string]$InstallDir,
        [string]$UsersRoot = 'Registry::HKEY_USERS',
        [string]$ValueName = $StsProductName
    )
    $removed = 0
    foreach ($user in @(Get-ChildItem -LiteralPath $UsersRoot -ErrorAction SilentlyContinue)) {
        $run = Join-Path $user.PSPath 'Software\Microsoft\Windows\CurrentVersion\Run'
        $value = (Get-ItemProperty -LiteralPath $run -Name $ValueName -ErrorAction SilentlyContinue).$ValueName
        if (-not $value -or -not (Test-RunValueInFolder $value $InstallDir)) { continue }
        Remove-ItemProperty -LiteralPath $run -Name $ValueName -ErrorAction SilentlyContinue
        $approved = Join-Path $user.PSPath 'Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run'
        Remove-ItemProperty -LiteralPath $approved -Name $ValueName -ErrorAction SilentlyContinue
        $removed++
        Write-Step "Removed the start-at-sign-in entry of account $($user.PSChildName)."
    }
    $removed
}

# ---------------------------------------------------------------------------
# Test signing
# ---------------------------------------------------------------------------

# Whether Windows started with test signing on, from
# HKLM\SYSTEM\CurrentControlSet\Control\SystemStartOptions (the options of
# the current boot; not translated, unlike bcdedit's output).
function Test-TestSigningOn([string]$SystemStartOptions) {
    @("$SystemStartOptions" -split '\s+' | Where-Object { $_ -ieq 'TESTSIGNING' }).Count -gt 0
}

# ---------------------------------------------------------------------------
# The installer's driver step
# ---------------------------------------------------------------------------

# The real tools Invoke-DriverInstall uses; tests pass script blocks instead.
function New-DriverTools {
    @{
        FileExists       = { param($Path) [IO.File]::Exists($Path) }
        HashFiles        = 'Get-DriverFileHashes'
        GetDevices       = 'Get-OurDevices'
        GetBoundFolder   = 'Get-BoundDriverFolder'
        GetPackages      = 'Get-OurDriverPackageNames'
        GetEndpoints     = 'Get-OurEndpoints'
        GetDriverState   = 'Get-LoadedDriverState'
        HashFile         = { param($Path) if ($Path -and (Test-Path -LiteralPath $Path -PathType Leaf)) { (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant() } else { $null } }
        TrustCertificate = 'Add-TestCertificate'
        CertificateOf    = 'Get-TestCertificateThumbprint'
        RecordedCerts    = 'Get-RecordedTestCertificates'
        RecordCerts      = { param($List) Set-RecordedTestCertificates -Thumbprints $List }
        RemoveCert       = 'Remove-TestCertificateFromStores'
        Stage            = 'Add-DriverPackage'
        UpdateDevice     = 'Update-DeviceDriver'
        CreateDevice     = 'New-Device'
        RemoveDevice     = 'Remove-Device'
        DeletePackage    = 'Remove-DriverPackage'
        RemoveEndpoints  = 'Remove-EndpointEntries'
        RestartAudio     = 'Restart-AudioServices'
        DefaultRoles     = 'Get-DefaultRoles'
        SetDefault       = { param($Id, $Role) [StreamToSpeaker.SetupNative]::SetDefault($Id, $Role) }
        SetEndpoint      = 'Set-EndpointState'
        Pause            = { Start-Sleep -Seconds 1 }
    }
}

function New-DriverResult([string]$Result, [int]$ExitCode) {
    [pscustomobject]@{
        Result     = $Result
        ExitCode   = $ExitCode
        Problem    = 0
        Restart    = $false
        NewOutput  = $false
        KeptBefore = $false
        Action     = ''
    }
}

# Installs the driver package in -PackageDir on this PC and makes its sound
# output usable; returns what happened (Result):
#   ok                  the device works and its sound output is there
#   trust-failed        a test build's certificate could not be trusted;
#                       nothing else was changed
#   stage-failed        Windows didn't accept the driver package; nothing was
#                       changed (KeptBefore: the earlier driver is still in use)
#   device-failed       the device could not be created or given the driver
#   driver-not-started  the device has the driver but Windows didn't start it
#                       (Problem: the Device Manager problem code)
#   endpoint-missing    the driver runs but no sound output appeared
#   endpoint-off        the sound output is there but turned off in Windows
# plus Restart (Windows must restart before the new driver runs) and
# NewOutput (the sound output was created now: Windows plays to it only once
# someone chooses it, apart from the default roles a rebuilt one gets back).
#
# The working driver is never removed before the new package is in the
# driver store, and nothing is removed when it isn't.
function Invoke-DriverInstall {
    param(
        [Parameter(Mandatory = $true)][string]$PackageDir,
        [string]$PreviousCertificate = '',
        [hashtable]$Tools = (New-DriverTools),
        [int]$DeviceAttempts = 15,
        [int]$EndpointAttempts = 30
    )
    $inf = Join-Path $PackageDir 'StreamToSpeaker.inf'
    $newFiles = & $Tools.HashFiles $PackageDir
    $newSys = $newFiles['StreamToSpeaker.sys']

    # 1. A test build's certificate, which Windows must trust before it
    #    accepts the package.
    $cer = Join-Path $PackageDir 'StreamToSpeaker.cer'
    $currentCert = $null
    $recorded = @(& $Tools.RecordedCerts)
    # The certificate of the build being replaced (versions before this one
    # didn't record theirs) is recorded first, so that whatever happens
    # below, the uninstaller can still find and remove it.
    $previous = if ($PreviousCertificate) { & $Tools.CertificateOf $PreviousCertificate } else { $null }
    if ($previous -and ($recorded -notcontains $previous)) {
        $recorded = @($recorded) + $previous
        $null = & $Tools.RecordCerts $recorded
    }
    if (& $Tools.FileExists $cer) {
        $currentCert = & $Tools.CertificateOf $cer
        try {
            $null = & $Tools.TrustCertificate $cer
        } catch {
            Write-Step "Could not trust the test certificate: $($_.Exception.Message)"
            return (New-DriverResult 'trust-failed' 20)
        }
        if ($currentCert) { $null = & $Tools.RecordCerts (@($recorded) + $currentCert) }
    }

    # 2. What is there now.
    $devices = @(& $Tools.GetDevices)
    $boundFolder = & $Tools.GetBoundFolder
    $boundFiles = & $Tools.HashFiles $boundFolder
    $endpoints = @(& $Tools.GetEndpoints)
    $present = @(Select-PresentEndpoints -Endpoints $endpoints -InstanceIds @($devices | ForEach-Object { $_.InstanceId }))
    $legacy = @($present | Where-Object { Test-LegacyEndpointCache $_ }).Count -gt 0
    $driverBefore = & $Tools.GetDriverState
    $loadedHash = if ($driverBefore.Image) { & $Tools.HashFile $driverBefore.Image } else { $null }
    foreach ($d in $devices) {
        Write-Step "Device $($d.InstanceId): $(Get-DeviceProblemText $d.ProblemCode); driver package $($d.InfName)"
    }
    if (-not $devices.Count) { Write-Step 'No Stream To Speaker device yet.' }
    $plan = Get-DriverPlan -Devices $devices -BoundFilesAreNew (Test-SameDriverFiles $boundFiles $newFiles) -LegacyEndpointCache $legacy
    Write-Step "Plan: $($plan.Action) (driver installed now: $(if (Test-SameDriverFiles $boundFiles $newFiles) { 'the same as this build''s' } elseif ($boundFolder) { 'a different one' } else { 'none' }))"

    # 3. The new package goes into the driver store first.
    if ($plan.Action -ne 'Keep') {
        $code = & $Tools.Stage $inf
        if ($code -ne 0 -and $code -ne 3010) {
            Write-Step 'Windows did not accept the driver package; nothing was changed.'
            $r = New-DriverResult 'stage-failed' 20
            $r.KeptBefore = $devices.Count -gt 0
            $r.Action = $plan.Action
            return $r
        }
    }

    # 4. The device.
    $roles = @()
    switch ($plan.Action) {
        'Update' {
            $code = & $Tools.UpdateDevice $inf
            if ($code -ne 0 -and $code -ne 1) {
                $r = New-DriverResult 'device-failed' 21
                $r.Action = $plan.Action
                return $r
            }
        }
        'Create' {
            $orphans = @(Select-OrphanedEndpoints $endpoints)
            if ($orphans.Count) {
                Write-Step "Clearing $($orphans.Count) cached entr$(if ($orphans.Count -eq 1) { 'y' } else { 'ies' }) of an earlier Stream To Speaker output..."
                $gone = & $Tools.RemoveEndpoints $orphans
                if ($gone.Removed) { $null = & $Tools.RestartAudio }
            }
            $code = & $Tools.CreateDevice $inf
            if ($code -ne 0 -and $code -ne 1) {
                $r = New-DriverResult 'device-failed' 21
                $r.Action = $plan.Action
                return $r
            }
        }
        'Rebuild' {
            $roles = @(& $Tools.DefaultRoles @($present | ForEach-Object { $_.Id }))
            Write-Step "Rebuilding the sound output: Windows' cached copy still has an early build's settings (default for roles: $(if ($roles.Count) { $roles -join ', ' } else { 'none' }))."
        }
    }
    foreach ($extra in $plan.RemoveDevices) {
        Write-Step "Removing the device $($extra.InstanceId)..."
        $null = & $Tools.RemoveDevice $extra.InstanceId
    }
    if ($plan.Action -eq 'Rebuild') {
        $gone = & $Tools.RemoveEndpoints $endpoints
        if ($gone.Failed.Count) { Write-Step "  $($gone.Failed.Count) cached entr$(if ($gone.Failed.Count -eq 1) { 'y' } else { 'ies' }) could not be deleted; the settings below are written to the new output instead." }
        $null = & $Tools.RestartAudio
        $code = & $Tools.CreateDevice $inf
        if ($code -ne 0 -and $code -ne 1) {
            $r = New-DriverResult 'device-failed' 21
            $r.Action = $plan.Action
            return $r
        }
    }

    # 5. The device as Windows has it now.
    $device = $null
    for ($i = 1; $i -le $DeviceAttempts; $i++) {
        $now = @(& $Tools.GetDevices)
        $device = @($now | Sort-Object @{ Expression = { [int]($_.ProblemCode -ne 0) } }, InstanceId) | Select-Object -First 1
        if ($device -and $device.ProblemCode -eq 0) { break }
        if ($i -lt $DeviceAttempts) { $null = & $Tools.Pause }
    }
    if (-not $device) {
        $r = New-DriverResult 'device-failed' 21
        $r.Action = $plan.Action
        return $r
    }
    Write-Step "Device $($device.InstanceId): $(Get-DeviceProblemText $device.ProblemCode); driver package $($device.InfName)"

    # 6. Driver packages and certificates nothing uses any more. A package is
    #    deleted only once the device has the new one.
    $packagesLeft = @()
    if ($device.InfName) {
        foreach ($p in @(& $Tools.GetPackages)) {
            if ($p -ieq $device.InfName) { continue }
            Write-Step "Deleting the driver package $p, which nothing uses now..."
            $code = & $Tools.DeletePackage $p
            if ($code -ne 0) { $packagesLeft += $p }
        }
    } else {
        Write-Step 'Windows did not say which driver package the device has; earlier packages are kept.'
        $packagesLeft = @(& $Tools.GetPackages)
    }
    $stale = @(Select-CertificatesToRemove -Candidates $recorded -Keep $currentCert)
    if ($stale.Count -and -not $packagesLeft.Count) {
        foreach ($t in $stale) { $null = & $Tools.RemoveCert $t }
        $null = & $Tools.RecordCerts @($currentCert | Where-Object { $_ })
    } elseif ($stale.Count) {
        Write-Step 'Keeping the earlier test certificate while its driver package is still in the driver store.'
    }

    # 7. Restart.
    $restart = Get-RestartDecision -Running $driverBefore.Running -LoadedKnown $driverBefore.Known `
        -LoadedHash $loadedHash -PreviousHash $boundFiles['StreamToSpeaker.sys'] -NewHash $newSys
    $result = New-DriverResult 'ok' 0
    $result.Action = $plan.Action
    $result.NewOutput = $plan.Action -eq 'Create' -or $plan.Action -eq 'Rebuild'
    $result.Problem = $device.ProblemCode
    $result.Restart = $restart.Needed
    Write-Step "Restart needed: $(if ($result.Restart) { 'yes' } else { 'no' }) ($($restart.Reason))."

    if ($device.ProblemCode -ne 0) {
        if ($StsProblemsRestartFixes -contains $device.ProblemCode) {
            $result.Restart = $true
            return $result
        }
        $result.Result = 'driver-not-started'
        $result.ExitCode = 22
        return $result
    }

    # 8. The sound output.
    $ours = @()
    for ($i = 1; $i -le $EndpointAttempts; $i++) {
        $ours = @(Select-PresentEndpoints -Endpoints @(& $Tools.GetEndpoints) -InstanceIds @($device.InstanceId))
        if ($ours.Count) {
            if ($i -gt 1) { Write-Step "The sound output appeared after $i tries." }
            break
        }
        if ($i -lt $EndpointAttempts) { $null = & $Tools.Pause }
    }
    if (-not $ours.Count) {
        if ($result.Restart) { return $result }
        $result.Result = 'endpoint-missing'
        $result.ExitCode = 23
        return $result
    }
    foreach ($ep in $ours) {
        $changes = @(Get-EndpointChanges -Endpoint $ep -MakeVisible ($plan.Action -ne 'Keep'))
        Write-Step "Sound output $($ep.Guid) ('$($ep.FriendlyName)'): $(if ($changes.Count) { 'setting ' + ($changes -join ', ') } else { 'nothing to change' })"
        if ($changes.Count) { $null = & $Tools.SetEndpoint $ep $changes $StsProductName }
        foreach ($role in $roles) {
            $hr = & $Tools.SetDefault $ep.Id $role
            Write-Step ("  default for role {0} again: HRESULT 0x{1:X8}" -f $role, $hr)
        }
    }
    $after = @(Select-PresentEndpoints -Endpoints @(& $Tools.GetEndpoints) -InstanceIds @($device.InstanceId))
    if (@($after | Where-Object { $_.State -eq $StsEndpointActive }).Count -eq 0) {
        $result.Result = 'endpoint-off'
        $result.ExitCode = 24
    }
    if ($result.Restart -and $result.ExitCode -eq 0) { $result.ExitCode = 3010 }
    $result
}

# The line the installer reads back from Install-Driver.ps1's output.
function Format-DriverResult($Result) {
    'driver-step-result: {0} restart={1} new-output={2} problem={3} kept-before={4} action={5}' -f
        $Result.Result, [int][bool]$Result.Restart, [int][bool]$Result.NewOutput, [int]$Result.Problem,
        [int][bool]$Result.KeptBefore, $(if ($Result.Action) { $Result.Action } else { 'none' })
}
