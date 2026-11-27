# Run with pwsh on any host. Load just the two readers so setup never runs.
$ErrorActionPreference = "Stop"
$path = Join-Path $PSScriptRoot "Test-InstallerLifecycle.ps1"
$errors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile($path, [ref]$null, [ref]$errors)
if ($errors) { throw ($errors -join [Environment]::NewLine) }
foreach ($name in "Test-ActiveRenderEndpoint", "Wait-OutputDevices") {
    $reader = $ast.Find({
        param($node)
        $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $name
    }, $false)
    . ([scriptblock]::Create($reader.Extent.Text))
}

$instance = "ROOT\MEDIA\0001"
$key = [pscustomobject]@{ PSChildName = "{endpoint}"; Name = "Render\{endpoint}" }
$key | Add-Member ScriptMethod GetValue {
    param($name)
    if ($name -ne "DeviceState") { throw "Unexpected value name $name" }
    if ($case -eq "missing state") { return $null }
    if ($case -eq "disabled") { return 2 }
    return 1
}
function Get-ChildItem {
    [CmdletBinding()] param([string]$LiteralPath)
    if ($LiteralPath -cne "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render") {
        throw "Unexpected render registry path '$LiteralPath'"
    }
    if ($case -eq "registry error") { Write-Error "Cannot enumerate render devices" }
    if ($case -ne "empty") { $key }
}
function Get-PnpDeviceProperty {
    [CmdletBinding()] param([string]$InstanceId, [string]$KeyName)
    if ($InstanceId -cne "SWD\MMDEVAPI\{0.0.0.00000000}.{endpoint}") { throw "Unexpected endpoint instance '$InstanceId'" }
    if ($KeyName -cne "DEVPKEY_Device_Parent") { throw "Unexpected property '$KeyName'" }
    if ($case -eq "parent error") { Write-Error "Cannot read endpoint parent" }
    $parent = switch ($case) {
        "other device" { "ROOT\MEDIA\0002" }
        "missing parent" { $null }
        default { $instance }
    }
    [pscustomobject]@{ Data = $parent }
}
foreach ($case in "active", "disabled", "empty", "other device", "registry error", "missing state", "parent error", "missing parent") {
    $expected = switch ($case) {
        "active" { "Active" }
        { $_ -in "disabled", "empty", "other device" } { "Inactive" }
        default { "Unreadable" }
    }
    $actual = Test-ActiveRenderEndpoint $instance
    if ($actual -ne $expected) { throw "$case returned $actual (expected $expected)" }
}

function Get-OurDevices($onError) {
    $script:case = $samples.Dequeue()
    if ($case -ne "removed") {
        [pscustomobject]@{ Present = $true; PNPDeviceID = $instance; ConfigManagerErrorCode = 0 }
    }
}
function Start-Sleep([int]$Milliseconds) {}

foreach ($expectedEndpoint in "", "Active") {
    $samples = [Collections.Generic.Queue[string]]::new()
    $samples.Enqueue("empty")
    if ($expectedEndpoint) { $samples.Enqueue("empty") }
    $samples.Enqueue("active")
    $samples.Enqueue("parent error")
    $samples.Enqueue("registry error")
    $samples.Enqueue("active")
    $samples.Enqueue("active")
    $devices = @(Wait-OutputDevices 1 0 $expectedEndpoint)
    if ($samples.Count -ne 0 -or $devices.Count -ne 1 -or $devices[0].Endpoint -ne "Active") {
        throw "Expected the second consecutive active reading"
    }
}
$samples.Enqueue("removed")
$samples.Enqueue("removed")
if (@(Wait-OutputDevices 0).Count -ne 0 -or $samples.Count -ne 0) {
    throw "Expected two readings with the device absent"
}
$samples.Enqueue("parent error")
try {
    Wait-OutputDevices 1 -timeoutSeconds 0
    throw "An unreadable endpoint passed the settle check"
} catch {
    if ($_.Exception.Message -notlike "*Last reading: $instance 0 Unreadable") { throw }
}
Write-Host "Endpoint readings and settle checks passed."
