# The installer's driver step: puts Stream To Speaker's sound driver on this
# PC and makes its sound output usable. Setup runs it elevated after copying
# the files, reads the "driver-step-result:" line it prints last and says on
# its last page what happened. It can also be run on its own, elevated, to
# repair the sound output (nothing is changed when everything works):
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File "C:\Program Files\Stream To Speaker\scripts\Install-Driver.ps1"
#
#   - An update or repair whose driver is the one already installed leaves
#     the device, its sound output (and so Windows' choice of output) and
#     Windows' audio services alone.
#   - A new driver goes into the driver store first; only then is it
#     installed on the existing device, which keeps its sound output.
#     Earlier packages are deleted by the run that finds the new driver
#     working: this one when no restart is due, else the next run after
#     the restart.
#   - The device is created only when there is none.
#   - An output that still carries an early build's cached settings is
#     rebuilt, and so is a working device when Windows has no Stream To
#     Speaker output at all although its sound services run: a new device
#     is created first, then the old one removed, and the new output gets
#     the default-output roles the old one held.
# The decisions are in InstallerFunctions.ps1 (Invoke-DriverInstall).
#
# Exit code: 0 working; 3010 working after a restart; 20 the driver package
# or a test build's certificate wasn't accepted (nothing changed); 21 the
# device couldn't be set up; 22 Windows didn't start the driver; 23 no sound
# output appeared; 24 the sound output is turned off in Windows; 25 a test
# build on a PC whose Windows doesn't run test builds' drivers (nothing
# changed); 1 the script itself failed.
#
# Keep this file ASCII (see InstallerFunctions.ps1).

[CmdletBinding()]
param(
    # A copy of the test certificate the build being replaced shipped
    # (Setup saves it before the new files go in).
    [string]$PreviousCertificate = '',
    # Install a test build's driver although Windows' test-signing mode is
    # off (the installer lifecycle test does, on machines that can't run it).
    [switch]$AllowTestBuild,
    # No copy of the app runs after this: if Windows plays to the app's
    # output, it goes back to the output it played to before.
    [switch]$NoAppAfterwards
)

$ErrorActionPreference = 'Continue'
# Modules load only from Windows PowerShell's own folder: the inherited
# module path starts with the profile of the user who approved the
# elevation, where that user's own programs could put a module.
$env:PSModulePath = Join-Path $PSHOME 'Modules'

# The transcript goes to <install dir>\logs\install.log. Setup runs this
# script elevated, so where the log goes follows from where the script is,
# never from the profile of the user who approved the install: that user's
# own programs can turn a folder in the profile into a link to anywhere,
# and an elevated write would follow it. In the install directory (Program
# Files, which only administrators can change) the log is as protected as
# the scripts are.
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
$line = 'driver-step-result: script-failed restart=0 replaced=0 new-output=0 problem=0 kept-before=0 action=none ' +
    'extras-removed=0 extras-left=0 was-default=0 is-default=0 sound-moved=0'
try {
    . ([ScriptBlock]::Create([IO.File]::ReadAllText((Join-Path $PSScriptRoot 'InstallerFunctions.ps1'))))
    $StsLogPrefix = '[install-driver] '
    $StsDevcon = Join-Path (Split-Path -Parent $PSScriptRoot) 'driver\devcon.exe'
    Initialize-NativeHelpers
    $result = Invoke-DriverInstall -PackageDir (Join-Path (Split-Path -Parent $PSScriptRoot) 'driver') `
        -PreviousCertificate $PreviousCertificate -AllowTestBuild:$AllowTestBuild -NoAppAfterwards:$NoAppAfterwards
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
