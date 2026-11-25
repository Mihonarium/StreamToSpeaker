# The installer's driver step: puts Stream To Speaker's sound driver on this
# PC and makes its sound output usable. Setup runs it elevated after copying
# the files, reads the "driver-step-result:" line it prints last and says on
# its last page what happened.
#
#   - An update or repair whose driver is the one already installed leaves
#     the device, its sound output (and so Windows' choice of output) and
#     Windows' audio services alone.
#   - A new driver goes into the driver store first; only then is it
#     installed on the existing device, which keeps its sound output.
#     Packages nothing uses any more are deleted afterwards.
#   - The device is created only when there is none.
#   - An output that still carries an early build's cached settings is
#     rebuilt, and Windows plays to it again as before.
# The decisions are in InstallerFunctions.ps1 (Invoke-DriverInstall).
#
# Exit code: 0 working; 3010 working after a restart; 20 the driver package
# wasn't accepted (nothing changed); 21 the device couldn't be set up; 22
# Windows didn't start the driver; 23 no sound output appeared; 24 the sound
# output is turned off in Windows; 1 the script itself failed.
#
# Keep this file ASCII (see InstallerFunctions.ps1).

[CmdletBinding()]
param(
    # A copy of the test certificate the build being replaced shipped
    # (Setup saves it before the new files go in).
    [string]$PreviousCertificate = ''
)

$ErrorActionPreference = 'Continue'

# The transcript goes to <install dir>\logs\install.log. Setup runs this
# script elevated, so where the log goes follows from where the script is,
# never from the profile of the user who approved the install: that user's
# own programs can turn a folder in the profile into a link to anywhere,
# and an elevated write would follow it. In the install directory the log
# is as protected as the scripts are (Setup makes sure only administrators
# can change that folder, wherever it is).
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
"==== Install-Driver started at $(Get-Date -Format 'u') ====" | Write-Host

$exitCode = 1
$line = 'driver-step-result: script-failed restart=0 new-output=0 problem=0 kept-before=0 action=none'
try {
    . ([ScriptBlock]::Create([IO.File]::ReadAllText((Join-Path $PSScriptRoot 'InstallerFunctions.ps1'))))
    $StsLogPrefix = '[install-driver] '
    $StsDevcon = Join-Path (Split-Path -Parent $PSScriptRoot) 'driver\devcon.exe'
    Initialize-NativeHelpers
    $result = Invoke-DriverInstall -PackageDir (Join-Path (Split-Path -Parent $PSScriptRoot) 'driver') `
        -PreviousCertificate $PreviousCertificate
    $line = Format-DriverResult $result
    $exitCode = $result.ExitCode
} catch {
    Write-Host "Install-Driver failed: $($_.Exception.Message)"
    Write-Host $_.ScriptStackTrace
} finally {
    Write-Host $line
    "==== Install-Driver finished at $(Get-Date -Format 'u') ====" | Write-Host
    if ($transcribing) { $null = Stop-Transcript }
}
exit $exitCode
