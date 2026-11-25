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

# -Roles: the default roles Windows gives this output when the test starts.
function Fake-Endpoint {
    param([string]$Guid = '{11111111-1111-1111-1111-111111111111}', [int]$State = 1, [string]$Device = 'ROOT\MEDIA\0000',
          [string]$Name = 'Stream To Speaker', [string]$Association = $StsKsNodeTypeSpeaker, $FormFactor = 1, $Enable = 0x101,
          [int[]]$Roles = @())
    [pscustomobject]@{
        Guid = $Guid; Id = "{0.0.0.00000000}.$Guid"; KeyPath = "fake:$Guid"; State = $State
        FriendlyName = $Name; DeviceDesc = 'Speakers'; InterfaceName = 'Stream To Speaker'
        DeviceInstance = "{1}.$Device"; FormFactor = $FormFactor; Association = $Association; EnableByDefault = $Enable
        Roles = @($Roles)
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
        [bool]$TestSigning = $false,
        [bool]$TrustFails = $false,
        [string[]]$StuckPackages = @(),
        [bool]$MoveResult = $true,
        [bool]$AudioRunning = $true
    )
    # Which output Windows plays to for each role (0, 1, 2).
    $roleOf = @{}
    foreach ($ep in @($Endpoints) + @($EndpointsAfter)) {
        if ($ep) { foreach ($r in @($ep.Roles)) { $roleOf[[int]$r] = $ep.Id } }
    }
    $script:W = @{
        Calls = New-Object System.Collections.ArrayList
        Devices = @($Devices); BoundFolder = $BoundFolder; BoundHashes = $BoundHashes; Packages = @($Packages)
        Endpoints = @($Endpoints); EndpointsAfter = $EndpointsAfter; DeviceChanged = $false
        StageCode = $StageCode; UpdateCode = $UpdateCode; CreateCode = $CreateCode; ProblemAfter = $ProblemAfter; NewInf = $NewInf
        Running = $Running; Known = $Known; LoadedHash = $LoadedHash
        HasCer = $HasCer; CurrentThumb = $CurrentThumb; PreviousThumb = $PreviousThumb; Recorded = @($Recorded)
        StartOptions = $(if ($TestSigning) { ' NOEXECUTE=OPTIN  TESTSIGNING' } else { ' NOEXECUTE=OPTIN' })
        TrustFails = $TrustFails; StuckPackages = @($StuckPackages); MoveResult = $MoveResult; RoleOf = $roleOf
        AudioRunning = $AudioRunning
    }
}
function Current-Endpoints {
    if ($script:W.DeviceChanged -and $null -ne $script:W.EndpointsAfter) { @($script:W.EndpointsAfter) } else { @($script:W.Endpoints) }
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
    StartOptions     = { $script:W.StartOptions }
    HashFiles        = { param($Folder)
        if ($Folder -eq 'PKG') { return $NewHashes }
        if ($Folder -and $Folder -eq $script:W.BoundFolder) { return $script:W.BoundHashes }
        [ordered]@{ 'StreamToSpeaker.sys' = $null; 'StreamToSpeaker.inf' = $null; 'StreamToSpeaker.cat' = $null } }
    GetDevices       = { @($script:W.Devices) }
    GetBoundFolder   = { $script:W.BoundFolder }
    GetPackages      = { @($script:W.Packages) }
    GetEndpoints     = { Current-Endpoints }
    GetDriverState   = { [pscustomobject]@{ Running = $script:W.Running; Known = $script:W.Known; Image = $(if ($script:W.Running) { 'C:\loaded\StreamToSpeaker.sys' } else { '' }) } }
    HashFile         = { param($Path) $script:W.LoadedHash }
    TrustCertificate = { param($Path) Note 'Trust'; if ($script:W.TrustFails) { throw 'TrustedPublisher refused it' } }
    CertificateOf    = { param($Path) if ($Path -like '*previous*') { $script:W.PreviousThumb } else { $script:W.CurrentThumb } }
    RecordedCerts    = { @($script:W.Recorded) }
    RecordCerts      = { param($List) $script:W.Recorded = @($List | Where-Object { $_ }); Note "Record:$(@($script:W.Recorded) -join ',')" }
    RemoveCert       = { param($T) Note "RemoveCert:$T"; 2 }
    Stage            = { param($Inf) Note 'Stage'
        if ($script:W.StageCode -eq 0 -or $script:W.StageCode -eq 3010) {
            if (@($script:W.Packages) -notcontains $script:W.NewInf) { $script:W.Packages = @($script:W.Packages) + $script:W.NewInf }
        }
        $script:W.StageCode }
    UpdateDevice     = { param($Inf) Note 'Update'
        if ($script:W.UpdateCode -eq 0 -or $script:W.UpdateCode -eq 1) {
            $script:W.Devices = @($script:W.Devices | ForEach-Object { Fake-Device $_.InstanceId $script:W.ProblemAfter $script:W.NewInf })
            $script:W.DeviceChanged = $true
        }
        $script:W.UpdateCode }
    # As DevCon's "install" does: the driver goes onto every device that is
    # there already, and onto the new one.
    CreateDevice     = { param($Inf) Note 'Create'
        if ($script:W.CreateCode -eq 0 -or $script:W.CreateCode -eq 1) {
            $script:W.Devices = @($script:W.Devices | ForEach-Object { Fake-Device $_.InstanceId $script:W.ProblemAfter $script:W.NewInf }) +
                (Fake-Device "ROOT\MEDIA\000$(@($script:W.Devices).Count)" $script:W.ProblemAfter $script:W.NewInf)
            $script:W.DeviceChanged = $true
        }
        $script:W.CreateCode }
    # A removed device's output stops being the default: Windows plays elsewhere.
    RemoveDevice     = { param($Id) Note "RemoveDevice:$Id"
        $script:W.Devices = @($script:W.Devices | Where-Object { $_.InstanceId -ne $Id })
        $gone = @(@($script:W.Endpoints) + @($script:W.EndpointsAfter) | Where-Object { $_ -and $_.DeviceInstance -like "*.$Id" } | ForEach-Object { $_.Id })
        foreach ($r in @($script:W.RoleOf.Keys)) { if ($gone -contains $script:W.RoleOf[$r]) { $script:W.RoleOf[$r] = 'elsewhere' } }
        0 }
    DeletePackage    = { param($Name) Note "DeletePackage:$Name"
        if (@($script:W.StuckPackages) -contains $Name) { return 5 }
        $script:W.Packages = @($script:W.Packages | Where-Object { $_ -ne $Name }); 0 }
    ForceDeletePackage = { param($Name) Note "ForceDeletePackage:$Name"
        if (@($script:W.StuckPackages) -contains $Name) { return 5 }
        $script:W.Packages = @($script:W.Packages | Where-Object { $_ -ne $Name }); 0 }
    RemoveEndpoints  = { param($List) Note "RemoveEndpoints:$(@($List).Count)"; [pscustomobject]@{ Removed = @($List).Count; Failed = @() } }
    RestartAudio     = { Note 'RestartAudio' }
    AudioRunning     = { $script:W.AudioRunning }
    DefaultRoles     = { param($Ids) @(@($script:W.RoleOf.Keys | Where-Object { @($Ids) -contains $script:W.RoleOf[$_] }) | Sort-Object) }
    SetDefault       = { param($Id, $Role) Note "SetDefault:$Role"; $script:W.RoleOf[[int]$Role] = $Id; 0 }
    SetEndpoint      = { param($Ep, $Changes, $Name) Note "SetEndpoint:$(@($Changes) -join ',')"; if (@($Changes) -contains 'Visible') { $Ep.State = 1 } }
    MoveDefaultAway  = { Note 'MoveDefaultAway'; $script:W.MoveResult }
    Pause            = { }
}

function Install([string]$Previous = '', [switch]$AllowTestBuild, [switch]$NoAppAfterwards) {
    Invoke-DriverInstall -PackageDir 'PKG' -PreviousCertificate $Previous -Tools $FakeTools -DeviceAttempts 3 -EndpointAttempts 3 `
        -AllowTestBuild:$AllowTestBuild -NoAppAfterwards:$NoAppAfterwards
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
    $legacy = Fake-Endpoint -Association '{DFF21BE2-F70F-11D0-B917-00A0C9223196}' -Name 'Internal AUX Jack (Stream To Speaker)' -Roles @(0, 1)
    New-World -Devices @(Fake-Device) -BoundFolder 'OLD' -BoundHashes $OldHashes -Packages @('oem5.inf') `
        -Endpoints @($legacy) -EndpointsAfter @(Fake-Endpoint -Guid '{22222222-2222-2222-2222-222222222222}' -State 2 -Name 'Speakers (Stream To Speaker)' -Device 'ROOT\MEDIA\0001')
    $r = Install
    Assert-Equal 'Rebuild' $r.Action 'plan'
    Assert-True ((Index-Of 'Create') -lt (Index-Of 'RemoveDevice:ROOT\MEDIA\0000')) 'the new device made before the old one goes'
    Assert-True ((Index-Of 'RemoveDevice') -lt (Index-Of 'RemoveEndpoints')) 'device removed before its cache'
    Assert-Equal 0 (Called 'RestartAudio') 'Windows audio restarts'
    Assert-Equal 1 (Called 'SetDefault:0') 'console role restored'
    Assert-Equal 1 (Called 'SetDefault:1') 'multimedia role restored'
    Assert-Equal 0 (Called 'SetDefault:2') 'communications role (not held before)'
    Assert-Equal 'SetEndpoint:Visible,Name' ((Calls) | Where-Object { $_ -like 'SetEndpoint*' }) 'what was written to the new output'
    Assert-True ($r.WasDefault -and $r.IsDefault) 'Windows played to it before and does again, as checked'
    Assert-Equal 0 $r.ExtrasRemoved 'the old device is part of the rebuild, not an extra one'
}

Test 'a rebuild that fails removes only the new device, and says what became of the old one as read back' {
    $legacy = Fake-Endpoint -Association '{DFF21BE2-F70F-11D0-B917-00A0C9223196}' -Roles @(0)
    New-World -Devices @(Fake-Device) -BoundFolder 'OLD' -BoundHashes $OldHashes -Packages @('oem5.inf') -Endpoints @($legacy) -CreateCode 2
    $r = Install
    Assert-Equal 'device-failed' $r.Result 'result when the device can''t be made'
    Assert-True $r.KeptBefore 'the old device is as it was: same driver package, still working'
    Assert-Equal 0 (Called 'RemoveDevice') 'devices removed'
    Assert-Equal 0 (Called 'RemoveEndpoints') 'cached entries removed'

    # Creating the new device put the new driver on the old one too, and it
    # doesn't start on either.
    New-World -Devices @(Fake-Device) -BoundFolder 'OLD' -BoundHashes $OldHashes -Packages @('oem5.inf') -Endpoints @($legacy) -ProblemAfter 39
    $r = Install
    Assert-Equal 'device-failed' $r.Result 'result when the new device doesn''t work'
    Assert-Equal 'RemoveDevice:ROOT\MEDIA\0001' ((Calls) | Where-Object { $_ -like 'RemoveDevice*' }) 'only the new device removed'
    Assert-Equal 'ROOT\MEDIA\0000' (@($script:W.Devices) | ForEach-Object { $_.InstanceId }) 'the old device stays'
    Assert-True (-not $r.KeptBefore) 'doesn''t say the old device kept its driver: the new one went onto it'
    Assert-Equal 0 (Called 'RemoveEndpoints') 'cached entries removed'
}

Test 'a device counts as it was before only with the same driver package and in the same condition' {
    $before = Fake-Device 'ROOT\MEDIA\0000' 0 'oem5.inf'
    Assert-True (Test-DeviceAsBefore -Before $before -Now @((Fake-Device 'ROOT\MEDIA\0001' 0 'oem9.inf'), (Fake-Device 'ROOT\MEDIA\0000' 0 'OEM5.INF'))) 'unchanged'
    Assert-True (-not (Test-DeviceAsBefore -Before $before -Now @(Fake-Device 'ROOT\MEDIA\0000' 0 'oem9.inf'))) 'another driver package'
    Assert-True (-not (Test-DeviceAsBefore -Before $before -Now @(Fake-Device 'ROOT\MEDIA\0000' 10 'oem5.inf'))) 'no longer working'
    Assert-True (-not (Test-DeviceAsBefore -Before $before -Now @(Fake-Device 'ROOT\MEDIA\0001' 0 'oem5.inf'))) 'gone'
    Assert-True (-not (Test-DeviceAsBefore -Before $null -Now @($before))) 'there was none'
    Assert-True (-not (Test-DeviceAsBefore -Before (Fake-Device 'ROOT\MEDIA\0000' 0 '') -Now @(Fake-Device 'ROOT\MEDIA\0000' 0 ''))) 'Windows doesn''t say which package'
}

Test 'a working device gets a new device and output when Windows has no Stream To Speaker output at all' {
    New-World -Devices @(Fake-Device) -BoundFolder 'OLD' -BoundHashes $NewHashes -Packages @('oem5.inf') `
        -Endpoints @() -EndpointsAfter @(Fake-Endpoint -Guid '{44444444-4444-4444-4444-444444444444}' -Device 'ROOT\MEDIA\0001')
    $r = Install
    Assert-Equal 'Rebuild' $r.Action 'plan'
    Assert-Equal 'ok' $r.Result 'result'
    Assert-True $r.NewOutput 'a new output'
    Assert-Equal 'ROOT\MEDIA\0001' (@($script:W.Devices) | ForEach-Object { $_.InstanceId }) 'one device, the new one'

    # An output that is only "not present" is no output either.
    New-World -Devices @(Fake-Device) -BoundFolder 'OLD' -BoundHashes $NewHashes -Packages @('oem5.inf') `
        -Endpoints @(Fake-Endpoint -State 4) -EndpointsAfter @(Fake-Endpoint -Guid '{44444444-4444-4444-4444-444444444444}' -Device 'ROOT\MEDIA\0001')
    Assert-Equal 'Rebuild' (Install).Action 'plan with a cached entry of an output that is gone'
}

Test 'a working device is not rebuilt because of what Windows records as its output''s device' {
    # The record is in another form than expected, or names a device that is gone.
    foreach ($recorded in 'SWD\MMDEVAPI\{0.0.0.00000000}.{77777777-7777-7777-7777-777777777777}', 'ROOT\MEDIA\0007', '') {
        $ep = Fake-Endpoint -Guid '{77777777-7777-7777-7777-777777777777}' -Roles @(0, 1)
        $ep.DeviceInstance = $recorded
        New-World -Devices @(Fake-Device) -BoundFolder 'OLD' -BoundHashes $NewHashes -Packages @('oem5.inf') -Endpoints @($ep)
        $r = Install
        Assert-Equal 'Keep' $r.Action "plan when the record says '$recorded'"
        Assert-Equal 'ok' $r.Result "result when the record says '$recorded'"
        Assert-Equal '' ((Calls) -join ',') "changes made when the record says '$recorded'"
        Assert-True ($r.WasDefault -and $r.IsDefault) "Windows still plays to it when the record says '$recorded'"
    }
    # A changed driver goes onto the device in place, as with the expected record.
    $ep = Fake-Endpoint -Guid '{77777777-7777-7777-7777-777777777777}'
    $ep.DeviceInstance = 'something else'
    New-World -Devices @(Fake-Device) -BoundFolder 'OLD' -BoundHashes $OldHashes -Packages @('oem5.inf') -Endpoints @($ep)
    $r = Install
    Assert-Equal 'Update' $r.Action 'plan with a changed driver'
    Assert-Equal 'ok' $r.Result 'result with a changed driver'
    Assert-Equal 0 ((Called 'Create') + (Called 'RemoveDevice')) 'devices created or removed'
}

Test 'while Windows'' sound services don''t run, a missing output changes nothing' {
    New-World -Devices @(Fake-Device) -BoundFolder 'OLD' -BoundHashes $NewHashes -Packages @('oem5.inf') -Endpoints @() -AudioRunning $false
    $r = Install
    Assert-Equal 'Keep' $r.Action 'plan'
    Assert-Equal 'endpoint-missing' $r.Result 'result'
    Assert-Equal '' ((Calls) -join ',') 'changes made'
    Assert-Equal 'ROOT\MEDIA\0000' (@($script:W.Devices) | ForEach-Object { $_.InstanceId }) 'the device is the one it was'
    Assert-Equal 'Rebuild' (Get-DriverPlan -Devices @(Fake-Device) -BoundFilesAreNew $true -OutputMissing $true).Action 'the plan when they run and no output is there'
    Assert-Equal 'Keep' (Get-DriverPlan -Devices @(Fake-Device) -BoundFilesAreNew $true -OutputMissing $false).Action 'the plan otherwise'
    Assert-Equal 'Keep' (Get-DriverPlan -Devices @(Fake-Device -Problem 52) -BoundFilesAreNew $true -OutputMissing $true).Action 'a device that doesn''t work has no output to miss'
}

Test 'with two devices, the one Windows plays to is kept, and the page is told one was removed' {
    New-World -Devices @((Fake-Device 'ROOT\MEDIA\0000' 0), (Fake-Device 'ROOT\MEDIA\0001' 0)) -BoundFolder 'OLD' -BoundHashes $NewHashes `
        -Packages @('oem5.inf') -Endpoints @(
            (Fake-Endpoint -Guid '{55555555-0000-0000-0000-000000000000}' -Device 'ROOT\MEDIA\0000'),
            (Fake-Endpoint -Guid '{55555555-0000-0000-0000-000000000001}' -Device 'ROOT\MEDIA\0001' -Roles @(0, 1, 2)))
    $r = Install
    Assert-Equal 'RemoveDevice:ROOT\MEDIA\0000' ((Calls) | Where-Object { $_ -like 'RemoveDevice*' }) 'the other device removed'
    Assert-Equal 1 $r.ExtrasRemoved 'extras removed'
    Assert-Equal 0 $r.ExtrasLeft 'extras left'
    Assert-True ($r.WasDefault -and $r.IsDefault) 'Windows still plays to it, as checked'
}

Test 'with two devices, the roles of the one removed go to the one kept' {
    New-World -Devices @((Fake-Device 'ROOT\MEDIA\0000' 0), (Fake-Device 'ROOT\MEDIA\0001' 0)) -BoundFolder 'OLD' -BoundHashes $NewHashes `
        -Packages @('oem5.inf') -Endpoints @(
            (Fake-Endpoint -Guid '{66666666-0000-0000-0000-000000000000}' -Device 'ROOT\MEDIA\0000' -Roles @(0, 1)),
            (Fake-Endpoint -Guid '{66666666-0000-0000-0000-000000000001}' -Device 'ROOT\MEDIA\0001' -Roles @(2)))
    $r = Install
    Assert-Equal 'RemoveDevice:ROOT\MEDIA\0001' ((Calls) | Where-Object { $_ -like 'RemoveDevice*' }) 'the device with fewer roles removed'
    Assert-Equal 1 (Called 'SetDefault:2') 'its role given to the kept output'
    Assert-Equal '0 | 1 | 2' (& $FakeTools.DefaultRoles @('{0.0.0.00000000}.{66666666-0000-0000-0000-000000000000}')) 'roles now'
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
    New-World -Devices @(Fake-Device -Problem 31) -BoundFolder 'OLD' -BoundHashes $OldHashes -Packages @('oem5.inf') `
        -Endpoints @(Fake-Endpoint -State 4) -EndpointsAfter @(Fake-Endpoint -State 4)
    $r = Install
    Assert-Equal 'Update' $r.Action 'plan'
    Assert-Equal 'endpoint-missing' $r.Result 'result'
    Assert-Equal 0 (Called 'SetEndpoint') 'writes to the absent output'
}

Test 'present endpoints: only active or turned off; a device''s: all but another known device''s' {
    $unrecorded = Fake-Endpoint -Guid 'e' -State 1
    $unrecorded.DeviceInstance = $null
    $list = @(
        (Fake-Endpoint -Guid 'a' -State 1),
        (Fake-Endpoint -Guid 'b' -State 2),
        (Fake-Endpoint -Guid 'c' -State 4),
        (Fake-Endpoint -Guid 'd' -State 1 -Device 'ROOT\MEDIA\0007'),
        $unrecorded)
    Assert-Equal 'a | b | d | e' (Select-PresentEndpoints $list | ForEach-Object { $_.Guid }) 'present'
    Assert-Equal 'c' (Select-OrphanedEndpoints $list | ForEach-Object { $_.Guid }) 'orphaned'
    Assert-Equal 'a | b | e' (Select-DeviceEndpoints -Endpoints $list -InstanceId 'ROOT\MEDIA\0000' -OtherInstanceIds @('ROOT\MEDIA\0000', 'ROOT\MEDIA\0007') | ForEach-Object { $_.Guid }) 'of the first device'
    Assert-Equal 'd | e' (Select-DeviceEndpoints -Endpoints $list -InstanceId 'ROOT\MEDIA\0007' -OtherInstanceIds @('ROOT\MEDIA\0000') | ForEach-Object { $_.Guid }) 'of the second'
    Assert-Equal 'a | b | d | e' (Select-DeviceEndpoints -Endpoints $list -InstanceId 'ROOT\MEDIA\0000' | ForEach-Object { $_.Guid }) 'with no other device known'
    Assert-True (Test-EndpointOfDevice $list[3] 'root\media\0007') 'the record names the device, in any case'
    Assert-True (-not (Test-EndpointOfDevice $unrecorded 'ROOT\MEDIA\0000')) 'no record names no device'
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

Test 'a test build records its certificate, then trusts it before staging, and keeps it' {
    New-World -EndpointsAfter @(Fake-Endpoint) -HasCer $true -CurrentThumb 'NEWTHUMB' -TestSigning $true
    $r = Install
    Assert-Equal 'ok' $r.Result 'result'
    Assert-True ((Index-Of 'Record:NEWTHUMB') -lt (Index-Of 'Trust')) 'recorded before it was trusted'
    Assert-True ((Index-Of 'Trust') -lt (Index-Of 'Stage')) 'trusted before staging'
    Assert-Equal 'NEWTHUMB' $script:W.Recorded 'recorded'
    Assert-Equal 0 (Called 'RemoveCert') 'certificates removed'
}

Test 'a test build on a PC without test signing changes nothing' {
    New-World -Devices @(Fake-Device) -BoundFolder 'OLD' -BoundHashes $OldHashes -Packages @('oem5.inf') `
        -Endpoints @(Fake-Endpoint -Roles @(0)) -HasCer $true -CurrentThumb 'NEWTHUMB' -PreviousThumb 'OLDTHUMB'
    $r = Install -Previous 'C:\tmp\previous-test-cert.cer' -NoAppAfterwards
    Assert-Equal 'test-signing-off' $r.Result 'result'
    Assert-Equal 25 $r.ExitCode 'exit code'
    Assert-Equal '' ((Calls) -join ',') 'changes made'

    New-World -Devices @(Fake-Device) -BoundFolder 'OLD' -BoundHashes $OldHashes -Packages @('oem5.inf') `
        -Endpoints @(Fake-Endpoint) -HasCer $true -CurrentThumb 'NEWTHUMB'
    $r = Install -AllowTestBuild
    Assert-Equal 'Update' $r.Action 'a developer can still install it (-AllowTestBuild)'
}

Test 'a release keeps the working driver''s package until the new driver runs' {
    # A different driver is loaded: until Windows restarts, the device runs on
    # the earlier package, so it stays, and so does the certificate it needs.
    New-World -Devices @(Fake-Device) -BoundFolder 'OLD' -BoundHashes $OldHashes -Packages @('oem5.inf') `
        -Endpoints @(Fake-Endpoint) -Running $true -LoadedHash '1111' -PreviousThumb 'OLDTHUMB'
    $r = Install -Previous 'C:\tmp\previous-test-cert.cer'
    Assert-Equal 'ok' $r.Result 'result'
    Assert-True ($r.Restart -and $r.Replaced) 'restart, with the earlier driver running until then'
    Assert-Equal 3010 $r.ExitCode 'exit code'
    Assert-Equal 0 (Called 'DeletePackage') 'packages deleted'
    Assert-Equal 0 (Called 'RemoveCert') 'certificates removed'
    Assert-Equal 'OLDTHUMB' $script:W.Recorded 'still recorded for later'
}

Test 'a restart Windows asks for is a restart' {
    New-World -Devices @(Fake-Device) -BoundFolder 'OLD' -BoundHashes $OldHashes -Packages @('oem5.inf') -Endpoints @(Fake-Endpoint) -UpdateCode 1
    $r = Install
    Assert-Equal 'ok' $r.Result 'result after devcon''s "restart needed"'
    Assert-True $r.Restart 'restart'
    Assert-True (-not $r.Replaced) 'not because another driver is loaded'
    New-World -Devices @(Fake-Device) -BoundFolder 'OLD' -BoundHashes $OldHashes -Packages @('oem5.inf') -Endpoints @(Fake-Endpoint) -StageCode 3010
    Assert-True (Install).Restart 'restart after pnputil''s 3010'
}

Test 'a test certificate that couldn''t be trusted is removed again, and a staged package that couldn''t be used too' {
    New-World -HasCer $true -CurrentThumb 'NEWTHUMB' -TestSigning $true -TrustFails $true
    $r = Install
    Assert-Equal 'trust-failed' $r.Result 'result'
    Assert-Equal 1 (Called 'RemoveCert:NEWTHUMB') 'removed from the stores it reached'
    Assert-Equal 0 @($script:W.Recorded).Count 'nothing left recorded'
    Assert-Equal 0 (Called 'Stage') 'staged'

    New-World -EndpointsAfter @(Fake-Endpoint) -HasCer $true -CurrentThumb 'NEWTHUMB' -TestSigning $true -CreateCode 2
    $r = Install
    Assert-Equal 'device-failed' $r.Result 'result'
    Assert-Equal 1 (Called 'DeletePackage:oem9.inf') 'the package it staged deleted'
    Assert-Equal 1 (Called 'RemoveCert:NEWTHUMB') 'its certificate removed'
    Assert-Equal 0 @($script:W.Recorded).Count 'nothing left recorded'

    New-World -EndpointsAfter @(Fake-Endpoint) -HasCer $true -CurrentThumb 'NEWTHUMB' -TestSigning $true -CreateCode 2 -StuckPackages @('oem9.inf')
    $r = Install
    Assert-Equal 0 (Called 'RemoveCert') 'kept while its package is still in the store'
    Assert-Equal 'NEWTHUMB' $script:W.Recorded 'and recorded for the uninstaller'
}

Test 'an earlier build''s certificate is recorded before anything else' {
    New-World -Devices @(Fake-Device) -BoundFolder 'OLD' -BoundHashes $OldHashes -Packages @('oem5.inf') `
        -Endpoints @(Fake-Endpoint) -PreviousThumb 'OLDTHUMB' -StageCode 5
    $null = Install -Previous 'C:\tmp\previous-test-cert.cer'
    Assert-Equal 'Record:OLDTHUMB' (@(Calls)[0]) 'first change'
}

Test 'when no copy of the app runs afterwards, Windows is moved off its output' {
    New-World -Devices @(Fake-Device) -BoundFolder 'OLD' -BoundHashes $NewHashes -Packages @('oem5.inf') -Endpoints @(Fake-Endpoint -Roles @(0, 1))
    $r = Install -NoAppAfterwards
    Assert-Equal 1 (Called 'MoveDefaultAway') 'moved'
    Assert-True ($r.SoundMoved -and -not $r.IsDefault) 'says so'
    New-World -Devices @(Fake-Device) -BoundFolder 'OLD' -BoundHashes $NewHashes -Packages @('oem5.inf') -Endpoints @(Fake-Endpoint -Roles @(0, 1))
    $r = Install
    Assert-Equal 0 (Called 'MoveDefaultAway') 'not moved when the app runs again'
    Assert-True ($r.WasDefault -and $r.IsDefault -and -not $r.SoundMoved) 'still the default'
}

Test 'the result line carries what Setup reads back' {
    $r = New-DriverResult 'driver-not-started' 22
    $r.Problem = 52
    $r.KeptBefore = $true
    $r.Action = 'Update'
    $r.ExtrasRemoved = 1
    $r.WasDefault = $true
    Assert-Equal ('driver-step-result: driver-not-started restart=0 replaced=0 new-output=0 problem=52 kept-before=1 action=Update ' +
        'extras-removed=1 extras-left=0 was-default=1 is-default=0 sound-moved=0') (Format-DriverResult $r) 'line'
}

Test 'the uninstall says what it couldn''t remove' {
    New-World -Devices @(Fake-Device) -Packages @('oem5.inf', 'oem9.inf') -Endpoints @(Fake-Endpoint) -StuckPackages @('oem9.inf')
    $left = @(Invoke-DriverRemoval -Tools $FakeTools)
    Assert-Equal "left: the driver package oem9.inf in Windows' driver store" $left 'left'
    Assert-Equal 0 @($script:W.Devices).Count 'devices'
    New-World -Devices @(Fake-Device) -Packages @('oem5.inf') -Endpoints @(Fake-Endpoint -State 4)
    Assert-Equal '' (@(Invoke-DriverRemoval -Tools $FakeTools) -join ',') 'nothing left'
    Assert-Equal 1 (Called 'RemoveEndpoints:1') 'cached entries removed once no device is left'
    # A cached entry Windows keeps is no failure: it does nothing.
    New-World -Devices @(Fake-Device) -Packages @('oem5.inf') -Endpoints @(Fake-Endpoint -State 4)
    $refusing = $FakeTools.Clone()
    $refusing.RemoveEndpoints = { param($List) [pscustomobject]@{ Removed = 0; Failed = @($List) } }
    Assert-Equal '' (@(Invoke-DriverRemoval -Tools $refusing) -join ',') 'a kept entry is not named'
}

Test 'a device Windows removes only when it restarts is said so, and its output is left alone' {
    $other = '{22222222-2222-2222-2222-222222222222}'
    New-World -Devices @(Fake-Device) -Packages @('oem5.inf') -Endpoints @((Fake-Endpoint), (Fake-Endpoint -Guid $other -State 4))
    $waiting = $FakeTools.Clone()
    # As DevCon answers for a device whose driver waits for a restart: exit
    # code 1, and Windows no longer lists the device.
    $waiting.RemoveDevice = { param($Id) Note "RemoveDevice:$Id"; $script:W.Devices = @(); 1 }
    $lines = @(Invoke-DriverRemoval -Tools $waiting)
    Assert-Equal 'removed-at-restart: the Stream To Speaker device ROOT\MEDIA\0000' $lines 'lines'
    Assert-True ((Index-Of 'MoveDefaultAway') -ge 0 -and (Index-Of 'MoveDefaultAway') -lt (Index-Of 'RemoveDevice')) 'Windows is moved off the output before the device goes'
    Assert-Equal 1 (Called 'RemoveEndpoints') 'one delete'
    Assert-Equal 1 (Called 'RemoveEndpoints:1') 'only the entry whose device is gone'
    # The uninstaller runs the removal a second time. That run finds no
    # device, and must not delete the entry of the output that is still there.
    New-World -Endpoints @((Fake-Endpoint), (Fake-Endpoint -Guid $other -State 4))
    Assert-Equal '' (@(Invoke-DriverRemoval -Tools $FakeTools) -join ',') 'nothing to say'
    Assert-Equal 1 (Called 'RemoveEndpoints') 'one delete again'
    Assert-Equal 1 (Called 'RemoveEndpoints:1') 'again only the entry whose device is gone'
}

Test 'a run that removed a device waits for its output to go, and a run that removed nothing does not wait' {
    New-World -Devices @(Fake-Device) -Packages @('oem5.inf') -Endpoints @(Fake-Endpoint)
    $settling = $FakeTools.Clone()
    # Here the output shows as not present only after a moment.
    $settling.Pause = { Note 'Pause'; foreach ($e in @($script:W.Endpoints)) { $e.State = 4 } }
    Assert-Equal '' (@(Invoke-DriverRemoval -Tools $settling) -join ',') 'nothing left'
    Assert-Equal 1 (Called 'Pause') 'one wait'
    Assert-Equal 1 (Called 'RemoveEndpoints:1') 'the entry is deleted once its output is gone'
    # A second run finds no device: waiting would change nothing.
    New-World -Endpoints @(Fake-Endpoint)
    Assert-Equal '' (@(Invoke-DriverRemoval -Tools $settling) -join ',') 'nothing to say'
    Assert-Equal 0 (Called 'Pause') 'no wait'
    Assert-Equal 0 (Called 'RemoveEndpoints') 'the entry of an output that is still there is left alone'
}

Test 'a device that could not be removed is named, and its output is not waited for' {
    New-World -Devices @(Fake-Device) -Packages @('oem5.inf') -Endpoints @(Fake-Endpoint)
    $failing = $FakeTools.Clone()
    # As DevCon answers when it could not remove the device: exit code 2,
    # and Windows still lists it.
    $failing.RemoveDevice = { param($Id) Note "RemoveDevice:$Id"; 2 }
    $failing.Pause = { Note 'Pause' }
    $lines = @(Invoke-DriverRemoval -Tools $failing)
    Assert-Equal 'left: the Stream To Speaker device ROOT\MEDIA\0000' $lines 'lines'
    Assert-Equal 0 (Called 'Pause') 'no wait'
    Assert-Equal 0 (Called 'RemoveEndpoints') 'the entry of its output is left alone'
}

Test 'the log names the app''s outputs, and no name a person chose' {
    Assert-Equal "'Stream To Speaker'" (Format-EndpointName 'Stream To Speaker') 'ours'
    Assert-Equal "'Stream To Speaker $([char]0x2192) Kitchen'" (Format-EndpointName "Stream To Speaker $([char]0x2192) Kitchen") 'the app''s'
    Assert-Equal '(a name chosen in Sound settings)' (Format-EndpointName 'Jane''s speakers') 'chosen'
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
    $deny = New-Object System.Security.AccessControl.RegistryAccessRule($me, 'Delete', 'Deny')
    # The rule is set and lifted through .NET: Windows PowerShell's Get-Acl
    # doesn't find a registry key that is given with -LiteralPath.
    $setRule = {
        param([bool]$Add)
        $rights = [System.Security.AccessControl.RegistryRights]'ReadPermissions, ChangePermissions'
        $k = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Software\StreamToSpeakerTests\Locked',
            [Microsoft.Win32.RegistryKeyPermissionCheck]::ReadWriteSubTree, $rights)
        try {
            $acl = $k.GetAccessControl()
            if ($Add) { $acl.AddAccessRule($deny) } else { $null = $acl.RemoveAccessRule($deny) }
            $k.SetAccessControl($acl)
        } finally {
            $k.Close()
        }
    }
    & $setRule $true
    try {
        $r = Remove-EndpointEntries @([pscustomobject]@{ Guid = 'Locked'; KeyPath = $key })
        Assert-Equal 0 $r.Removed 'removed'
        Assert-Equal 1 @($r.Failed).Count 'reported'
    } finally {
        & $setRule $false
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

Test 'a firewall rule Windows refuses to remove is found again afterwards' {
    $app = 'C:\Program Files\Stream To Speaker\stream-to-speaker.exe'
    $script:Rules = @(
        [pscustomobject]@{ Name = $StsFirewallRuleName; DisplayName = 'Stream To Speaker'; Action = 'Allow'; Direction = 'Inbound'; Program = $app; Locked = $false },
        [pscustomobject]@{ Name = 'Asked'; DisplayName = 'stream-to-speaker.exe'; Action = 'Block'; Direction = 'Inbound'; Program = $app; Locked = $true },
        [pscustomobject]@{ Name = 'Other'; DisplayName = 'Another program'; Action = 'Allow'; Direction = 'Inbound'; Program = 'C:\Other\other.exe'; Locked = $false })
    # Stand-ins for Windows' firewall commands, over the list above.
    function Get-NetFirewallRule {
        [CmdletBinding()]
        param([string]$Name, [Parameter(ValueFromPipeline = $true)]$Filter)
        process {
            $wanted = if ($Filter) { $Filter.Name } else { $Name }
            @($script:Rules | Where-Object { $_.Name -eq $wanted })
        }
    }
    function Get-NetFirewallApplicationFilter {
        [CmdletBinding()]
        param()
        $script:Rules
    }
    function Remove-NetFirewallRule {
        [CmdletBinding()]
        param([Parameter(ValueFromPipeline = $true)]$Rule)
        process {
            if ($Rule.Locked) { throw 'Access is denied.' }
            $script:Rules = @($script:Rules | Where-Object { $_.Name -ne $Rule.Name })
        }
    }
    Assert-Equal 2 @(Get-AppFirewallRules $app).Count 'Setup''s rule and the one Windows'' question made'
    Assert-Equal 'Other' (@(Get-AppFirewallRules 'C:\Other\other.exe' -OnlyForProgram) | ForEach-Object { $_.Name }) 'only a program''s own rules when asked so'
    Assert-Equal 1 (Remove-AppFirewallRules $app) 'removed'
    Assert-Equal 'Asked' (@(Get-AppFirewallRules $app) | ForEach-Object { $_.Name }) 'the refused rule is found again'
    Assert-Equal 'Asked | Other' ($script:Rules | ForEach-Object { $_.Name }) 'another program''s rule is not touched'
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
    Assert-Equal 'S-1-5-21-1-1001' (Get-SignInAccounts -InstallDir $dir -UsersRoot $users) 'the accounts that start this install'
    Assert-Equal 1 (Remove-SignInEntries -InstallDir $dir -UsersRoot $users) 'removed'
    Assert-Equal '' (@(Get-SignInAccounts -InstallDir $dir -UsersRoot $users) -join ',') 'none is left'
    Assert-True ($null -eq (Get-ItemProperty -LiteralPath "$users\S-1-5-21-1-1001\$run" -Name 'Stream To Speaker' -ErrorAction SilentlyContinue)) 'this install''s entry'
    Assert-True ($null -eq (Get-ItemProperty -LiteralPath "$users\S-1-5-21-1-1001\$approved" -Name 'Stream To Speaker' -ErrorAction SilentlyContinue)) 'its Task Manager state'
    Assert-True ($null -ne (Get-ItemProperty -LiteralPath "$users\S-1-5-21-1-1002\$run" -Name 'Stream To Speaker' -ErrorAction SilentlyContinue)) 'another copy''s entry stays'
    Assert-True (Test-RunValueInFolder 'C:\Apps\STS\stream-to-speaker.exe --startup' 'C:\Apps\STS') 'unquoted'
    Assert-True (-not (Test-RunValueInFolder '"C:\Program Files\Stream To Speaker Beta\stream-to-speaker.exe"' $dir)) 'a folder with a longer name'
}

Test 'an account is named as Windows shows it, and by its ID where Windows has no name' {
    $me = [System.Security.Principal.WindowsIdentity]::GetCurrent()
    Assert-Equal $me.Name (Get-AccountName $me.User.Value) 'the account that runs this'
    Assert-Equal 'S-1-5-21-1-2-3-4999' (Get-AccountName 'S-1-5-21-1-2-3-4999') 'an ID no account has'
    Assert-Equal '.DEFAULT' (Get-AccountName '.DEFAULT') 'not an ID'
}

Test 'sign-in entries that start an earlier folder''s copy start the new one' {
    $users = "$TestKey\MoveUsers"
    $run = 'Software\Microsoft\Windows\CurrentVersion\Run'
    foreach ($u in 'S-1-5-21-1-1001', 'S-1-5-21-1-1002') { $null = New-Item -Path "$users\$u\$run" -Force }
    $null = New-ItemProperty -LiteralPath "$users\S-1-5-21-1-1001\$run" -Name 'Stream To Speaker' -Value '"C:\STS [old]\stream-to-speaker.exe" --startup'
    $null = New-ItemProperty -LiteralPath "$users\S-1-5-21-1-1002\$run" -Name 'Stream To Speaker' -Value '"D:\Portable\stream-to-speaker.exe" --startup'
    $new = 'C:\Program Files\Stream To Speaker\stream-to-speaker.exe'
    Assert-Equal 1 (Move-SignInEntries -FromFolder 'C:\STS [old]' -ToExe $new -UsersRoot $users) 'moved'
    Assert-Equal "`"$new`" --startup" (Get-ItemProperty -LiteralPath "$users\S-1-5-21-1-1001\$run" -Name 'Stream To Speaker').'Stream To Speaker' 'the earlier folder''s entry'
    Assert-Equal '"D:\Portable\stream-to-speaker.exe" --startup' (Get-ItemProperty -LiteralPath "$users\S-1-5-21-1-1002\$run" -Name 'Stream To Speaker').'Stream To Speaker' 'another copy''s entry'
}

# ---------------------------------------------------------------------------
# Running copies, and Windows' output when none runs afterwards
# ---------------------------------------------------------------------------

Test 'only copies started from the install folder are closed, and each is told apart by its account' {
    $copies = @(
        [pscustomobject]@{ ProcessId = 11; Path = 'C:\Program Files\Stream To Speaker\stream-to-speaker.exe'; OwnerSid = 'S-1-5-21-9-1001' },
        [pscustomobject]@{ ProcessId = 12; Path = 'C:\Program Files\Stream To Speaker\stream-to-speaker.exe'; OwnerSid = 'S-1-5-21-9-1002' },
        [pscustomobject]@{ ProcessId = 13; Path = 'D:\dev\target\release\stream-to-speaker.exe'; OwnerSid = 'S-1-5-21-9-1001' },
        [pscustomobject]@{ ProcessId = 14; Path = 'C:\Program Files\Stream To Speaker Beta\stream-to-speaker.exe'; OwnerSid = 'S-1-5-21-9-1001' })
    $plan = @(Get-CopyPlan -Copies $copies -Folders @('C:\Program Files\Stream To Speaker') -PersonSid 'S-1-5-21-9-1001')
    Assert-Equal '11 | 12' ($plan | Where-Object { $_.Close } | ForEach-Object { $_.ProcessId }) 'closed'
    Assert-Equal '11 | 13 | 14' ($plan | Where-Object { $_.Mine } | ForEach-Object { $_.ProcessId }) 'the person''s'
    $plan = @(Get-CopyPlan -Copies $copies -Folders @('C:\Program Files\Stream To Speaker') -PersonSid '')
    Assert-Equal 0 @($plan | Where-Object { $_.Mine }).Count 'nobody''s when the person is unknown'
}

Test 'the person who ran Setup is found, and the app is started for them only without administrator rights' {
    $t0 = Get-Date '2026-11-25 10:00:00'
    $p = { param($Id, $Parent, $Name, $Seconds, $Owner) [pscustomobject]@{ ProcessId = $Id; ParentProcessId = $Parent; Name = $Name; CreationDate = $t0.AddSeconds($Seconds); Owner = $Owner } }
    $tmp = 'StreamToSpeakerSetup-0.2.0.tmp'
    $exe = 'StreamToSpeakerSetup-0.2.0.exe'
    $procs = @{}
    foreach ($x in @(
            # Setup asked for elevation and an administrator approved it.
            @(100, 90, $tmp, 3, 'ADMIN'), @(90, 80, $exe, 2, 'ADMIN'), @(80, 70, $tmp, 1, 'JANE'), @(70, 60, $exe, 0, 'JANE'),
            # Started elevated from a prompt (the test runner).
            @(200, 190, $tmp, 3, 'RUNNER'), @(190, 180, $exe, 2, 'RUNNER'), @(180, 1, 'powershell.exe', 0, 'RUNNER'),
            # "Run as administrator" with an administrator's password.
            @(400, 390, $tmp, 3, 'ADMIN'), @(390, 380, $exe, 2, 'ADMIN'), @(380, 1, 'explorer.exe', 0, 'JANE'),
            # The parent's ID now belongs to a newer process.
            @(300, 290, $tmp, 0, 'ADMIN'), @(290, 1, 'notepad.exe', 9, 'OTHER'),
            # Whoever started the Setup program is gone.
            @(500, 490, $tmp, 3, 'ADMIN'), @(490, 480, $exe, 2, 'ADMIN'))) {
        $procs[[int]$x[0]] = & $p $x[0] $x[1] $x[2] $x[3] $x[4]
    }
    $get = { param($Id) $procs[[int]$Id] }
    $owner = { param($P) $P.Owner }
    # The account; whether Setup starts the app for it; whether Setup changes its settings.
    $say = { param($Id) $r = Get-SetupPerson -SetupProcessId $Id -GetProcess $get -GetOwnerSid $owner
        "$($r.Sid) start=$(if ($r.CanStartAs) { 'yes' } else { 'no' }) settings=$(if ($r.CanActAs) { 'yes' } else { 'no' })" }
    Assert-Equal 'JANE start=yes settings=yes' (& $say 100) 'Setup asked for elevation, and an administrator''s password approved it'
    Assert-Equal 'RUNNER start=no settings=yes' (& $say 200) 'Setup started with administrator rights by the same account: the app would run with them'
    Assert-Equal 'JANE start=no settings=no' (& $say 400) 'Run as administrator with another account'
    Assert-Equal 'ADMIN start=no settings=yes' (& $say 500) 'whoever started the Setup program is gone'
    Assert-Equal ' start=no settings=no' (& $say 300) 'its parent''s ID belongs to another process now'
    Assert-Equal ' start=no settings=no' (& $say 0) 'no process'
}

Test 'a close that fails halfway says which copies are closed' {
    $mine = [pscustomobject]@{ ProcessId = 11; Mine = $true; Close = $true }
    $theirs = [pscustomobject]@{ ProcessId = 12; Mine = $false; Close = $true }
    $jane = [pscustomobject]@{ Sid = 'S-1-5-21-9-1001'; CanStartAs = $true; CanActAs = $true }
    Assert-Equal 'running-copies: mine=1 other=1 closed=failed closed-mine=1 closed-other=0 person=known start=yes settings=yes signin=off' `
        (Format-CopiesResult -Closing @($mine, $theirs) -Tried $true -Still @($theirs) -Person $jane -SignIn 'off') 'the other account''s copy still runs'
    Assert-Equal 'running-copies: mine=1 other=1 closed=ok closed-mine=1 closed-other=1 person=known start=yes settings=yes signin=none' `
        (Format-CopiesResult -Closing @($mine, $theirs) -Tried $true -Person $jane) 'both closed'
    Assert-Equal 'running-copies: mine=1 other=1 closed=none closed-mine=0 closed-other=0 person=known start=yes settings=yes signin=none' `
        (Format-CopiesResult -Closing @($mine, $theirs) -Tried $false -Person $jane) 'only looked'
    $nobody = [pscustomobject]@{ Sid = ''; CanStartAs = $false; CanActAs = $false }
    Assert-Equal 'running-copies: mine=0 other=0 closed=none closed-mine=0 closed-other=0 person=unknown start=no settings=no signin=none' `
        (Format-CopiesResult -Closing @() -Tried $true -Person $nobody) 'nothing to close, nobody known'
}

Test 'the person''s start-at-sign-in entry is read as on, turned off, or not there' {
    $users = "$TestKey\SignInUsers"
    $run = 'Software\Microsoft\Windows\CurrentVersion\Run'
    $approved = 'Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run'
    foreach ($u in 'S-1-5-21-1-1001', 'S-1-5-21-1-1002', 'S-1-5-21-1-1003', 'S-1-5-21-1-1004') {
        $null = New-Item -Path "$users\$u\$run" -Force
        $null = New-Item -Path "$users\$u\$approved" -Force
    }
    foreach ($u in 'S-1-5-21-1-1001', 'S-1-5-21-1-1002', 'S-1-5-21-1-1003') {
        $null = New-ItemProperty -LiteralPath "$users\$u\$run" -Name 'Stream To Speaker' -Value '"C:\Program Files\Stream To Speaker\stream-to-speaker.exe" --startup'
    }
    $null = New-ItemProperty -LiteralPath "$users\S-1-5-21-1-1002\$approved" -Name 'Stream To Speaker' -PropertyType Binary -Value ([byte[]](2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0))
    $null = New-ItemProperty -LiteralPath "$users\S-1-5-21-1-1003\$approved" -Name 'Stream To Speaker' -PropertyType Binary -Value ([byte[]](3, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8))
    $null = New-ItemProperty -LiteralPath "$users\S-1-5-21-1-1004\$approved" -Name 'Stream To Speaker' -PropertyType Binary -Value ([byte[]](3, 0, 0, 0))
    Assert-Equal 'on' (Get-SignInState -Sid 'S-1-5-21-1-1001' -UsersRoot $users) 'an entry Windows keeps no switch for'
    Assert-Equal 'on' (Get-SignInState -Sid 'S-1-5-21-1-1002' -UsersRoot $users) 'an entry that is switched on'
    Assert-Equal 'off' (Get-SignInState -Sid 'S-1-5-21-1-1003' -UsersRoot $users) 'an entry turned off in Task Manager'
    Assert-Equal 'none' (Get-SignInState -Sid 'S-1-5-21-1-1004' -UsersRoot $users) 'no entry, only a switch left behind'
    Assert-Equal 'none' (Get-SignInState -Sid 'S-1-5-21-1-1005' -UsersRoot $users) 'an account whose registry isn''t loaded'
    Assert-Equal 'none' (Get-SignInState -Sid '' -UsersRoot $users) 'nobody'
}

Test 'without the app, Windows goes back to the output it played to before' {
    $time = {
        param([int]$Year, [int]$Month, [int]$Day, [int]$Hour)
        $b = New-Object byte[] 16
        $i = 0
        foreach ($v in $Year, $Month, 0, $Day, $Hour, 0, 0, 0) { [BitConverter]::GetBytes([uint16]$v).CopyTo($b, $i); $i += 2 }
        , $b
    }
    Assert-Equal (Get-Date '2026-11-20 08:00:00').Ticks (ConvertFrom-RoleTime (& $time 2026 11 20 8)) 'a Role time'
    Assert-Equal 0 (ConvertFrom-RoleTime ([byte[]](1, 2, 3))) 'not a Role time'
    $outputs = @(
        [pscustomobject]@{ Id = 'ours'; State = 1; Ours = $true; RoleTimes = @{ 0 = 9; 1 = 9; 2 = 9 } },
        [pscustomobject]@{ Id = 'realtek'; State = 1; Ours = $false; RoleTimes = @{ 0 = 5; 1 = 5; 2 = 1 } },
        [pscustomobject]@{ Id = 'headset'; State = 1; Ours = $false; RoleTimes = @{ 0 = 2; 1 = 2; 2 = 7 } },
        [pscustomobject]@{ Id = 'hdmi'; State = 4; Ours = $false; RoleTimes = @{ 0 = 8; 1 = 8; 2 = 8 } })
    Assert-Equal 'realtek' (Select-FallbackOutput -Outputs $outputs -Role 0) 'console role'
    Assert-Equal 'headset' (Select-FallbackOutput -Outputs $outputs -Role 2) 'communications role'
    Assert-True ($null -eq (Select-FallbackOutput -Outputs @($outputs[0], $outputs[3]) -Role 0)) 'none that works'
}

# ---------------------------------------------------------------------------
# An earlier install in another folder
# ---------------------------------------------------------------------------

function New-EarlierFolder([string]$Name) {
    $dir = Join-Path $TestDir $Name
    foreach ($f in 'stream-to-speaker.exe', 'unins000.exe', 'unins000.dat', 'README.md', 'driver\devcon.exe',
                   'driver\StreamToSpeaker.inf', 'scripts\Diagnose.ps1', 'logs\install.log') {
        $path = Join-Path $dir $f
        $null = [IO.Directory]::CreateDirectory((Split-Path -Parent $path))
        [IO.File]::WriteAllText($path, $f)
    }
    $dir
}

Test 'an earlier folder loses only what releases put there, and goes when empty' {
    $dir = New-EarlierFolder 'earlier [a]'
    $r = Remove-EarlierInstall -Folder $dir
    Assert-Equal 'removed' $r.Result 'result'
    Assert-True (-not [IO.Directory]::Exists($dir)) 'the folder is gone'

    $dir = New-EarlierFolder 'earlier [b]'
    [IO.File]::WriteAllText((Join-Path $dir 'notes.txt'), 'mine')
    [IO.File]::WriteAllText((Join-Path $dir 'scripts\my-script.ps1'), 'mine too')
    $r = Remove-EarlierInstall -Folder $dir
    Assert-Equal 'not-empty' $r.Result 'result'
    Assert-Equal 'mine' ([IO.File]::ReadAllText((Join-Path $dir 'notes.txt'))) 'the person''s file'
    Assert-True ([IO.File]::Exists((Join-Path $dir 'scripts\my-script.ps1'))) 'the person''s file in a folder of ours'
    Assert-True (-not [IO.File]::Exists((Join-Path $dir 'unins000.exe'))) 'the earlier uninstaller'
    Assert-True (-not [IO.Directory]::Exists((Join-Path $dir 'driver'))) 'an emptied folder of ours'
    Assert-Equal 'gone' (Remove-EarlierInstall -Folder (Join-Path $TestDir 'never there')).Result 'a folder that is gone'
}

Test 'nothing is deleted through a link or junction' {
    $target = New-EarlierFolder 'link target'
    $link = Join-Path $TestDir 'earlier link'
    $null = & cmd.exe /c mklink /J "$link" "$target"
    Assert-True (Test-PathHasLink $link) 'a junction is seen'
    Assert-True (Test-PathHasLink (Join-Path $link 'driver\devcon.exe')) 'so is a file inside one'
    $r = Remove-EarlierInstall -Folder $link
    Assert-Equal 'left-alone' $r.Result 'result'
    Assert-True ([IO.File]::Exists((Join-Path $target 'stream-to-speaker.exe'))) 'files behind the junction'

    $dir = New-EarlierFolder 'earlier [c]'
    $elsewhere = New-EarlierFolder 'somewhere else'
    [IO.Directory]::Delete((Join-Path $dir 'driver'), $true)
    $null = & cmd.exe /c mklink /J "$(Join-Path $dir 'driver')" "$(Join-Path $elsewhere 'driver')"
    $r = Remove-EarlierInstall -Folder $dir
    Assert-Equal 'not-empty' $r.Result 'result with a junction inside'
    Assert-True ([IO.File]::Exists((Join-Path $elsewhere 'driver\devcon.exe'))) 'the file the junction leads to'
    Assert-True (-not [IO.File]::Exists((Join-Path $dir 'stream-to-speaker.exe'))) 'the files beside it'
    Assert-True (-not (Test-PathHasLink (Join-Path $elsewhere 'README.md'))) 'a plain path'
    # Removing a junction itself leaves what it leads to.
    [IO.Directory]::Delete($link)
    [IO.Directory]::Delete((Join-Path $dir 'driver'))
}

Test 'a link put in place after the folder was checked leads no delete elsewhere' {
    # The check of the path is made blind, as if each link had appeared just
    # after it.
    function Test-PathHasLink { $false }

    $dir = New-EarlierFolder 'earlier [d]'
    $new = New-EarlierFolder 'new install [d]'
    [IO.Directory]::Delete((Join-Path $dir 'driver'), $true)
    $null = & cmd.exe /c mklink /J "$(Join-Path $dir 'driver')" "$(Join-Path $new 'driver')"
    $r = Remove-EarlierInstall -Folder $dir
    Assert-Equal 'not-empty' $r.Result 'a folder of the earlier install replaced by a link'
    Assert-True ([IO.File]::Exists((Join-Path $new 'driver\devcon.exe'))) 'the file the link leads to'
    Assert-True ($r.Kept -contains 'driver\devcon.exe') 'which is reported as kept'
    Assert-True (-not [IO.File]::Exists((Join-Path $dir 'stream-to-speaker.exe'))) 'the earlier install''s own files'
    [IO.Directory]::Delete((Join-Path $dir 'driver'))

    $link = Join-Path $TestDir 'earlier link [d]'
    $null = & cmd.exe /c mklink /J "$link" "$new"
    $r = Remove-EarlierInstall -Folder $link
    Assert-Equal 'left-alone' $r.Result 'the earlier folder itself replaced by a link'
    Assert-True ([IO.File]::Exists((Join-Path $new 'stream-to-speaker.exe'))) 'the files behind it'
    Assert-True ([IO.File]::Exists((Join-Path $new 'unins000.exe'))) 'the uninstaller behind it'
    [IO.Directory]::Delete($link)
}

Test 'the folder of this install is never emptied as an earlier one' {
    $dir = New-EarlierFolder 'this install'
    Assert-Equal 'gone' (Remove-EarlierInstall -Folder $dir -Keep $dir).Result 'the same folder'
    Assert-Equal 'gone' (Remove-EarlierInstall -Folder $dir.ToUpperInvariant() -Keep $dir).Result 'the same folder, written another way'
    Assert-Equal 'gone' (Remove-EarlierInstall -Folder $TestDir -Keep $dir).Result 'a folder that holds it'
    Assert-True ([IO.File]::Exists((Join-Path $dir 'unins000.exe'))) 'its files'
    $other = New-EarlierFolder 'another install'
    Assert-Equal 'removed' (Remove-EarlierInstall -Folder $dir -Keep $other).Result 'a folder that is not this install'
    Assert-True ([IO.File]::Exists((Join-Path $other 'unins000.exe'))) 'this install''s files'
}

Test 'a shortcut that still starts the earlier copy goes; others stay' {
    $shell = New-Object -ComObject WScript.Shell
    $links = @()
    foreach ($pair in @(@('to-old.lnk', 'old copy'), @('to-new.lnk', 'new copy'))) {
        $exe = Join-Path (Join-Path $TestDir $pair[1]) 'stream-to-speaker.exe'
        $null = [IO.Directory]::CreateDirectory((Split-Path -Parent $exe))
        [IO.File]::WriteAllText($exe, 'exe')
        $path = Join-Path $TestDir $pair[0]
        $l = $shell.CreateShortcut($path)
        $l.TargetPath = $exe
        $l.Save()
        $links += $path
    }
    # The folder as Windows writes it in the shortcut (the temporary folder
    # can be given in its short 8.3 form).
    $old = Split-Path -Parent $shell.CreateShortcut($links[0]).TargetPath
    Assert-Equal 1 (Remove-ShortcutsInto -Links ($links + (Join-Path $TestDir 'missing.lnk')) -Folder $old) 'removed'
    Assert-True (-not [IO.File]::Exists($links[0])) 'the earlier copy''s shortcut'
    Assert-True ([IO.File]::Exists($links[1])) 'the new copy''s shortcut'
}

# ---------------------------------------------------------------------------
# System programs and modules come from Windows itself
# ---------------------------------------------------------------------------

Test 'pnputil and the INF folder come from Windows, whatever %windir% says' {
    $saved = $env:windir
    $env:windir = Join-Path $TestDir 'not Windows'
    try {
        $seen = & { . ([ScriptBlock]::Create([IO.File]::ReadAllText((Join-Path $here 'InstallerFunctions.ps1')))); $StsPnputil; $StsWindowsDir }
    } finally {
        $env:windir = $saved
    }
    Assert-Equal (Join-Path ([Environment]::GetFolderPath('System')) 'pnputil.exe') $seen[0] 'pnputil'
    Assert-Equal ([Environment]::GetFolderPath('Windows')) $seen[1] 'Windows folder'
}

Test 'every script that runs as administrator loads modules only from Windows PowerShell''s own folder' {
    $missing = @()
    foreach ($name in 'Install-Driver.ps1', 'Uninstall-Driver.ps1', 'Remove-TestCertificate.ps1', 'Set-FirewallRule.ps1',
                      'Remove-SignInEntries.ps1', 'Rename-Endpoint.ps1', 'Reset-Install.ps1', 'Find-RunningCopies.ps1', 'Remove-EarlierInstall.ps1') {
        $text = [IO.File]::ReadAllText((Join-Path $here $name))
        # Before the shared functions load, which load modules.
        $at = $text.IndexOf("`$env:PSModulePath = Join-Path `$PSHOME 'Modules'")
        if ($at -lt 0 -or $at -gt $text.IndexOf('ReadAllText')) { $missing += $name }
    }
    Assert-Equal '' ($missing -join ', ') 'scripts that don''t'
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
