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
# Windows' own folders, from Windows itself: an elevated script inherits the
# environment of the user who approved it, who could have set %windir% to a
# folder of their own.
$StsWindowsDir = [Environment]::GetFolderPath('Windows')
$StsSystemDir = [Environment]::GetFolderPath('System')
$StsPnputil = Join-Path $StsSystemDir 'pnputil.exe'
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
    # Expected to hold "{1}." followed by the instance ID of the device the
    # endpoint belongs to. What it holds on a real PC has not been confirmed,
    # so it only tells two Stream To Speaker devices' outputs apart; no
    # device is created or removed because of it.
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
    param([string]$Path, [string]$WindowsDirectory = $StsWindowsDir)
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
function Get-OurDriverPackageNames([string]$InfDirectory = (Join-Path $StsWindowsDir 'INF')) {
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

# The endpoints that are there now: active or turned off, not "not present".
function Select-PresentEndpoints([object[]]$Endpoints = @()) {
    @($Endpoints | Where-Object { $_.State -eq $StsEndpointActive -or $_.State -eq $StsEndpointDisabled })
}

# Whether Windows records this device as the endpoint's (see
# $StsPkey.DeviceInstance for how far that record is trusted).
function Test-EndpointOfDevice($Endpoint, [string]$InstanceId) {
    $recorded = [string]$Endpoint.DeviceInstance
    [bool]($InstanceId -and $recorded -and $recorded.EndsWith($InstanceId, [StringComparison]::OrdinalIgnoreCase))
}

# The present endpoints to take as one device's: every present endpoint of
# ours, except those Windows records as another Stream To Speaker device's
# (-OtherInstanceIds). An endpoint whose record names no device we know
# counts, so that a record in another form than expected can't make a
# working device look as if it had no sound output.
function Select-DeviceEndpoints {
    param([object[]]$Endpoints = @(), [string]$InstanceId = '', [string[]]$OtherInstanceIds = @())
    @(Select-PresentEndpoints $Endpoints | Where-Object {
        $endpoint = $_
        @($OtherInstanceIds | Where-Object { $_ -and $_ -ine $InstanceId -and (Test-EndpointOfDevice $endpoint $_) }).Count -eq 0
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

# Keep:    the device has the driver Setup brings, its sound output is there,
#          and nothing a reinstall would fix is wrong: leave the device, its
#          output and Windows' audio alone.
# Update:  install the new driver on the existing device, which keeps its
#          output, instead of creating a new device with a new output.
# Create:  there is no device: create one.
# Rebuild: the output carries an early build's cache, or the device works
#          but Windows has no Stream To Speaker output at all although its
#          sound services run (-OutputMissing): create a new device (with a
#          new output) and only then remove the old one and its cache; the
#          caller gives the new output the default roles the old one held.
# The device kept is the one Windows plays to (-PreferredDevice), else a
# working one; devices beyond it are removed in every plan (one output only).
function Get-DriverPlan {
    param(
        [object[]]$Devices = @(),
        [bool]$BoundFilesAreNew,
        [bool]$LegacyEndpointCache,
        [string]$PreferredDevice = '',
        [bool]$OutputMissing
    )
    $sorted = @($Devices | Sort-Object @{ Expression = { [int]($_.InstanceId -ne $PreferredDevice) } },
        @{ Expression = { [int]($_.ProblemCode -ne 0) } }, InstanceId)
    $device = if ($sorted.Count) { $sorted[0] } else { $null }
    $reason = ''
    if (-not $device) {
        $action = 'Create'
        $reason = 'there is no Stream To Speaker device'
    } elseif ($LegacyEndpointCache) {
        $action = 'Rebuild'
        $reason = "Windows' cached copy of the sound output still has an early build's settings"
    } elseif ($OutputMissing -and $device.ProblemCode -eq 0) {
        $action = 'Rebuild'
        $reason = 'the device works but Windows has no Stream To Speaker sound output'
    } elseif ($BoundFilesAreNew -and ($device.ProblemCode -eq 0 -or $StsProblemsReinstallDoesNotFix -contains $device.ProblemCode)) {
        $action = 'Keep'
        $reason = 'the device has this driver already'
    } else {
        $action = 'Update'
        $reason = if ($BoundFilesAreNew) { "the device has this driver but it isn't working" } else { 'the device has another driver' }
    }
    $extra = if ($action -eq 'Rebuild') { $sorted } else { @($sorted | Select-Object -Skip 1) }
    [pscustomobject]@{
        Action        = $action
        Reason        = $reason
        Device        = $device
        RemoveDevices = @($extra)
    }
}

# After a change to the device failed: whether the device that was there
# before (-Before) is still there as it was, with the same driver package on
# it and in the same condition, among the devices Windows has now (-Now).
# It is read back, not assumed: DevCon's "install" puts the driver on every
# Stream To Speaker device, not only on the one it creates. Says in the log
# what it found.
function Test-DeviceAsBefore {
    param($Before, [object[]]$Now = @())
    if (-not $Before) { return $false }
    $device = @($Now | Where-Object { $_.InstanceId -eq $Before.InstanceId }) | Select-Object -First 1
    if (-not $device) {
        Write-Step "The device $($Before.InstanceId), which was there before, is gone."
        return $false
    }
    $same = [bool]($device.InfName -and $device.InfName -ieq $Before.InfName -and $device.ProblemCode -eq $Before.ProblemCode)
    $was = if ($same) { 'as before' } else { "before: $(Get-DeviceProblemText $Before.ProblemCode); driver package $($Before.InfName)" }
    Write-Step "The device $($device.InstanceId), which was there before: $(Get-DeviceProblemText $device.ProblemCode); driver package $($device.InfName) ($was)."
    $same
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
# updateni: as update, but never asks a question (nobody could answer it).
function Update-DeviceDriver([string]$Inf) { Invoke-Tool -FilePath $StsDevcon -Arguments @('updateni', $Inf, $StsHardwareId) }
function New-Device([string]$Inf) { Invoke-Tool -FilePath $StsDevcon -Arguments @('install', $Inf, $StsHardwareId) }
function Remove-Device([string]$InstanceId) { Invoke-Tool -FilePath $StsDevcon -Arguments @('remove', "@$InstanceId") }
function Remove-DriverPackage([string]$Name) { Invoke-Tool -FilePath $StsPnputil -Arguments @('/delete-driver', $Name) }
function Remove-DriverPackageForce([string]$Name) { Invoke-Tool -FilePath $StsPnputil -Arguments @('/delete-driver', $Name, '/uninstall', '/force') }

# Deletes endpoint entries from Windows' cache where Windows allows it: it
# keeps these entries for itself and can refuse the delete even to an
# administrator. Counts only what is gone afterwards. An entry that stays
# does nothing: its device is gone, Windows shows it nowhere, and a device
# created later gets an entry of its own.
function Remove-EndpointEntries([object[]]$Endpoints = @()) {
    $removed = 0
    $failed = @()
    foreach ($ep in $Endpoints) {
        try {
            Remove-Item -LiteralPath $ep.KeyPath -Recurse -Force -ErrorAction Stop
        } catch {
            Write-Step "  Windows keeps the cached entry $($ep.Guid): $($_.Exception.Message)"
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

# Whether both of Windows' sound services run (Windows Audio Endpoint
# Builder and Windows Audio). While they don't, Windows makes no sound output
# for any device, so a missing output says nothing about ours.
function Test-AudioServicesRunning {
    foreach ($name in 'AudioEndpointBuilder', 'AudioSrv') {
        $service = Get-Service -Name $name -ErrorAction SilentlyContinue
        if (-not $service -or $service.Status -ne 'Running') { return $false }
    }
    $true
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

# Setup's rule and every other rule for the program (the ones Windows'
# first-run prompt made); with -OnlyForProgram, only the latter.
function Get-AppFirewallRules([string]$Program, [switch]$OnlyForProgram) {
    $rules = @()
    if (-not $OnlyForProgram) { $rules += @(Get-NetFirewallRule -Name $StsFirewallRuleName -ErrorAction SilentlyContinue) }
    $rules += @(Get-NetFirewallApplicationFilter -ErrorAction SilentlyContinue |
        Where-Object { Test-SameProgram $_.Program $Program } |
        Get-NetFirewallRule -ErrorAction SilentlyContinue)
    @($rules | Sort-Object Name -Unique)
}

# Removes those rules; returns how many went.
function Remove-AppFirewallRules([string]$Program, [switch]$OnlyForProgram) {
    $removed = 0
    foreach ($rule in @(Get-AppFirewallRules $Program -OnlyForProgram:$OnlyForProgram)) {
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

# The accounts (their IDs) whose registry is loaded (signed in, or in use by
# a service) and whose start-at-sign-in entry starts the app from this
# folder.
function Get-SignInAccounts {
    param(
        [Parameter(Mandatory = $true)][string]$InstallDir,
        [string]$UsersRoot = 'Registry::HKEY_USERS',
        [string]$ValueName = $StsProductName
    )
    foreach ($user in @(Get-ChildItem -LiteralPath $UsersRoot -ErrorAction SilentlyContinue)) {
        $run = Join-Path $user.PSPath 'Software\Microsoft\Windows\CurrentVersion\Run'
        $value = (Get-ItemProperty -LiteralPath $run -Name $ValueName -ErrorAction SilentlyContinue).$ValueName
        if ($value -and (Test-RunValueInFolder $value $InstallDir)) { $user.PSChildName }
    }
}

# An account's name as Windows shows it ("PC\name") for its ID, for a
# message a person reads. The ID itself where Windows has no name for it
# (the account was deleted, its domain can't be reached).
function Get-AccountName([string]$Id) {
    try {
        ([System.Security.Principal.SecurityIdentifier]$Id).Translate([System.Security.Principal.NTAccount]).Value
    } catch {
        $Id
    }
}

# Removes the start-at-sign-in entry of each of those accounts; returns how
# many are gone afterwards. An account that isn't signed in keeps its
# entry, which points at a program that is gone and does nothing.
function Remove-SignInEntries {
    param(
        [Parameter(Mandatory = $true)][string]$InstallDir,
        [string]$UsersRoot = 'Registry::HKEY_USERS',
        [string]$ValueName = $StsProductName
    )
    $accounts = @(Get-SignInAccounts -InstallDir $InstallDir -UsersRoot $UsersRoot -ValueName $ValueName)
    foreach ($account in $accounts) {
        $base = "$UsersRoot\$account\Software\Microsoft\Windows\CurrentVersion"
        Remove-ItemProperty -LiteralPath "$base\Run" -Name $ValueName -ErrorAction SilentlyContinue
        Remove-ItemProperty -LiteralPath "$base\Explorer\StartupApproved\Run" -Name $ValueName -ErrorAction SilentlyContinue
    }
    $left = @(Get-SignInAccounts -InstallDir $InstallDir -UsersRoot $UsersRoot -ValueName $ValueName)
    foreach ($account in $accounts) {
        if ($left -contains $account) {
            Write-Step "Could not remove the start-at-sign-in entry of account $account."
        } else {
            Write-Step "Removed the start-at-sign-in entry of account $account."
        }
    }
    $accounts.Count - $left.Count
}

# Points the start-at-sign-in entry of every signed-in account that starts
# the app from -FromFolder at -ToExe instead; returns how many changed.
function Move-SignInEntries {
    param(
        [Parameter(Mandatory = $true)][string]$FromFolder,
        [Parameter(Mandatory = $true)][string]$ToExe,
        [string]$UsersRoot = 'Registry::HKEY_USERS',
        [string]$ValueName = $StsProductName
    )
    $moved = 0
    foreach ($user in @(Get-ChildItem -LiteralPath $UsersRoot -ErrorAction SilentlyContinue)) {
        $run = Join-Path $user.PSPath 'Software\Microsoft\Windows\CurrentVersion\Run'
        $value = (Get-ItemProperty -LiteralPath $run -Name $ValueName -ErrorAction SilentlyContinue).$ValueName
        if (-not $value -or -not (Test-RunValueInFolder $value $FromFolder)) { continue }
        $null = New-ItemProperty -LiteralPath $run -Name $ValueName -Value "`"$ToExe`" --startup" -PropertyType String -Force
        $moved++
        Write-Step "Account $($user.PSChildName) now starts $ToExe at sign-in."
    }
    $moved
}

# ---------------------------------------------------------------------------
# Running copies of the app
# ---------------------------------------------------------------------------

# Whether a file is in a folder or below it (paths compared as Windows does).
function Test-InFolder([string]$Path, [string]$Folder) {
    if (-not $Path -or -not $Folder) { return $false }
    $Path.TrimEnd('\').StartsWith($Folder.TrimEnd('\') + '\', [StringComparison]::OrdinalIgnoreCase)
}

# Which running copies Setup closes and whose they are. Only copies started
# from these folders are closed: they hold files Setup replaces or removes.
# A copy started from anywhere else (a portable copy, a build) is left
# alone. Mine: the copy belongs to the account that ran Setup.
function Get-CopyPlan {
    param([object[]]$Copies = @(), [string[]]$Folders = @(), [string]$PersonSid = '')
    @($Copies | ForEach-Object {
        $copy = $_
        $inFolder = @($Folders | Where-Object { Test-InFolder $copy.Path $_ }).Count -gt 0
        [pscustomobject]@{
            ProcessId = $copy.ProcessId
            Path      = $copy.Path
            Close     = $inFolder
            Mine      = [bool]($PersonSid -and $copy.OwnerSid -and $copy.OwnerSid -eq $PersonSid)
        }
    })
}

# Every running copy of the app: process ID, the file it was started from
# and the account it runs as.
function Get-AppCopies {
    @(Get-CimInstance -ClassName Win32_Process -Filter "Name='$StsExeName'" -ErrorAction SilentlyContinue | ForEach-Object {
        $sid = $null
        try { $sid = (Invoke-CimMethod -InputObject $_ -MethodName GetOwnerSid -ErrorAction Stop).Sid } catch { }
        [pscustomobject]@{ ProcessId = [int]$_.ProcessId; Path = [string]$_.ExecutablePath; OwnerSid = [string]$sid }
    })
}

# The person who ran Setup (Sid; '' when it can't be told), whether Setup
# can start a program as that person, without administrator rights
# (CanStartAs), and whether what Setup runs "as the original user" acts on
# that person's own account, so that its HKCU is the person's (CanActAs).
# -SetupProcessId is the process Setup's [Code] runs in (<setup>.tmp),
# started by the Setup program itself:
#   - When Setup had to ask for elevation, Inno Setup started the Setup
#     program again, elevated, from its first <setup>.tmp, which keeps
#     running as the person, without administrator rights, to run programs
#     as them: the grandparent is a process of the same name, and its owner
#     is the person, also when an administrator's password approved the
#     elevation. Only then can Setup start a program as the person.
#   - Otherwise Setup was started elevated, by the grandparent (Explorer's
#     "Run as administrator", an elevated prompt, a deployment tool), whose
#     owner is the person. Whatever Setup starts then runs with
#     administrator rights, as Setup's own account: it starts no app, and
#     it changes the person's settings only when that account is the
#     person's.
function Get-SetupPerson {
    param(
        [int]$SetupProcessId,
        [scriptblock]$GetProcess = { param($Id) Get-CimInstance -ClassName Win32_Process -Filter "ProcessId=$Id" -ErrorAction SilentlyContinue },
        [scriptblock]$GetOwnerSid = { param($Process) try { [string](Invoke-CimMethod -InputObject $Process -MethodName GetOwnerSid -ErrorAction Stop).Sid } catch { '' } }
    )
    $person = [pscustomobject]@{ Sid = ''; CanStartAs = $false; CanActAs = $false }
    if (-not $SetupProcessId) { return $person }
    $chain = @()
    $p = & $GetProcess $SetupProcessId
    while ($p -and $chain.Count -lt 3) {
        $chain += $p
        if (-not $p.ParentProcessId) { break }
        $parent = & $GetProcess $p.ParentProcessId
        # A process that started after this one only reuses its parent's ID.
        if ($parent -and $parent.CreationDate -and $p.CreationDate -and $parent.CreationDate -gt $p.CreationDate) { break }
        $p = $parent
    }
    if ($chain.Count -lt 2) { return $person }
    $setupAccount = [string](& $GetOwnerSid $chain[1])
    if ($chain.Count -lt 3) {
        # Whoever started the Setup program is gone: Setup's own account.
        $person.Sid = $setupAccount
        $person.CanActAs = [bool]$setupAccount
        return $person
    }
    $person.Sid = [string](& $GetOwnerSid $chain[2])
    $respawned = [bool]($chain[2].Name -and $chain[2].Name -ieq $chain[0].Name)
    $person.CanStartAs = [bool]($person.Sid -and $respawned)
    $person.CanActAs = [bool]($person.Sid -and ($respawned -or $person.Sid -eq $setupAccount))
    $person
}

# Whether an account starts the app at sign-in: 'on'; 'off' when its entry
# is there but turned off in Task Manager or Settings (Windows records that
# under StartupApproved, with an odd first byte); 'none' without an entry.
function Get-SignInState {
    param(
        [string]$Sid,
        [string]$UsersRoot = 'Registry::HKEY_USERS',
        [string]$ValueName = $StsProductName
    )
    if (-not $Sid) { return 'none' }
    $base = "$UsersRoot\$Sid\Software\Microsoft\Windows\CurrentVersion"
    $value = (Get-ItemProperty -LiteralPath "$base\Run" -Name $ValueName -ErrorAction SilentlyContinue).$ValueName
    if (-not $value) { return 'none' }
    $approved = (Get-ItemProperty -LiteralPath "$base\Explorer\StartupApproved\Run" -Name $ValueName -ErrorAction SilentlyContinue).$ValueName
    if ($approved -is [byte[]] -and $approved.Count -gt 0 -and ($approved[0] -band 1)) { return 'off' }
    'on'
}

# The line Setup reads back from Find-RunningCopies.ps1's output.
#   -Closing  the copies to close (Get-CopyPlan's, those with Close set)
#   -Tried    closing them was tried
#   -Still    those of them that still run afterwards
# closed-mine and closed-other count what is closed now, also when another
# copy couldn't be closed: Setup then starts the person's copy again, or
# says that it is closed.
function Format-CopiesResult {
    param([object[]]$Closing = @(), [bool]$Tried, [object[]]$Still = @(), $Person, [string]$SignIn = 'none')
    $mine = @($Closing | Where-Object { $_.Mine }).Count
    $stillIds = @($Still | ForEach-Object { $_.ProcessId })
    $gone = @(if ($Tried) { $Closing | Where-Object { $stillIds -notcontains $_.ProcessId } })
    $goneMine = @($gone | Where-Object { $_.Mine }).Count
    $closed = if (-not $Tried -or -not $Closing.Count) { 'none' } elseif ($Still.Count) { 'failed' } else { 'ok' }
    'running-copies: mine={0} other={1} closed={2} closed-mine={3} closed-other={4} person={5} start={6} settings={7} signin={8}' -f
        $mine, ($Closing.Count - $mine), $closed, $goneMine, ($gone.Count - $goneMine),
        $(if ($Person.Sid) { 'known' } else { 'unknown' }), $(if ($Person.CanStartAs) { 'yes' } else { 'no' }),
        $(if ($Person.CanActAs) { 'yes' } else { 'no' }), $SignIn
}

# Ends these copies by process ID and waits up to 10 s for each; returns the
# ones still running.
function Stop-AppCopies([object[]]$Copies = @()) {
    try { [System.Diagnostics.Process]::EnterDebugMode() } catch { }
    foreach ($c in $Copies) {
        Write-Step "Closing Stream To Speaker (process $($c.ProcessId), $($c.Path))."
        Stop-Process -Id $c.ProcessId -Force -ErrorAction SilentlyContinue
    }
    @($Copies | Where-Object {
        $p = Get-Process -Id $_.ProcessId -ErrorAction SilentlyContinue
        if ($p) { $null = $p.WaitForExit(10000) }
        [bool](Get-Process -Id $_.ProcessId -ErrorAction SilentlyContinue)
    })
}

# ---------------------------------------------------------------------------
# Windows' default output when the app won't run
# ---------------------------------------------------------------------------

# A Role:N value of an MMDevices endpoint (a SYSTEMTIME: when the endpoint
# became the default for that role) as ticks; 0 when it isn't one.
function ConvertFrom-RoleTime($Bytes) {
    if (-not ($Bytes -is [byte[]]) -or $Bytes.Length -lt 16) { return [int64]0 }
    $w = @(0..7 | ForEach-Object { [BitConverter]::ToUInt16($Bytes, $_ * 2) })
    try {
        (New-Object DateTime ([int]$w[0]), ([int]$w[1]), ([int]$w[3]), ([int]$w[4]), ([int]$w[5]), ([int]$w[6]), ([int]$w[7]), ([DateTimeKind]::Utc)).Ticks
    } catch {
        [int64]0
    }
}

# Every render output Windows has: its ID, state, whether it is ours, and
# when it last became the default for each role.
function Get-RenderOutputs([string]$RenderKey = $StsRenderKey) {
    if (-not (Test-Path -LiteralPath $RenderKey)) { return @() }
    @(Get-ChildItem -LiteralPath $RenderKey -ErrorAction SilentlyContinue | ForEach-Object {
        $key = Get-ItemProperty -LiteralPath $_.PSPath -ErrorAction SilentlyContinue
        $props = Get-ItemProperty -LiteralPath (Join-Path $_.PSPath 'Properties') -ErrorAction SilentlyContinue
        $names = @($props.($StsPkey.FriendlyName), $props.($StsPkey.InterfaceName), $props.($StsPkey.DeviceDesc))
        $times = @{}
        foreach ($role in 0, 1, 2) { $times[$role] = ConvertFrom-RoleTime $key."Role:$role" }
        [pscustomobject]@{
            Id        = "{0.0.0.00000000}.$($_.PSChildName)"
            State     = if ($null -ne $key.DeviceState) { ([int64]$key.DeviceState) -band 0xF } else { $null }
            Ours      = @($names | Where-Object { $_ -and ([string]$_).IndexOf($StsProductName, [StringComparison]::OrdinalIgnoreCase) -ge 0 }).Count -gt 0
            RoleTimes = $times
        }
    })
}

# The output to play to instead of ours for a role: the one Windows played
# to most recently before (the newest Role time among the others that work
# now), else any other that works; $null when there is none.
function Select-FallbackOutput([object[]]$Outputs = @(), [int]$Role) {
    $others = @($Outputs | Where-Object { -not $_.Ours -and $_.State -eq $StsEndpointActive })
    if (-not $others.Count) { return $null }
    ($others | Sort-Object @{ Expression = { [int64]$_.RoleTimes[$Role] }; Descending = $true }, Id | Select-Object -First 1).Id
}

# When no copy of the app will run (after a Setup that starts none, and
# when the app is uninstalled), Windows must not stay on the app's output,
# which then plays nowhere: each role it holds goes to the output Windows
# played to before. Returns whether anything moved.
function Move-DefaultOutputAway {
    $outputs = @(Get-RenderOutputs)
    $ours = @($outputs | Where-Object { $_.Ours } | ForEach-Object { $_.Id })
    if (-not $ours.Count) { return $false }
    $moved = $false
    foreach ($role in @(Get-DefaultRoles $ours)) {
        $to = Select-FallbackOutput -Outputs $outputs -Role $role
        if (-not $to) {
            Write-Step "No other sound output works on this PC; Windows stays on Stream To Speaker for role $role."
            continue
        }
        $hr = [StreamToSpeaker.SetupNative]::SetDefault($to, $role)
        Write-Step ("Windows now plays to {0} for role {1} (HRESULT 0x{2:X8}): no copy of Stream To Speaker will be running." -f $to, $role, $hr)
        if ($hr -eq 0) { $moved = $true }
    }
    $moved
}

# ---------------------------------------------------------------------------
# An earlier install in another folder
# ---------------------------------------------------------------------------

# What releases put in their install folder (and the scripts' log), by name.
$StsEarlierInstallFiles = @(
    'stream-to-speaker.exe', 'README.md', 'LICENSE', 'LICENSE-BINARIES.md', 'LICENSE-INSTALLER.txt',
    'unins000.exe', 'unins000.dat', 'unins000.msg',
    'driver\StreamToSpeaker.sys', 'driver\StreamToSpeaker.inf', 'driver\StreamToSpeaker.cat',
    'driver\StreamToSpeaker.cer', 'driver\devcon.exe',
    'scripts\Pre-Install.ps1', 'scripts\Rename-Endpoint.ps1', 'scripts\Reset-Install.ps1', 'scripts\Diagnose.ps1',
    'scripts\Uninstall-Driver.ps1', 'scripts\Remove-TestCertificate.ps1', 'scripts\InstallerFunctions.ps1',
    'scripts\Install-Driver.ps1', 'scripts\Set-FirewallRule.ps1', 'scripts\Remove-SignInEntries.ps1',
    'scripts\Find-RunningCopies.ps1', 'scripts\Remove-EarlierInstall.ps1',
    'logs\install.log')
$StsEarlierInstallFolders = @('logs', 'scripts', 'driver')

# Deleting an earlier install's files without being led elsewhere. The
# earlier folder may be one that programs without administrator rights can
# change. Such a program could wait until Setup has checked a path and then
# put a link in the path's place, so that the delete lands on another
# file: one of this install's, in Program Files. So no file is deleted by
# its path alone: the file is opened, Windows is asked where the opened
# file really is, and it is deleted through that same handle. Compiled
# apart from the helpers of the driver step, and only when an earlier
# install is moved.
$StsEarlierFilesSource = @'
using System;
using System.ComponentModel;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;
using Microsoft.Win32.SafeHandles;

namespace StreamToSpeaker {

    public static class EarlierFiles {
        const uint DeleteAccess = 0x00010000;
        const uint ReadAttributes = 0x0080;
        const uint ShareAll = 7;
        const uint OpenExisting = 3;
        // Needed to open a folder.
        const uint BackupSemantics = 0x02000000;
        // Opens a link itself, not what it leads to.
        const uint OpenReparsePoint = 0x00200000;
        const uint ReparsePointAttribute = 0x0400;
        const int FileDispositionInfo = 4;
        const int FileAttributeTagInfo = 9;
        const int FileDispositionInfoEx = 21;

        [StructLayout(LayoutKind.Sequential)]
        struct AttributeTagInfo {
            public uint FileAttributes;
            public uint ReparseTag;
        }

        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        static extern SafeFileHandle CreateFileW(string name, uint access, uint share, IntPtr security,
            uint disposition, uint flags, IntPtr template);

        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        static extern uint GetFinalPathNameByHandleW(SafeFileHandle file, StringBuilder path, uint size, uint flags);

        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        static extern uint GetLongPathNameW(string path, StringBuilder longPath, uint size);

        [DllImport("kernel32.dll", SetLastError = true)]
        static extern bool GetFileInformationByHandleEx(SafeFileHandle file, int infoClass, out AttributeTagInfo info, uint size);

        [DllImport("kernel32.dll", EntryPoint = "SetFileInformationByHandle", SetLastError = true)]
        static extern bool SetDispositionEx(SafeFileHandle file, int infoClass, ref uint flags, uint size);

        [DllImport("kernel32.dll", EntryPoint = "SetFileInformationByHandle", SetLastError = true)]
        static extern bool SetDisposition(SafeFileHandle file, int infoClass, ref byte delete, uint size);

        static string LastError() {
            return new Win32Exception(Marshal.GetLastWin32Error()).Message;
        }

        // Without the "\\?\" Windows puts in front, and without a "\" at the end.
        static string Plain(string path) {
            if (path.StartsWith(@"\\?\UNC\", StringComparison.OrdinalIgnoreCase)) {
                path = @"\\" + path.Substring(8);
            } else if (path.StartsWith(@"\\?\", StringComparison.Ordinal)) {
                path = path.Substring(4);
            }
            return path.Length > 3 ? path.TrimEnd('\\') : path;
        }

        // Where an opened file or folder is, as Windows gives it: every link
        // in the path followed, every name in its long form. Null when
        // Windows doesn't say.
        static string PathOf(SafeFileHandle handle) {
            var buffer = new StringBuilder(1024);
            uint length = GetFinalPathNameByHandleW(handle, buffer, (uint)buffer.Capacity, 0);
            if (length > buffer.Capacity) {
                buffer = new StringBuilder((int)length + 1);
                length = GetFinalPathNameByHandleW(handle, buffer, (uint)buffer.Capacity, 0);
            }
            if (length == 0 || length > buffer.Capacity) {
                return null;
            }
            return Plain(buffer.ToString());
        }

        // A path with every name in its long form ("C:\PROGRA~1" becomes
        // "C:\Program Files"). Links in it are not followed. Null when
        // nothing is at the path.
        public static string LongPath(string path) {
            string full = Path.GetFullPath(path);
            var buffer = new StringBuilder(1024);
            uint length = GetLongPathNameW(full, buffer, (uint)buffer.Capacity);
            if (length > buffer.Capacity) {
                buffer = new StringBuilder((int)length + 1);
                length = GetLongPathNameW(full, buffer, (uint)buffer.Capacity);
            }
            if (length == 0 || length > buffer.Capacity) {
                return null;
            }
            return Plain(buffer.ToString());
        }

        // Where a folder really is: opened with every link followed. Null
        // when it can't be opened.
        public static string RealPath(string folder) {
            using (SafeFileHandle handle = CreateFileW(folder, ReadAttributes, ShareAll, IntPtr.Zero,
                                                       OpenExisting, BackupSemantics, IntPtr.Zero)) {
                if (handle.IsInvalid) {
                    return null;
                }
                return PathOf(handle);
            }
        }

        // Deletes the file, or the empty folder, that is at this path, and
        // only there. The path is opened without following a link in its
        // last part, and Windows is asked where the opened file is: a link
        // anywhere above it makes that another path. Only when it is this
        // very path is the file deleted, through the same handle, so nothing
        // can be put in its place between the check and the delete.
        // The path must be in its long form (LongPath). Returns "deleted",
        // or why not, in words for the log.
        public static string DeleteIfAt(string path, bool folder) {
            uint flags = OpenReparsePoint | (folder ? BackupSemantics : 0u);
            using (SafeFileHandle handle = CreateFileW(path, DeleteAccess | ReadAttributes, ShareAll, IntPtr.Zero,
                                                       OpenExisting, flags, IntPtr.Zero)) {
                if (handle.IsInvalid) {
                    return "Windows refused: " + LastError();
                }
                AttributeTagInfo tag;
                if (!GetFileInformationByHandleEx(handle, FileAttributeTagInfo, out tag,
                                                  (uint)Marshal.SizeOf(typeof(AttributeTagInfo)))) {
                    return "Windows refused: " + LastError();
                }
                if ((tag.FileAttributes & ReparsePointAttribute) != 0) {
                    return "it is a link";
                }
                string real = PathOf(handle);
                if (real == null) {
                    return "Windows did not say where it really is";
                }
                if (!string.Equals(real, path, StringComparison.OrdinalIgnoreCase)) {
                    return "it is really at " + real;
                }
                // Gone as soon as this handle closes, also while another
                // program has the file open, and also when it is read-only
                // (Windows 10 version 1809 and later, on NTFS)...
                uint request = 0x1 | 0x2 | 0x10;
                if (!SetDispositionEx(handle, FileDispositionInfoEx, ref request, 4)) {
                    // ...or the plain request, which every Windows knows.
                    byte yes = 1;
                    if (!SetDisposition(handle, FileDispositionInfo, ref yes, 1)) {
                        return "Windows refused: " + LastError();
                    }
                }
                return "deleted";
            }
        }
    }
}
'@

# Compiles the helper above, in Windows' own temp folder (see
# Initialize-NativeHelpers for why).
function Initialize-EarlierFilesHelper {
    if ('StreamToSpeaker.EarlierFiles' -as [type]) { return }
    $inherited = $env:TEMP, $env:TMP
    $env:TEMP = $env:TMP = Join-Path ([Environment]::GetFolderPath('Windows')) 'Temp'
    try {
        Add-Type -ErrorAction Stop -Language CSharp -TypeDefinition $StsEarlierFilesSource
    } finally {
        $env:TEMP, $env:TMP = $inherited
    }
}

# Whether a path, or any folder above it up to the drive, is a link,
# junction or other reparse point (or can't be read): then the path may lead
# somewhere other than it says, and the folder is left as it is.
function Test-PathHasLink([string]$Path) {
    $p = $Path.TrimEnd('\')
    while ($p) {
        try {
            $attributes = [IO.File]::GetAttributes($p)
        } catch {
            return $true
        }
        if (($attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { return $true }
        $parent = [IO.Path]::GetDirectoryName($p)
        if (-not $parent -or $parent -eq $p) { break }
        $p = $parent.TrimEnd('\')
        if ($p.Length -le 2) { break }
    }
    $false
}

# Removes from an earlier install folder the files releases put there, by
# name, then the folders they made and the folder itself when empty. Nothing
# in the folder is run. Nothing at all is done when the folder or a folder
# above it is a link, when Windows says the folder really is somewhere other
# than its path, or when it is the folder of this install (-Keep) or holds
# it, under whatever name. Each file and folder is deleted only where it
# really is at its path (see the helper above); one that is a link, or is
# somewhere else, is kept. Result: gone, removed, not-empty (something Setup
# didn't put there is left) or left-alone.
function Remove-EarlierInstall {
    param(
        [Parameter(Mandatory = $true)][string]$Folder,
        [string]$Keep = '',
        [string[]]$Files = $StsEarlierInstallFiles,
        [string[]]$SubFolders = $StsEarlierInstallFolders
    )
    $result = [pscustomobject]@{ Result = ''; Removed = 0; Kept = @() }
    if (-not [IO.Directory]::Exists($Folder)) {
        $result.Result = 'gone'
        return $result
    }
    if (Test-PathHasLink $Folder) {
        Write-Step "$Folder is a link or lies inside one: left as it is."
        $result.Result = 'left-alone'
        return $result
    }
    Initialize-EarlierFilesHelper
    # The folder's path with every name in its long form, and where Windows
    # says the opened folder is: the same, unless a link leads elsewhere.
    $path = [StreamToSpeaker.EarlierFiles]::LongPath($Folder)
    $real = [StreamToSpeaker.EarlierFiles]::RealPath($Folder)
    if (-not $path -or -not $real -or -not [string]::Equals($real, $path, [StringComparison]::OrdinalIgnoreCase)) {
        Write-Step "$Folder is really at '$real', which is not '$path': left as it is."
        $result.Result = 'left-alone'
        return $result
    }
    if ($Keep) {
        $kept = [StreamToSpeaker.EarlierFiles]::RealPath($Keep)
        if (-not $kept) {
            Write-Step "Could not tell where $Keep really is: $Folder is left as it is."
            $result.Result = 'left-alone'
            return $result
        }
        if ([string]::Equals($kept, $real, [StringComparison]::OrdinalIgnoreCase) -or
            $kept.StartsWith($real.TrimEnd('\') + '\', [StringComparison]::OrdinalIgnoreCase)) {
            Write-Step "$Folder holds this install; nothing to remove."
            $result.Result = 'gone'
            return $result
        }
    }
    foreach ($name in $Files) {
        $file = Join-Path $path $name
        if (-not [IO.File]::Exists($file)) { continue }
        $outcome = [StreamToSpeaker.EarlierFiles]::DeleteIfAt($file, $false)
        if ($outcome -eq 'deleted') {
            $result.Removed++
        } else {
            Write-Step "  kept ${name}: $outcome"
            $result.Kept += $name
        }
    }
    foreach ($sub in $SubFolders + @('')) {
        $dir = if ($sub) { Join-Path $path $sub } else { $path }
        if (-not [IO.Directory]::Exists($dir)) { continue }
        if (@([IO.Directory]::EnumerateFileSystemEntries($dir)).Count) { continue }
        $outcome = [StreamToSpeaker.EarlierFiles]::DeleteIfAt($dir, $true)
        if ($outcome -ne 'deleted') { Write-Step "  kept the empty folder ${dir}: $outcome" }
    }
    $result.Result = if ([IO.Directory]::Exists($Folder)) { 'not-empty' } else { 'removed' }
    Write-Step "Removed $($result.Removed) file(s) an earlier install put in $Folder$(if ($result.Result -eq 'not-empty') { '; the folder holds other files and stays' })."
    $result
}

# The shortcuts earlier releases made, by path: the Start-menu entry and the
# desktop shortcut for all users.
function Get-EarlierShortcuts {
    @(
        (Join-Path ([Environment]::GetFolderPath('CommonPrograms')) "$StsProductName\$StsProductName.lnk"),
        (Join-Path ([Environment]::GetFolderPath('CommonDesktopDirectory')) "$StsProductName.lnk"))
}

# Deletes each of these shortcuts that still starts a program in -Folder
# (Setup has made the ones it was asked for, pointing at the new folder,
# before this runs); returns how many went.
function Remove-ShortcutsInto {
    param([string[]]$Links = @(), [Parameter(Mandatory = $true)][string]$Folder)
    $removed = 0
    $shell = $null
    foreach ($link in $Links) {
        if (-not [IO.File]::Exists($link)) { continue }
        if (-not $shell) { $shell = New-Object -ComObject WScript.Shell }
        $target = ''
        try { $target = $shell.CreateShortcut($link).TargetPath } catch { }
        if (-not (Test-InFolder $target $Folder)) { continue }
        try {
            [IO.File]::Delete($link)
            $removed++
            Write-Step "Removed the shortcut $link, which started the earlier copy."
        } catch {
            Write-Step "Could not remove the shortcut ${link}: $($_.Exception.Message)"
        }
    }
    $removed
}

# ---------------------------------------------------------------------------
# Uninstall
# ---------------------------------------------------------------------------

# Removes every Stream To Speaker device and every package of ours in the
# driver store. Returns the lines the uninstaller reads:
#   "left: ..."                what is still there afterwards and
#                              uninstalling again may remove;
#   "removed-at-restart: ..."  a device Windows removes only when it
#                              restarts. It answers so for a device whose
#                              driver was replaced and waits for a restart.
#                              Until then the device's sound output stays
#                              in Windows' list.
# Windows is moved off the app's sound output before the devices go: the app
# no longer runs, so an output that stays until the restart would play
# nowhere. Windows' cached entries of outputs whose device is gone are
# deleted where Windows allows it (see Remove-EndpointEntries). The entry of
# an output that is still there is left alone.
function Invoke-DriverRemoval([hashtable]$Tools = (New-DriverTools)) {
    try {
        $null = & $Tools.MoveDefaultAway
    } catch {
        Write-Step "Could not move Windows off the app's sound output: $($_.Exception.Message)"
    }
    $atRestart = @()
    foreach ($d in @(& $Tools.GetDevices)) {
        Write-Step "Removing the device $($d.InstanceId)..."
        # DevCon's exit code 1: the device is removed when Windows restarts.
        if ((& $Tools.RemoveDevice $d.InstanceId) -eq 1) { $atRestart += $d.InstanceId }
    }
    $packages = @(& $Tools.GetPackages)
    if (-not $packages.Count) { Write-Step 'No Stream To Speaker driver package in the driver store.' }
    foreach ($p in $packages) {
        Write-Step "Deleting the driver package $p..."
        $null = & $Tools.ForceDeletePackage $p
    }
    $lines = @()
    $stillThere = @(@(& $Tools.GetDevices) | Where-Object { $atRestart -notcontains $_.InstanceId })
    foreach ($d in $stillThere) { $lines += "left: the Stream To Speaker device $($d.InstanceId)" }
    foreach ($p in @(& $Tools.GetPackages)) { $lines += "left: the driver package $p in Windows' driver store" }
    foreach ($id in $atRestart) {
        Write-Step "Windows removes the device $id when it restarts. Until then its sound output stays in Windows' list."
        $lines += "removed-at-restart: the Stream To Speaker device $id"
    }
    # The output of a device that just went shows as gone a moment later.
    # One that is still there after the wait belongs to a device Windows
    # removes when it restarts (this run may be a second one, which no
    # longer finds that device).
    $entries = @(& $Tools.GetEndpoints)
    for ($i = 0; $i -lt 5 -and -not $atRestart.Count -and @(Select-PresentEndpoints $entries).Count; $i++) {
        & $Tools.Pause
        $entries = @(& $Tools.GetEndpoints)
    }
    $present = @(Select-PresentEndpoints $entries).Count
    if ($present -and -not $atRestart.Count) {
        Write-Step "Windows still lists the app's sound output. It goes when Windows restarts."
    }
    $gone = @(Select-OrphanedEndpoints $entries)
    if ($gone.Count) { $null = & $Tools.RemoveEndpoints $gone }
    if (-not $lines.Count -and -not $present) { Write-Step 'The driver and its device are removed.' }
    $lines
}

# How a sound output's name is written to the log: ours, the app's and the
# ones Windows makes up as they are; a name the person chose is left out of
# the log, which people post with bug reports.
function Format-EndpointName([string]$Name) {
    if (-not $Name) { return '(no name)' }
    if ($Name -eq $StsProductName -or $Name.StartsWith("$StsProductName $([char]0x2192) ") -or (Test-ComposedEndpointName $Name)) {
        return "'$Name'"
    }
    '(a name chosen in Sound settings)'
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
        FileExists         = { param($Path) [IO.File]::Exists($Path) }
        StartOptions       = { (Get-ItemProperty -LiteralPath 'HKLM:\SYSTEM\CurrentControlSet\Control' -Name SystemStartOptions -ErrorAction SilentlyContinue).SystemStartOptions }
        HashFiles          = 'Get-DriverFileHashes'
        GetDevices         = 'Get-OurDevices'
        GetBoundFolder     = 'Get-BoundDriverFolder'
        GetPackages        = 'Get-OurDriverPackageNames'
        GetEndpoints       = 'Get-OurEndpoints'
        GetDriverState     = 'Get-LoadedDriverState'
        HashFile           = { param($Path) if ($Path -and (Test-Path -LiteralPath $Path -PathType Leaf)) { (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant() } else { $null } }
        TrustCertificate   = 'Add-TestCertificate'
        CertificateOf      = 'Get-TestCertificateThumbprint'
        RecordedCerts      = 'Get-RecordedTestCertificates'
        RecordCerts        = { param($List) Set-RecordedTestCertificates -Thumbprints $List }
        RemoveCert         = 'Remove-TestCertificateFromStores'
        Stage              = 'Add-DriverPackage'
        UpdateDevice       = 'Update-DeviceDriver'
        CreateDevice       = 'New-Device'
        RemoveDevice       = 'Remove-Device'
        DeletePackage      = 'Remove-DriverPackage'
        ForceDeletePackage = 'Remove-DriverPackageForce'
        RemoveEndpoints    = 'Remove-EndpointEntries'
        RestartAudio       = 'Restart-AudioServices'
        AudioRunning       = 'Test-AudioServicesRunning'
        DefaultRoles       = 'Get-DefaultRoles'
        SetDefault         = { param($Id, $Role) [StreamToSpeaker.SetupNative]::SetDefault($Id, $Role) }
        SetEndpoint        = 'Set-EndpointState'
        MoveDefaultAway    = 'Move-DefaultOutputAway'
        Pause              = { Start-Sleep -Seconds 1 }
    }
}

function New-DriverResult([string]$Result, [int]$ExitCode) {
    [pscustomobject]@{
        Result        = $Result
        ExitCode      = $ExitCode
        Problem       = 0
        Restart       = $false
        Replaced      = $false
        NewOutput     = $false
        KeptBefore    = $false
        Action        = ''
        ExtrasRemoved = 0
        ExtrasLeft    = 0
        WasDefault    = $false
        IsDefault     = $false
        SoundMoved    = $false
    }
}

# Installs the driver package in -PackageDir on this PC and makes its sound
# output usable; returns what happened (Result):
#   ok                  the device works and its sound output is there
#   test-signing-off    a test build on a PC whose Windows doesn't run test
#                       builds' drivers: nothing was changed
#   trust-failed        a test build's certificate could not be trusted;
#                       nothing else was changed
#   stage-failed        Windows didn't accept the driver package; nothing was
#                       changed (KeptBefore: there is an earlier driver)
#   device-failed       the device could not be created or given the driver
#                       (KeptBefore: the earlier device is still there as it
#                       was, with the driver it had, as read back)
#   driver-not-started  the device has the driver but Windows didn't start it
#                       (Problem: the Device Manager problem code)
#   endpoint-missing    the driver runs but no sound output appeared
#   endpoint-off        the sound output is there but turned off in Windows
# plus Restart (Windows must restart before the new driver runs), Replaced
# (a different driver is loaded until then), NewOutput (the sound output was
# created now), ExtrasRemoved and ExtrasLeft (other Stream To Speaker
# devices removed, and still there), WasDefault and IsDefault (Windows
# played to Stream To Speaker before, and does now, as checked) and
# SoundMoved (Windows was moved to another output because no copy of the
# app will run).
#
# The working driver is never removed before the new package is in the
# driver store. Earlier packages are deleted only by a run that finds the new
# driver working with no restart due: this run, or else the next time the
# step runs after the restart (nothing deletes them in between). Every test
# certificate is recorded before it is trusted.
function Invoke-DriverInstall {
    param(
        [Parameter(Mandatory = $true)][string]$PackageDir,
        [string]$PreviousCertificate = '',
        [switch]$AllowTestBuild,
        [switch]$NoAppAfterwards,
        [hashtable]$Tools = (New-DriverTools),
        [int]$DeviceAttempts = 15,
        [int]$EndpointAttempts = 30
    )
    $stepArgs = @{
        PackageDir = $PackageDir; PreviousCertificate = $PreviousCertificate; AllowTestBuild = [bool]$AllowTestBuild
        Tools = $Tools; DeviceAttempts = $DeviceAttempts; EndpointAttempts = $EndpointAttempts
    }
    $result = Invoke-DriverInstallSteps @stepArgs
    if ($NoAppAfterwards -and $result.Result -ne 'test-signing-off') {
        # No copy of the app runs after Setup, so the app's output would play
        # nowhere: Windows goes back to the output it played to before.
        $result.SoundMoved = [bool](& $Tools.MoveDefaultAway)
        if ($result.SoundMoved) { $result.IsDefault = $false }
    }
    $result
}

function Invoke-DriverInstallSteps {
    param(
        [string]$PackageDir,
        [string]$PreviousCertificate,
        [bool]$AllowTestBuild,
        [hashtable]$Tools,
        [int]$DeviceAttempts,
        [int]$EndpointAttempts
    )
    $inf = Join-Path $PackageDir 'StreamToSpeaker.inf'
    $cer = Join-Path $PackageDir 'StreamToSpeaker.cer'
    $testBuild = [bool](& $Tools.FileExists $cer)

    # 0. A test build's driver starts only while Windows' test-signing mode is
    #    on. Setup refuses such a PC before it changes anything; this keeps the
    #    step safe when it runs on its own.
    if ($testBuild -and -not $AllowTestBuild -and -not (Test-TestSigningOn (& $Tools.StartOptions))) {
        Write-Step "This is a test build and Windows' test-signing mode is off: its driver couldn't start on this PC, so nothing was changed."
        return (New-DriverResult 'test-signing-off' 25)
    }
    $newFiles = & $Tools.HashFiles $PackageDir
    $newSys = $newFiles['StreamToSpeaker.sys']

    # 1. Certificates: each is recorded before it is trusted, so that the
    #    uninstaller always finds it (versions before this one didn't record
    #    theirs, so the replaced build's comes first).
    $recorded = @(& $Tools.RecordedCerts)
    $previous = if ($PreviousCertificate) { & $Tools.CertificateOf $PreviousCertificate } else { $null }
    $currentCert = if ($testBuild) { & $Tools.CertificateOf $cer } else { $null }
    $newlyTrusted = [bool]($currentCert -and $recorded -notcontains $currentCert -and $previous -ne $currentCert)
    $want = @(@($recorded) + $previous + $currentCert | Where-Object { $_ } | Select-Object -Unique)
    if (($want -join ',') -ne ($recorded -join ',')) {
        $null = & $Tools.RecordCerts $want
        $recorded = $want
    }
    if ($testBuild) {
        $trusted = $true
        try {
            $null = & $Tools.TrustCertificate $cer
        } catch {
            Write-Step "Could not trust the test certificate: $($_.Exception.Message)"
            $trusted = $false
        }
        if (-not $trusted) {
            if ($newlyTrusted) {
                $null = & $Tools.RemoveCert $currentCert
                $recorded = @($recorded | Where-Object { $_ -ne $currentCert })
                $null = & $Tools.RecordCerts $recorded
            }
            return (New-DriverResult 'trust-failed' 20)
        }
    }

    # 2. What is there now, and which device's output Windows plays to.
    $devices = @(& $Tools.GetDevices)
    $boundFolder = & $Tools.GetBoundFolder
    $boundFiles = & $Tools.HashFiles $boundFolder
    $boundIsNew = Test-SameDriverFiles $boundFiles $newFiles
    $endpoints = @(& $Tools.GetEndpoints)
    $present = @(Select-PresentEndpoints $endpoints)
    $legacy = @($present | Where-Object { Test-LegacyEndpointCache $_ }).Count -gt 0
    $rolesBefore = @(if ($present.Count) { & $Tools.DefaultRoles @($present | ForEach-Object { $_.Id }) })
    # Which device's output Windows plays to matters only when there is more
    # than one device; then it is told by what Windows records as each
    # output's device.
    $rolesByDevice = @{}
    foreach ($d in $devices) {
        $ids = @($present | Where-Object { $devices.Count -eq 1 -or (Test-EndpointOfDevice $_ $d.InstanceId) } | ForEach-Object { $_.Id })
        $rolesByDevice[$d.InstanceId] = @(if ($ids.Count) { & $Tools.DefaultRoles $ids })
    }
    $preferred = ''
    $most = 0
    foreach ($id in @($rolesByDevice.Keys | Sort-Object)) {
        if (@($rolesByDevice[$id]).Count -gt $most) {
            $most = @($rolesByDevice[$id]).Count
            $preferred = $id
        }
    }
    $driverBefore = & $Tools.GetDriverState
    $loadedHash = if ($driverBefore.Image) { & $Tools.HashFile $driverBefore.Image } else { $null }
    foreach ($d in $devices) {
        Write-Step "Device $($d.InstanceId): $(Get-DeviceProblemText $d.ProblemCode); driver package $($d.InfName)$(if (@($rolesByDevice[$d.InstanceId]).Count) { '; Windows plays to its output' })"
    }
    if (-not $devices.Count) { Write-Step 'No Stream To Speaker device yet.' }
    foreach ($ep in $present) {
        Write-Step "Sound output $($ep.Guid): $(if ($ep.State -eq $StsEndpointActive) { 'on' } else { 'turned off' }); Windows records as its device: $(if ($ep.DeviceInstance) { $ep.DeviceInstance } else { '(nothing)' })"
    }
    # A device is rebuilt for a missing output only when Windows has no Stream
    # To Speaker output at all, found by name, and its sound services run.
    $audioRunning = [bool](& $Tools.AudioRunning)
    if (-not $present.Count -and $devices.Count) {
        Write-Step "Windows has no Stream To Speaker sound output$(if (-not $audioRunning) { ', and its sound services (Windows Audio, Windows Audio Endpoint Builder) are not running: none can appear until they run' })."
    }
    $plan = Get-DriverPlan -Devices $devices -BoundFilesAreNew $boundIsNew -LegacyEndpointCache $legacy `
        -PreferredDevice $preferred -OutputMissing (-not $present.Count -and $audioRunning)
    Write-Step "Plan: $($plan.Action) ($($plan.Reason))."
    $windowsRestart = $false

    # 3. The new package goes into the driver store first; a package Windows
    #    refuses changes nothing.
    $packagesBefore = @(& $Tools.GetPackages)
    if ($plan.Action -ne 'Keep') {
        $code = & $Tools.Stage $inf
        if ($code -eq 3010) {
            $windowsRestart = $true
        } elseif ($code -ne 0) {
            Write-Step 'Windows did not accept the driver package; nothing was changed.'
            if ($newlyTrusted) {
                # No package needs this build's certificate now.
                $null = & $Tools.RemoveCert $currentCert
                $null = & $Tools.RecordCerts @($recorded | Where-Object { $_ -ne $currentCert })
            }
            $r = New-DriverResult 'stage-failed' 20
            $r.KeptBefore = $devices.Count -gt 0
            $r.Action = $plan.Action
            $r.WasDefault = $rolesBefore.Count -gt 0
            $r.IsDefault = $r.WasDefault
            return $r
        }
    }

    # When the device step fails, a test build's package and certificate that
    # this run added go again, so that Windows trusts nothing no driver needs.
    $undo = {
        if (-not $newlyTrusted) { return }
        $added = @(& $Tools.GetPackages | Where-Object { $packagesBefore -notcontains $_ })
        if (-not $added.Count) {
            Write-Step "Keeping this build's test certificate: its driver package was in the driver store before."
            return
        }
        $stillThere = @($added | Where-Object { (& $Tools.DeletePackage $_) -ne 0 })
        if ($stillThere.Count) {
            Write-Step "Keeping this build's test certificate: its driver package $($stillThere -join ', ') is still in the driver store."
            return
        }
        $null = & $Tools.RemoveCert $currentCert
        $null = & $Tools.RecordCerts @($recorded | Where-Object { $_ -ne $currentCert })
    }

    # 4. The device. A rebuild creates the new device first, next to the old
    #    one, and removes the old one only once the new one works. Creating
    #    it also puts the driver on the old one (see Test-DeviceAsBefore), so
    #    when a change fails, what became of the old device is read back.
    $oldIds = @($devices | ForEach-Object { $_.InstanceId })
    $failed = {
        param([int]$Problem = 0)
        $r = New-DriverResult 'device-failed' 21
        $r.Action = $plan.Action
        $r.Problem = $Problem
        $r.KeptBefore = Test-DeviceAsBefore -Before $plan.Device -Now @(& $Tools.GetDevices)
        $r
    }
    # A device this run made goes again when a rebuild fails.
    $removeNew = {
        foreach ($x in @(& $Tools.GetDevices | Where-Object { $oldIds -notcontains $_.InstanceId })) {
            Write-Step "Removing the new device $($x.InstanceId)..."
            $null = & $Tools.RemoveDevice $x.InstanceId
        }
    }
    if ($plan.Action -eq 'Update') {
        $code = & $Tools.UpdateDevice $inf
        if ($code -eq 1) { $windowsRestart = $true }
        elseif ($code -ne 0) {
            & $undo
            return (& $failed)
        }
    } elseif ($plan.Action -eq 'Create') {
        $orphans = @(Select-OrphanedEndpoints $endpoints)
        if ($orphans.Count) {
            Write-Step "Clearing $($orphans.Count) cached entr$(if ($orphans.Count -eq 1) { 'y' } else { 'ies' }) of an earlier Stream To Speaker output..."
            $gone = & $Tools.RemoveEndpoints $orphans
            if ($gone.Removed) { $null = & $Tools.RestartAudio }
        }
        $code = & $Tools.CreateDevice $inf
        if ($code -eq 1) { $windowsRestart = $true }
        elseif ($code -ne 0) {
            & $undo
            return (& $failed)
        }
    } elseif ($plan.Action -eq 'Rebuild') {
        $code = & $Tools.CreateDevice $inf
        if ($code -eq 1) { $windowsRestart = $true }
        elseif ($code -ne 0) {
            & $removeNew
            & $undo
            return (& $failed)
        }
    }

    # 5. The device as Windows has it now: the new one after a rebuild or a
    #    create, else the one kept.
    $device = $null
    for ($i = 1; $i -le $DeviceAttempts; $i++) {
        $now = @(& $Tools.GetDevices)
        $candidates = if ($plan.Action -eq 'Rebuild' -or $plan.Action -eq 'Create') {
            @($now | Where-Object { $oldIds -notcontains $_.InstanceId })
        } else {
            @($now | Where-Object { $_.InstanceId -eq $plan.Device.InstanceId })
        }
        $device = $candidates | Select-Object -First 1
        if ($device -and $device.ProblemCode -eq 0) { break }
        if ($i -lt $DeviceAttempts) { $null = & $Tools.Pause }
    }
    if (-not $device) {
        if ($plan.Action -ne 'Rebuild') { & $undo }
        return (& $failed)
    }
    if ($plan.Action -eq 'Rebuild' -and $device.ProblemCode -ne 0 -and $StsProblemsRestartFixes -notcontains $device.ProblemCode) {
        # The new device doesn't work: it goes, and the old one is not removed.
        Write-Step "The new device $($device.InstanceId) doesn't work ($(Get-DeviceProblemText $device.ProblemCode))."
        & $removeNew
        return (& $failed $device.ProblemCode)
    }

    # The devices beyond the one kept (and, in a rebuild, the old one) go now.
    $removing = @($plan.RemoveDevices)
    foreach ($x in $removing) {
        Write-Step "Removing the device $($x.InstanceId)..."
        $null = & $Tools.RemoveDevice $x.InstanceId
    }
    if ($plan.Action -eq 'Rebuild') {
        # The old outputs' cached entries go too.
        $gone = & $Tools.RemoveEndpoints $endpoints
        if (@($gone.Failed).Count) { Write-Step "  Windows kept $(@($gone.Failed).Count) cached entr$(if (@($gone.Failed).Count -eq 1) { 'y' } else { 'ies' }) of the old output." }
    }
    $now = @(& $Tools.GetDevices)
    $extrasLeft = @($now | Where-Object { $_.InstanceId -ne $device.InstanceId }).Count
    Write-Step "Device $($device.InstanceId): $(Get-DeviceProblemText $device.ProblemCode); driver package $($device.InfName)"
    if ($extrasLeft) { Write-Step "$extrasLeft other Stream To Speaker device(s) could not be removed." }

    # 6. Restart: when the driver Windows has loaded isn't this one, or when
    #    Windows itself said so.
    $restart = Get-RestartDecision -Running $driverBefore.Running -LoadedKnown $driverBefore.Known `
        -LoadedHash $loadedHash -PreviousHash $boundFiles['StreamToSpeaker.sys'] -NewHash $newSys
    $result = New-DriverResult 'ok' 0
    $result.Action = $plan.Action
    $result.NewOutput = $plan.Action -eq 'Create' -or $plan.Action -eq 'Rebuild'
    $result.Problem = $device.ProblemCode
    $result.Restart = $restart.Needed -or $windowsRestart
    $result.Replaced = $restart.Needed
    # The old device of a rebuild is part of the rebuild, not an extra one.
    $extras = if ($plan.Action -eq 'Rebuild') { @($removing | Where-Object { $_.InstanceId -ne $plan.Device.InstanceId }) } else { $removing }
    $result.ExtrasRemoved = @($extras | Where-Object { @($now | ForEach-Object { $_.InstanceId }) -notcontains $_.InstanceId }).Count
    $result.ExtrasLeft = $extrasLeft
    $result.WasDefault = $rolesBefore.Count -gt 0
    if ($windowsRestart) { Write-Step 'Windows asked for a restart to finish installing the driver.' }
    Write-Step "Restart needed: $(if ($result.Restart) { 'yes' } else { 'no' }) ($($restart.Reason)$(if ($windowsRestart) { '; Windows asked for one' }))."

    # 7. Earlier packages, and the certificates only they needed, go once
    #    the new driver is known to run: the device works and no restart is
    #    due. Until then they stay, so that the PC can go back to them; they
    #    go the next time this step runs and finds the new driver working
    #    (after the restart, nothing runs it until Setup is run again).
    $packagesLeft = @(& $Tools.GetPackages | Where-Object { $_ -ine $device.InfName })
    if ($device.ProblemCode -eq 0 -and -not $result.Restart -and $device.InfName) {
        $kept = @()
        foreach ($p in $packagesLeft) {
            Write-Step "Deleting the driver package $p, which nothing uses now..."
            $code = & $Tools.DeletePackage $p
            if ($code -ne 0) { $kept += $p }
        }
        $packagesLeft = $kept
    } elseif ($packagesLeft.Count) {
        Write-Step "Keeping the earlier driver package(s) $($packagesLeft -join ', ') for now: Setup deletes them the next time it runs and finds the new driver working."
    }
    $stale = @(Select-CertificatesToRemove -Candidates $recorded -Keep $currentCert)
    if ($stale.Count -and -not $packagesLeft.Count) {
        foreach ($t in $stale) { $null = & $Tools.RemoveCert $t }
        $null = & $Tools.RecordCerts @($currentCert | Where-Object { $_ })
    } elseif ($stale.Count) {
        Write-Step 'Keeping the earlier test certificate while its driver package is still in the driver store.'
    }

    if ($device.ProblemCode -ne 0) {
        if ($StsProblemsRestartFixes -contains $device.ProblemCode) {
            $result.Restart = $true
            if ($result.ExitCode -eq 0) { $result.ExitCode = 3010 }
            return $result
        }
        $result.Result = 'driver-not-started'
        $result.ExitCode = 22
        return $result
    }

    # 8. The sound output: every Stream To Speaker output that is there now,
    #    except one Windows records as another Stream To Speaker device's.
    $otherIds = @($oldIds + @($now | ForEach-Object { $_.InstanceId }) | Select-Object -Unique)
    $ours = @()
    for ($i = 1; $i -le $EndpointAttempts; $i++) {
        $ours = @(Select-DeviceEndpoints -Endpoints @(& $Tools.GetEndpoints) -InstanceId $device.InstanceId -OtherInstanceIds $otherIds)
        if ($ours.Count) {
            if ($i -gt 1) { Write-Step "The sound output appeared after $i tries." }
            break
        }
        if ($i -lt $EndpointAttempts) { $null = & $Tools.Pause }
    }
    if (-not $ours.Count) {
        if ($result.Restart) {
            $result.ExitCode = 3010
            return $result
        }
        if (-not (& $Tools.AudioRunning)) {
            Write-Step "No sound output appeared, and Windows' sound services (Windows Audio, Windows Audio Endpoint Builder) are not running: none can appear until they run."
        }
        $result.Result = 'endpoint-missing'
        $result.ExitCode = 23
        return $result
    }
    foreach ($ep in $ours) {
        $changes = @(Get-EndpointChanges -Endpoint $ep -MakeVisible ($plan.Action -ne 'Keep'))
        Write-Step "Sound output $($ep.Guid) ($(Format-EndpointName $ep.FriendlyName)): $(if ($changes.Count) { 'setting ' + ($changes -join ', ') } else { 'nothing to change' })"
        if ($changes.Count) { $null = & $Tools.SetEndpoint $ep $changes $StsProductName }
    }
    if ($removing.Count -and $rolesBefore.Count) {
        # Removing a device took the roles its output held: the output that
        # stays gets every role a Stream To Speaker output held before.
        $held = @(& $Tools.DefaultRoles @($ours | ForEach-Object { $_.Id }))
        foreach ($role in @($rolesBefore | Where-Object { $held -notcontains $_ })) {
            $hr = & $Tools.SetDefault $ours[0].Id $role
            Write-Step ("  Windows plays to it again for role {0}: HRESULT 0x{1:X8}" -f $role, $hr)
        }
    }
    $after = @(Select-DeviceEndpoints -Endpoints @(& $Tools.GetEndpoints) -InstanceId $device.InstanceId -OtherInstanceIds $otherIds)
    $rolesAfter = @(if ($after.Count) { & $Tools.DefaultRoles @($after | ForEach-Object { $_.Id }) })
    $result.IsDefault = $rolesAfter.Count -gt 0
    $lost = @($rolesBefore | Where-Object { $rolesAfter -notcontains $_ })
    if ($lost.Count) { Write-Step "Windows no longer plays to Stream To Speaker for role(s) $($lost -join ', ')." }
    if (@($after | Where-Object { $_.State -eq $StsEndpointActive }).Count -eq 0) {
        $result.Result = 'endpoint-off'
        $result.ExitCode = 24
    }
    if ($result.Restart -and $result.ExitCode -eq 0) { $result.ExitCode = 3010 }
    $result
}

# The line the installer reads back from Install-Driver.ps1's output.
function Format-DriverResult($Result) {
    ('driver-step-result: {0} restart={1} replaced={2} new-output={3} problem={4} kept-before={5} action={6} ' +
        'extras-removed={7} extras-left={8} was-default={9} is-default={10} sound-moved={11}') -f
        $Result.Result, [int][bool]$Result.Restart, [int][bool]$Result.Replaced, [int][bool]$Result.NewOutput,
        [int]$Result.Problem, [int][bool]$Result.KeptBefore, $(if ($Result.Action) { $Result.Action } else { 'none' }),
        [int]$Result.ExtrasRemoved, [int]$Result.ExtrasLeft, [int][bool]$Result.WasDefault, [int][bool]$Result.IsDefault,
        [int][bool]$Result.SoundMoved
}
