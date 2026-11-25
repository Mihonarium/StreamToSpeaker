# Names Stream To Speaker's sound output and turns it on, by hand.
#
# Setup does this itself (Install-Driver.ps1); run this when the output
# shows a name Windows made up, such as "Speakers (Stream To Speaker)" or
# "Internal AUX Jack", or is turned off in Sound settings. -Name sets a
# name of your own. From an elevated PowerShell:
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File "C:\Program Files\Stream To Speaker\scripts\Rename-Endpoint.ps1"
#
# It writes through IPolicyConfig, the interface Sound settings itself
# uses: on Windows 11 24H2 and later the audio service overwrites direct
# registry writes to an endpoint's entry. Only the outputs of the Stream To
# Speaker device that are there now are changed, never a cached entry of a
# device that is gone.
#
# Output also goes to logs\install.log in the folder above this script's
# own (as for Setup's own run); run without elevation from a folder only
# administrators can change, it goes to the console only.
#
# Keep this file ASCII (see InstallerFunctions.ps1).

[CmdletBinding()]
param(
    [string]$Name = 'Stream To Speaker'
)

$ErrorActionPreference = 'Continue'

$logDir = Join-Path (Split-Path -Parent $PSScriptRoot) 'logs'
$logFile = Join-Path $logDir 'install.log'
$transcribing = $false
try {
    $null = New-Item -ItemType Directory -Force -Path $logDir -ErrorAction Stop
    $null = Start-Transcript -LiteralPath $logFile -Append -ErrorAction Stop
    $transcribing = $true
} catch {
    Write-Host "Not logging to $logFile ($($_.Exception.Message)); output goes to this console only."
}
"==== Rename-Endpoint started at $(Get-Date -Format 'u') ====" | Write-Host

$exitCode = 1
try {
    . ([ScriptBlock]::Create([IO.File]::ReadAllText((Join-Path $PSScriptRoot 'InstallerFunctions.ps1'))))
    Initialize-NativeHelpers
    $devices = @(Get-OurDevices)
    $working = @($devices | Where-Object { $_.ProblemCode -eq 0 })
    if (-not $devices.Count) {
        Write-Host 'There is no Stream To Speaker device on this PC. Run Setup again to install it.'
        $exitCode = 2
    } elseif (-not $working.Count) {
        foreach ($d in $devices) {
            Write-Host "The Stream To Speaker device $($d.InstanceId) isn't working: $(Get-DeviceProblemText $d.ProblemCode). Its sound output appears once it works."
        }
        $exitCode = 2
    } else {
        $outputs = @(Select-PresentEndpoints -Endpoints @(Get-OurEndpoints) -InstanceIds @($working | ForEach-Object { $_.InstanceId }))
        if (-not $outputs.Count) {
            Write-Host 'The Stream To Speaker device works, but Windows shows no sound output for it yet. Restart the PC and run this again.'
            $exitCode = 2
        } else {
            foreach ($ep in $outputs) {
                $changes = @(Get-EndpointChanges -Endpoint $ep -MakeVisible $true -Name $Name)
                Write-Host "Sound output $($ep.Guid) ('$($ep.FriendlyName)'): $(if ($changes.Count) { 'setting ' + ($changes -join ', ') } else { 'nothing to change' })"
                if ($changes.Count) { Set-EndpointState -Endpoint $ep -Changes $changes -Name $Name }
            }
            $after = @(Select-PresentEndpoints -Endpoints @(Get-OurEndpoints) -InstanceIds @($working | ForEach-Object { $_.InstanceId }))
            foreach ($ep in $after) {
                $state = if ($ep.State -eq $StsEndpointActive) { 'on' } else { 'off' }
                Write-Host "Now: '$($ep.FriendlyName)', $state."
            }
            $exitCode = if (@($after | Where-Object { $_.State -eq $StsEndpointActive -and $_.FriendlyName -eq $Name }).Count) { 0 } else { 3 }
            if ($exitCode -ne 0) {
                Write-Host "Windows didn't keep every change. Sound settings may still show the old name or ask you to allow the output; restart the PC and run this again."
            }
        }
    }
} catch {
    Write-Host "Rename-Endpoint failed: $($_.Exception.Message)"
    Write-Host $_.ScriptStackTrace
} finally {
    "==== Rename-Endpoint finished at $(Get-Date -Format 'u') ====" | Write-Host
    if ($transcribing) { $null = Stop-Transcript }
}
exit $exitCode
