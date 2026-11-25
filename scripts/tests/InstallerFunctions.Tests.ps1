# Tests of what the installer's scripts decide (scripts\InstallerFunctions.ps1).
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\tests\InstallerFunctions.Tests.ps1
#
# Runs in Windows PowerShell 5.1, the PowerShell Setup uses. The driver step
# is driven with made-up devices, packages and endpoints; the registry and
# file tests use their own keys under HKCU:\Software\StreamToSpeakerTests
# and a temporary folder, and remove them again. Nothing else on the
# machine is read or changed. Exits with the number of failed tests.
#
# Keep this file ASCII.

$ErrorActionPreference = 'Continue'
$here = Split-Path -Parent $PSScriptRoot
. ([ScriptBlock]::Create([IO.File]::ReadAllText((Join-Path $here 'InstallerFunctions.ps1'))))
$StsLogPrefix = '      | '

$script:Passed = 0
$script:Failed = @()
function Test([string]$Name, [scriptblock]$Body) {
    try {
        $null = & $Body
        $script:Passed++
        Write-Host "ok    $Name"
    } catch {
        $script:Failed += $Name
        Write-Host "FAIL  $Name"
        Write-Host "      $($_.Exception.Message)"
    }
}
function Assert-Equal($Expected, $Actual, [string]$What = 'value') {
    $e = @($Expected) -join ' | '
    $a = @($Actual) -join ' | '
    if ($e -cne $a) { throw "${What}: expected [$e], got [$a]" }
}
function Assert-True($Condition, [string]$What) {
    if (-not $Condition) { throw $What }
}

$TestKey = 'HKCU:\Software\StreamToSpeakerTests'
$TestDir = Join-Path ([IO.Path]::GetTempPath()) ('sts-installer-tests [' + [guid]::NewGuid().ToString('N').Substring(0, 8) + ']')
$null = [IO.Directory]::CreateDirectory($TestDir)
if (Test-Path -LiteralPath $TestKey) { Remove-Item -LiteralPath $TestKey -Recurse -Force }

# ---------------------------------------------------------------------------
# A made-up PC for Invoke-DriverInstall
# ---------------------------------------------------------------------------

$NewHashes = [ordered]@{ 'StreamToSpeaker.sys' = 'aaaa'; 'StreamToSpeaker.inf' = 'bbbb'; 'StreamToSpeaker.cat' = 'cccc' }
$OldHashes = [ordered]@{ 'StreamToSpeaker.sys' = '1111'; 'StreamToSpeaker.inf' = '2222'; 'StreamToSpeaker.cat' = '3333' }

function Fake-Device([string]$Id = 'ROOT\MEDIA\0000', [int]$Problem = 0, [string]$Inf = 'oem5.inf') {
    [pscustomobject]@{ InstanceId = $Id; ProblemCode = $Problem; InfName = $Inf }
}

function Fake-Endpoint {
    param([string]$Guid = '{11111111-1111-1111-1111-111111111111}', [int]$State = 1, [string]$Device = 'ROOT\MEDIA\0000',
          [string]$Name = 'Stream To Speaker', [string]$Association = $StsKsNodeTypeSpeaker, $FormFactor = 1, $Enable = 0x101)
    [pscustomobject]@{
        Guid = $Guid; Id = "{0.0.0.00000000}.$Guid"; KeyPath = "fake:$Guid"; State = $State
        FriendlyName = $Name; DeviceDesc = 'Speakers'; InterfaceName = 'Stream To Speaker'
        DeviceInstance = "{1}.$Device"; FormFactor = $FormFactor; Association = $Association; EnableByDefault = $Enable
    }
}

# The PC's state the fake tools read and change, and every change they were asked for.
function New-World {
    param(
        [object[]]$Devices = @(),
        [string]$BoundFolder = '',
        $BoundHashes = $null,
        [string[]]$Packages = @(),
        [object[]]$Endpoints = @(),
        [object[]]$EndpointsAfter = $null,
        [int]$StageCode = 0,
        [int]$UpdateCode = 0,
        [int]$CreateCode = 0,
        [int]$ProblemAfter = 0,
        [string]$NewInf = 'oem9.inf',
        [bool]$Running = $false,
        [bool]$Known = $true,
        [string]$LoadedHash = $null,
        [bool]$HasCer = $false,
        [string]$CurrentThumb = $null,
        [string]$PreviousThumb = $null,
        [string[]]$Recorded = @(),
        [int[]]$Roles = @()
    )
    $script:W = @{
        Calls = New-Object System.Collections.ArrayList
        Devices = @($Devices); BoundFolder = $BoundFolder; BoundHashes = $BoundHashes; Packages = @($Packages)
        Endpoints = @($Endpoints); EndpointsAfter = $EndpointsAfter; DeviceChanged = $false
        StageCode = $StageCode; UpdateCode = $UpdateCode; CreateCode = $CreateCode; ProblemAfter = $ProblemAfter; NewInf = $NewInf
        Running = $Running; Known = $Known; LoadedHash = $LoadedHash
        HasCer = $HasCer; CurrentThumb = $CurrentThumb; PreviousThumb = $PreviousThumb; Recorded = @($Recorded); Roles = @($Roles)
    }
}
function Note([string]$Call) { [void]$script:W.Calls.Add($Call) }
function Calls { @($script:W.Calls) }
function Called([string]$Prefix) { @($script:W.Calls | Where-Object { $_ -like "$Prefix*" }).Count }
function Index-Of([string]$Prefix) {
    for ($i = 0; $i -lt $script:W.Calls.Count; $i++) { if ($script:W.Calls[$i] -like "$Prefix*") { return $i } }
    -1
}

$FakeTools = @{
    FileExists       = { param($Path) $script:W.HasCer -and $Path -like '*StreamToSpeaker.cer' }
    HashFiles        = { param($Folder)
        if ($Folder -eq 'PKG') { return $NewHashes }
        if ($Folder -and $Folder -eq $script:W.BoundFolder) { return $script:W.BoundHashes }
        [ordered]@{ 'StreamToSpeaker.sys' = $null; 'StreamToSpeaker.inf' = $null; 'StreamToSpeaker.cat' = $null } }
    GetDevices       = { @($script:W.Devices) }
    GetBoundFolder   = { $script:W.BoundFolder }
    GetPackages      = { @($script:W.Packages) }
    GetEndpoints     = { if ($script:W.DeviceChanged -and $null -ne $script:W.EndpointsAfter) { @($script:W.EndpointsAfter) } else { @($script:W.Endpoints) } }
    GetDriverState   = { [pscustomobject]@{ Running = $script:W.Running; Known = $script:W.Known; Image = $(if ($script:W.Running) { 'C:\loaded\StreamToSpeaker.sys' } else { '' }) } }
    HashFile         = { param($Path) $script:W.LoadedHash }
    TrustCertificate = { param($Path) Note 'Trust' }
    CertificateOf    = { param($Path) if ($Path -like '*previous*') { $script:W.PreviousThumb } else { $script:W.CurrentThumb } }
    RecordedCerts    = { @($script:W.Recorded) }
    RecordCerts      = { param($List) $script:W.Recorded = @($List | Where-Object { $_ }); Note "Record:$(@($script:W.Recorded) -join ',')" }
    RemoveCert       = { param($T) Note "RemoveCert:$T"; 2 }
    Stage            = { param($Inf) Note 'Stage'; $script:W.StageCode }
    UpdateDevice     = { param($Inf) Note 'Update'
        if ($script:W.UpdateCode -eq 0) {
            $script:W.Devices = @($script:W.Devices | ForEach-Object { Fake-Device $_.InstanceId $script:W.ProblemAfter $script:W.NewInf })
            $script:W.DeviceChanged = $true
        }
        $script:W.UpdateCode }
    CreateDevice     = { param($Inf) Note 'Create'
        if ($script:W.CreateCode -eq 0) {
            $script:W.Devices = @($script:W.Devices) + (Fake-Device "ROOT\MEDIA\000$(@($script:W.Devices).Count)" $script:W.ProblemAfter $script:W.NewInf)
            $script:W.DeviceChanged = $true
        }
        $script:W.CreateCode }
    RemoveDevice     = { param($Id) Note "RemoveDevice:$Id"; $script:W.Devices = @($script:W.Devices | Where-Object { $_.InstanceId -ne $Id }); 0 }
    DeletePackage    = { param($Name) Note "DeletePackage:$Name"; $script:W.Packages = @($script:W.Packages | Where-Object { $_ -ne $Name }); 0 }
    RemoveEndpoints  = { param($List) Note "RemoveEndpoints:$(@($List).Count)"; [pscustomobject]@{ Removed = @($List).Count; Failed = @() } }
    RestartAudio     = { Note 'RestartAudio' }
    DefaultRoles     = { param($Ids) @($script:W.Roles) }
    SetDefault       = { param($Id, $Role) Note "SetDefault:$Role"; 0 }
    SetEndpoint      = { param($Ep, $Changes, $Name) Note "SetEndpoint:$(@($Changes) -join ',')"; if (@($Changes) -contains 'Visible') { $Ep.State = 1 } }
    Pause            = { }
}

function Install([string]$Previous = '') {
    Invoke-DriverInstall -PackageDir 'PKG' -PreviousCertificate $Previous -Tools $FakeTools -DeviceAttempts 3 -EndpointAttempts 3
}

# ---------------------------------------------------------------------------
# The driver step
# ---------------------------------------------------------------------------

Test 'a driver package Windows refuses changes nothing and keeps the working driver' {
    New-World -Devices @(Fake-Device) -BoundFolder 'OLD' -BoundHashes $OldHashes -Packages @('oem5.inf') `
        -Endpoints @(Fake-Endpoint) -StageCode 5
    $r = Install
    Assert-Equal 'stage-failed' $r.Result 'result'
    Assert-True $r.KeptBefore 'says the earlier driver is still in use'
    Assert-Equal 'Stage' (Calls) 'the only change attempted'
}

Test 'the new package is in the driver store before the old one is deleted' {
    New-World -Devices @(Fake-Device) -BoundFolder 'OLD' -BoundHashes $OldHashes -Packages @('oem5.inf', 'oem9.inf') `
        -Endpoints @(Fake-Endpoint)
    $r = Install
    Assert-Equal 'ok' $r.Result 'result'
    Assert-True ((Index-Of 'Stage') -lt (Index-Of 'Update')) 'staged before the device was updated'
    Assert-True ((Index-Of 'Update') -lt (Index-Of 'DeletePackage:oem5.inf')) 'old package deleted only after the update'
    Assert-Equal 0 (Called 'DeletePackage:oem9.inf') 'the new package stays'
}

Test 'a device update that fails deletes no package' {
    New-World -Devices @(Fake-Device) -BoundFolder 'OLD' -BoundHashes $OldHashes -Packages @('oem5.inf') `
        -Endpoints @(Fake-Endpoint) -UpdateCode 2
    $r = Install
    Assert-Equal 'device-failed' $r.Result 'result'
    Assert-Equal 0 (Called 'DeletePackage') 'packages deleted'
}

Test 'an update with the driver already installed leaves device, output and audio alone' {
    New-World -Devices @(Fake-Device) -BoundFolder 'OLD' -BoundHashes $NewHashes -Packages @('oem5.inf') `
        -Endpoints @(Fake-Endpoint)
    $r = Install
    Assert-Equal 'ok' $r.Result 'result'
    Assert-Equal 'Keep' $r.Action 'plan'
    Assert-Equal '' ((Calls) -join ',') 'changes made'
    Assert-True (-not $r.NewOutput) 'the output is the one Windows had'
    Assert-True (-not $r.Restart) 'no restart'
}

Test 'a changed driver goes onto the existing device, which keeps its output' {
    New-World -Devices @(Fake-Device) -BoundFolder 'OLD' -BoundHashes $OldHashes -Packages @('oem5.inf') `
        -Endpoints @(Fake-Endpoint)
    $r = Install
    Assert-Equal 'Update' $r.Action 'plan'
    foreach ($forbidden in 'Create', 'RemoveDevice', 'RemoveEndpoints', 'RestartAudio') {
        Assert-Equal 0 (Called $forbidden) "calls to $forbidden"
    }
    Assert-True (-not $r.NewOutput) 'the output is the one Windows had'
}

Test 'a device whose driver failed to load gets the driver again, on the same device' {
    New-World -Devices @(Fake-Device -Problem 39) -BoundFolder 'OLD' -BoundHashes $NewHashes -Packages @('oem5.inf') `
        -Endpoints @() -EndpointsAfter @(Fake-Endpoint) -NewInf 'oem5.inf'
    $r = Install
    Assert-Equal 'Update' $r.Action 'plan'
    Assert-Equal 0 (Called 'Create') 'devices created'
    Assert-Equal 'ok' $r.Result 'result'
}

Test 'a signature Windows refuses is reported, and a repair does not reinstall it' {
    New-World -Devices @(Fake-Device -Problem 52) -BoundFolder 'OLD' -BoundHashes $NewHashes -Packages @('oem5.inf')
    $r = Install
    Assert-Equal 'Keep' $r.Action 'plan'
    Assert-Equal 'driver-not-started' $r.Result 'result'
    Assert-Equal 52 $r.Problem 'problem code'
    Assert-Equal '' ((Calls) -join ',') 'changes made'
}

Test 'an early build''s cached output is rebuilt and gets its default roles back' {
    $legacy = Fake-Endpoint -Association '{DFF21BE2-F70F-11D0-B917-00A0C9223196}' -Name 'Internal AUX Jack (Stream To Speaker)'
    New-World -Devices @(Fake-Device) -BoundFolder 'OLD' -BoundHashes $OldHashes -Packages @('oem5.inf') `
        -Endpoints @($legacy) -EndpointsAfter @(Fake-Endpoint -Guid '{22222222-2222-2222-2222-222222222222}' -State 2 -Name 'Speakers (Stream To Speaker)') `
        -Roles @(0, 1)
    $r = Install
    Assert-Equal 'Rebuild' $r.Action 'plan'
    Assert-True ((Index-Of 'RemoveDevice') -lt (Index-Of 'RemoveEndpoints')) 'device removed before its cache'
    Assert-True ((Index-Of 'RemoveEndpoints') -lt (Index-Of 'RestartAudio')) 'audio restarted after the cache was cleared'
    Assert-True ((Index-Of 'RestartAudio') -lt (Index-Of 'Create')) 'device created after the restart'
    Assert-Equal 1 (Called 'SetDefault:0') 'console role restored'
    Assert-Equal 1 (Called 'SetDefault:1') 'multimedia role restored'
    Assert-Equal 0 (Called 'SetDefault:2') 'communications role (not held before)'
    Assert-Equal 'SetEndpoint:Visible,Name' ((Calls) | Where-Object { $_ -like 'SetEndpoint*' }) 'what was written to the new output'
}

Test 'a device that exists is never created again; extra devices are removed' {
    New-World -Devices @((Fake-Device 'ROOT\MEDIA\0001' 0), (Fake-Device 'ROOT\MEDIA\0000' 0)) -BoundFolder 'OLD' `
        -BoundHashes $NewHashes -Packages @('oem5.inf') -Endpoints @(Fake-Endpoint)
    $r = Install
    Assert-Equal 0 (Called 'Create') 'devices created'
    Assert-Equal 'RemoveDevice:ROOT\MEDIA\0001' ((Calls) | Where-Object { $_ -like 'RemoveDevice*' }) 'devices removed'
}

Test 'a first install creates the device and leaves Windows audio running' {
    New-World -EndpointsAfter @(Fake-Endpoint -State 2 -Name 'Speakers (Stream To Speaker)')
    $r = Install
    Assert-Equal 'Create' $r.Action 'plan'
    Assert-Equal 'Stage,Create,SetEndpoint:Visible,Name' ((Calls) -join ',') 'changes made'
    Assert-True $r.NewOutput 'a new output'
    Assert-Equal 'ok' $r.Result 'result'
}

Test 'an install after an uninstall clears the old output''s cached entry first' {
    New-World -Endpoints @(Fake-Endpoint -State 4) -EndpointsAfter @(Fake-Endpoint -Guid '{33333333-3333-3333-3333-333333333333}')
    $r = Install
    Assert-Equal 'Stage,RemoveEndpoints:1,RestartAudio,Create' (@(Calls)[0..3] -join ',') 'order of changes'
}

Test 'a sound output that is not there is not mistaken for the new one' {
    New-World -Devices @(Fake-Device) -BoundFolder 'OLD' -BoundHashes $OldHashes -Packages @('oem5.inf') `
        -Endpoints @(Fake-Endpoint -State 4) -EndpointsAfter @(Fake-Endpoint -State 4)
    $r = Install
    Assert-Equal 'endpoint-missing' $r.Result 'result'
    Assert-Equal 0 (Called 'SetEndpoint') 'writes to the absent output'
}

Test 'present endpoints: only active or turned off, only of our device' {
    $list = @(
        (Fake-Endpoint -Guid 'a' -State 1),
        (Fake-Endpoint -Guid 'b' -State 2),
        (Fake-Endpoint -Guid 'c' -State 4),
        (Fake-Endpoint -Guid 'd' -State 1 -Device 'ROOT\MEDIA\0007'))
    Assert-Equal 'a | b' (Select-PresentEndpoints -Endpoints $list -InstanceIds @('ROOT\MEDIA\0000') | ForEach-Object { $_.Guid }) 'present'
    Assert-Equal 'c' (Select-OrphanedEndpoints $list | ForEach-Object { $_.Guid }) 'orphaned'
}

Test 'the restart follows the loaded driver, not the file on disk' {
    $cases = @(
        @{ Running = $false; Known = $true;  Loaded = '';     Previous = '1111'; Want = $false },
        @{ Running = $true;  Known = $true;  Loaded = 'aaaa'; Previous = '1111'; Want = $false },
        @{ Running = $true;  Known = $true;  Loaded = '1111'; Previous = 'aaaa'; Want = $true },
        @{ Running = $true;  Known = $true;  Loaded = '';     Previous = 'aaaa'; Want = $true },
        @{ Running = $true;  Known = $false; Loaded = '';     Previous = 'aaaa'; Want = $false },
        @{ Running = $true;  Known = $false; Loaded = '';     Previous = '1111'; Want = $true })
    foreach ($c in $cases) {
        $d = Get-RestartDecision -Running $c.Running -LoadedKnown $c.Known -LoadedHash $c.Loaded -PreviousHash $c.Previous -NewHash 'aaaa'
        Assert-Equal $c.Want $d.Needed "restart for $($c | ConvertTo-Json -Compress)"
    }
    # A repair after a restart that was put off: the files match, the loaded image doesn't.
    New-World -Devices @(Fake-Device) -BoundFolder 'OLD' -BoundHashes $NewHashes -Packages @('oem5.inf') `
        -Endpoints @(Fake-Endpoint) -Running $true -LoadedHash '1111'
    Assert-True (Install).Restart 'restart asked again'
}

Test 'test certificates: the old one goes once its driver is gone, and stays while it is not' {
    New-World -Devices @(Fake-Device) -BoundFolder 'OLD' -BoundHashes $OldHashes -Packages @('oem5.inf') `
        -Endpoints @(Fake-Endpoint) -PreviousThumb 'OLDTHUMB'
    $r = Install -Previous 'C:\tmp\previous-test-cert.cer'
    Assert-Equal 'ok' $r.Result 'result'
    Assert-Equal 1 (Called 'RemoveCert:OLDTHUMB') 'old certificate removed'
    Assert-Equal 0 @($script:W.Recorded).Count 'certificates still recorded'

    New-World -Devices @(Fake-Device) -BoundFolder 'OLD' -BoundHashes $OldHashes -Packages @('oem5.inf') `
        -Endpoints @(Fake-Endpoint) -PreviousThumb 'OLDTHUMB' -StageCode 5
    $r = Install -Previous 'C:\tmp\previous-test-cert.cer'
    Assert-Equal 'stage-failed' $r.Result 'result'
    Assert-Equal 0 (Called 'RemoveCert') 'certificates removed'
    Assert-Equal 'OLDTHUMB' $script:W.Recorded 'recorded for the uninstaller'
}

Test 'a test build trusts its certificate before staging, records it and keeps it' {
    New-World -EndpointsAfter @(Fake-Endpoint) -HasCer $true -CurrentThumb 'NEWTHUMB'
    $r = Install
    Assert-True ((Index-Of 'Trust') -lt (Index-Of 'Stage')) 'trusted before staging'
    Assert-Equal 'NEWTHUMB' $script:W.Recorded 'recorded'
    Assert-Equal 0 (Called 'RemoveCert') 'certificates removed'
}

Test 'the result line carries what Setup reads back' {
    $r = New-DriverResult 'driver-not-started' 22
    $r.Problem = 52
    $r.KeptBefore = $true
    $r.Action = 'Update'
    Assert-Equal 'driver-step-result: driver-not-started restart=0 new-output=0 problem=52 kept-before=1 action=Update' (Format-DriverResult $r) 'line'
}

# ---------------------------------------------------------------------------
# Endpoints
# ---------------------------------------------------------------------------

Test 'our endpoints are found by any of their three names' {
    $render = "$TestKey\Render"
    $make = {
        param($Guid, $Values, $State)
        $key = "$render\$Guid"
        $null = New-Item -Path "$key\Properties" -Force
        $null = New-ItemProperty -LiteralPath $key -Name DeviceState -PropertyType DWord -Value $State -Force
        foreach ($n in $Values.Keys) { $null = New-ItemProperty -LiteralPath "$key\Properties" -Name $n -Value $Values[$n] -Force }
    }
    & $make '{aaaaaaaa-0000-0000-0000-000000000001}' @{ $StsPkey.FriendlyName = 'Living room'; $StsPkey.InterfaceName = 'Stream To Speaker'; $StsPkey.DeviceDesc = 'Speakers' } 1
    & $make '{aaaaaaaa-0000-0000-0000-000000000002}' @{ $StsPkey.FriendlyName = 'Speakers (Realtek(R) Audio)'; $StsPkey.InterfaceName = 'Realtek(R) Audio'; $StsPkey.DeviceDesc = 'Speakers' } 1
    & $make '{aaaaaaaa-0000-0000-0000-000000000003}' @{ $StsPkey.FriendlyName = "Stream To Speaker $([char]0x2192) Kitchen"; $StsPkey.DeviceInstance = '{1}.ROOT\MEDIA\0000' } 0x10000004
    $found = @(Get-OurEndpoints -RenderKey $render)
    Assert-Equal '{aaaaaaaa-0000-0000-0000-000000000001} | {aaaaaaaa-0000-0000-0000-000000000003}' ($found | Sort-Object Guid | ForEach-Object { $_.Guid }) 'found'
    Assert-Equal 4 ($found | Where-Object { $_.Guid -like '*3}' }).State 'state with the high flag bits masked'
    Remove-Item -LiteralPath $render -Recurse -Force
}

Test 'a cached entry Windows refuses to delete is reported, not counted' {
    $key = "$TestKey\Locked"
    $null = New-Item -Path "$key\Properties" -Force
    $me = [Security.Principal.WindowsIdentity]::GetCurrent().User
    $acl = Get-Acl -LiteralPath $key
    $deny = New-Object System.Security.AccessControl.RegistryAccessRule($me, 'Delete', 'Deny')
    $acl.AddAccessRule($deny)
    Set-Acl -LiteralPath $key -AclObject $acl
    try {
        $r = Remove-EndpointEntries @([pscustomobject]@{ Guid = 'Locked'; KeyPath = $key })
        Assert-Equal 0 $r.Removed 'removed'
        Assert-Equal 1 @($r.Failed).Count 'reported'
    } finally {
        $acl = Get-Acl -LiteralPath $key
        $null = $acl.RemoveAccessRule($deny)
        Set-Acl -LiteralPath $key -AclObject $acl
        Remove-Item -LiteralPath $key -Recurse -Force
    }
    $null = New-Item -Path "$TestKey\Free\Properties" -Force
    $r = Remove-EndpointEntries @([pscustomobject]@{ Guid = 'Free'; KeyPath = "$TestKey\Free" })
    Assert-Equal 1 $r.Removed 'removed'
    Assert-True (-not (Test-Path -LiteralPath "$TestKey\Free")) 'gone'
}

Test 'names Windows made up are replaced; ours and the person''s are kept' {
    foreach ($n in '', 'Speakers (Stream To Speaker)', 'Internal AUX Jack (Stream To Speaker)') {
        Assert-True (Test-ComposedEndpointName $n) "made up: '$n'"
    }
    foreach ($n in 'Stream To Speaker', "Stream To Speaker $([char]0x2192) Kitchen", 'Living room') {
        Assert-True (-not (Test-ComposedEndpointName $n)) "kept: '$n'"
    }
}

Test 'a healthy output needs no writes; a turned-off one is turned on only when asked' {
    Assert-Equal '' (@(Get-EndpointChanges -Endpoint (Fake-Endpoint) -MakeVisible $true) -join ',') 'healthy'
    Assert-Equal '' (@(Get-EndpointChanges -Endpoint (Fake-Endpoint -State 2 -Name 'Living room') -MakeVisible $false) -join ',') 'kept as the person left it'
    Assert-Equal 'Visible,FormFactor,Association,EnableByDefault,Name' (@(Get-EndpointChanges -Endpoint (Fake-Endpoint -State 2 -Name 'Speakers (Stream To Speaker)' -Association '' -FormFactor $null -Enable 0) -MakeVisible $true) -join ',') 'everything, in order'
    Assert-Equal 'Name' (@(Get-EndpointChanges -Endpoint (Fake-Endpoint -Name 'Living room') -MakeVisible $true -Name 'Stream To Speaker') -join ',') 'an explicit name'
    Assert-True (Test-LegacyEndpointCache (Fake-Endpoint -Association '{DFF21FE3-F70F-11D0-B917-00A0C9223196}')) 'line connector'
    Assert-True (-not (Test-LegacyEndpointCache (Fake-Endpoint))) 'current'
}

# ---------------------------------------------------------------------------
# Paths, packages, programs
# ---------------------------------------------------------------------------

Test 'arguments are quoted the way Windows reads them back' {
    Assert-Equal 'plain' (ConvertTo-CommandLineArgument 'plain') 'plain'
    Assert-Equal '"C:\Program Files\x.inf"' (ConvertTo-CommandLineArgument 'C:\Program Files\x.inf') 'spaces'
    Assert-Equal '"C:\a b\\"' (ConvertTo-CommandLineArgument 'C:\a b\') 'trailing backslash'
    Assert-Equal '"say \"hi\""' (ConvertTo-CommandLineArgument 'say "hi"') 'quotes'
    Assert-Equal '""' (ConvertTo-CommandLineArgument '') 'empty'
}

Test 'service and loaded-driver paths become normal paths' {
    Assert-Equal 'C:\Windows\System32\DriverStore\FileRepository\x\StreamToSpeaker.sys' (ConvertTo-WindowsPath '\SystemRoot\System32\DriverStore\FileRepository\x\StreamToSpeaker.sys' 'C:\Windows') 'SystemRoot'
    Assert-Equal 'C:\Windows\System32\drivers\a.sys' (ConvertTo-WindowsPath 'System32\drivers\a.sys' 'C:\Windows') 'relative'
    Assert-Equal 'D:\x\a.sys' (ConvertTo-WindowsPath '\??\D:\x\a.sys' 'C:\Windows') 'NT prefix'
}

Test 'driver packages and driver files are found in folders with [ and ] in their names' {
    $inf = Join-Path $TestDir 'INF'
    $null = [IO.Directory]::CreateDirectory($inf)
    Set-Content -LiteralPath (Join-Path $inf 'oem1.inf') -Value "[Models]`r`n%Dev% = Inst, Root\StreamToSpeaker"
    Set-Content -LiteralPath (Join-Path $inf 'oem2.inf') -Value '[Models] something else'
    Set-Content -LiteralPath (Join-Path $inf 'oem10.inf') -Value 'ROOT\STREAMTOSPEAKER'
    Assert-Equal 'oem1.inf | oem10.inf' (Get-OurDriverPackageNames $inf) 'packages'
    $pkg = Join-Path $TestDir 'driver'
    $null = [IO.Directory]::CreateDirectory($pkg)
    foreach ($f in $StsDriverFiles) { Set-Content -LiteralPath (Join-Path $pkg $f) -Value $f }
    $h = Get-DriverFileHashes $pkg
    Assert-True ($h['StreamToSpeaker.sys'] -and $h['StreamToSpeaker.cat']) 'hashed'
    Assert-True (Test-SameDriverFiles $h (Get-DriverFileHashes $pkg)) 'same files'
    Assert-True (-not (Test-SameDriverFiles $h (Get-DriverFileHashes (Join-Path $TestDir 'nothing')))) 'no files'
}

Test 'a program in a folder with [ and ] in its name is started' {
    $copy = Join-Path $TestDir 'where.exe'
    [IO.File]::Copy((Join-Path $env:windir 'System32\where.exe'), $copy, $true)
    Assert-Equal 0 (Invoke-Tool -FilePath $copy -Arguments @('cmd')) 'exit code'
    Assert-Equal -1 (Invoke-Tool -FilePath (Join-Path $TestDir 'missing.exe')) 'missing program'
}

# ---------------------------------------------------------------------------
# Firewall, sign-in, test signing
# ---------------------------------------------------------------------------

Test 'the firewall rule is for the program, inbound, on private and work networks' {
    $spec = Get-FirewallRuleSpec 'C:\Program Files\Stream To Speaker\stream-to-speaker.exe'
    Assert-Equal 'Inbound' $spec.Direction 'direction'
    Assert-Equal 'Allow' $spec.Action 'action'
    Assert-Equal 'Domain, Private' $spec.Profile 'networks'
    Assert-Equal 'C:\Program Files\Stream To Speaker\stream-to-speaker.exe' $spec.Program 'program'
    Assert-True (-not $spec.ContainsKey('RemoteAddress')) 'any address on those networks'
    Assert-True (Test-SameProgram '%ProgramFiles%\Stream To Speaker\stream-to-speaker.exe' (Join-Path $env:ProgramFiles 'Stream To Speaker\stream-to-speaker.exe')) 'environment variables'
    Assert-True (-not (Test-SameProgram 'C:\Other\stream-to-speaker.exe' 'C:\Program Files\Stream To Speaker\stream-to-speaker.exe')) 'another folder'
}

Test 'sign-in entries are removed for every account that starts this install, and only those' {
    $users = "$TestKey\Users"
    $run = 'Software\Microsoft\Windows\CurrentVersion\Run'
    $approved = 'Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run'
    $dir = 'C:\Program Files\Stream To Speaker'
    foreach ($u in 'S-1-5-21-1-1001', 'S-1-5-21-1-1002', 'S-1-5-21-1-1003') { $null = New-Item -Path "$users\$u\$run" -Force }
    $null = New-Item -Path "$users\S-1-5-21-1-1001\$approved" -Force
    $null = New-ItemProperty -LiteralPath "$users\S-1-5-21-1-1001\$run" -Name 'Stream To Speaker' -Value "`"$dir\stream-to-speaker.exe`" --startup"
    $null = New-ItemProperty -LiteralPath "$users\S-1-5-21-1-1001\$approved" -Name 'Stream To Speaker' -PropertyType Binary -Value ([byte[]](3, 0, 0, 0))
    $null = New-ItemProperty -LiteralPath "$users\S-1-5-21-1-1002\$run" -Name 'Stream To Speaker' -Value '"D:\Downloads\stream-to-speaker.exe" --startup'
    Assert-Equal 1 (Remove-SignInEntries -InstallDir $dir -UsersRoot $users) 'removed'
    Assert-True ($null -eq (Get-ItemProperty -LiteralPath "$users\S-1-5-21-1-1001\$run" -Name 'Stream To Speaker' -ErrorAction SilentlyContinue)) 'this install''s entry'
    Assert-True ($null -eq (Get-ItemProperty -LiteralPath "$users\S-1-5-21-1-1001\$approved" -Name 'Stream To Speaker' -ErrorAction SilentlyContinue)) 'its Task Manager state'
    Assert-True ($null -ne (Get-ItemProperty -LiteralPath "$users\S-1-5-21-1-1002\$run" -Name 'Stream To Speaker' -ErrorAction SilentlyContinue)) 'another copy''s entry stays'
    Assert-True (Test-RunValueInFolder 'C:\Apps\STS\stream-to-speaker.exe --startup' 'C:\Apps\STS') 'unquoted'
    Assert-True (-not (Test-RunValueInFolder '"C:\Program Files\Stream To Speaker Beta\stream-to-speaker.exe"' $dir)) 'a folder with a longer name'
}

Test 'test signing is read from the boot options, whatever the language' {
    Assert-True (Test-TestSigningOn ' NOEXECUTE=OPTIN  TESTSIGNING  NOVGA') 'on'
    Assert-True (-not (Test-TestSigningOn ' NOEXECUTE=OPTIN  FVEBOOT=2981888')) 'off'
    Assert-True (-not (Test-TestSigningOn '')) 'empty'
}

Test 'recorded test certificates are kept only as the installer recorded them' {
    $key = "$TestKey\Installer"
    Set-RecordedTestCertificates -Thumbprints @('0123456789ABCDEF0123456789ABCDEF01234567', 'nonsense') -Key $key
    Assert-Equal '0123456789ABCDEF0123456789ABCDEF01234567' (Get-RecordedTestCertificates $key) 'read back'
    Set-RecordedTestCertificates -Thumbprints @() -Key $key
    Assert-True (-not (Test-Path -LiteralPath $key)) 'cleared'
    Assert-Equal 'B | C' (Select-CertificatesToRemove -Candidates @('A', 'B', $null, 'C', 'B') -Keep 'A') 'to remove'
}

# ---------------------------------------------------------------------------
# Every script parses in this PowerShell
# ---------------------------------------------------------------------------

Test 'every installer script parses in Windows PowerShell and is ASCII' {
    $bad = @()
    $folders = @($here, (Join-Path $here 'tests'), (Join-Path (Split-Path -Parent $here) 'installer'))
    foreach ($f in @($folders | ForEach-Object { Get-ChildItem -LiteralPath $_ -Filter '*.ps1' -File })) {
        $tokens = $null
        $errors = $null
        $null = [System.Management.Automation.Language.Parser]::ParseFile($f.FullName, [ref]$tokens, [ref]$errors)
        foreach ($e in @($errors)) { $bad += "$($f.Name):$($e.Extent.StartLineNumber): $($e.Message)" }
        $bytes = [IO.File]::ReadAllBytes($f.FullName)
        if (@($bytes -gt 127).Count) { $bad += "$($f.Name): not ASCII" }
    }
    Assert-Equal '' ($bad -join '; ') 'parse errors'
}

Remove-Item -LiteralPath $TestKey -Recurse -Force -ErrorAction SilentlyContinue
Remove-Item -LiteralPath $TestDir -Recurse -Force -ErrorAction SilentlyContinue
Write-Host ''
Write-Host "$script:Passed passed, $($script:Failed.Count) failed"
foreach ($f in $script:Failed) { Write-Host "  failed: $f" }
exit $script:Failed.Count
