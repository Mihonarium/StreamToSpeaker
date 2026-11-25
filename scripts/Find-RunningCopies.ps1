# Finds the running copies of Stream To Speaker that Setup or the
# uninstaller must close, and with -Close closes them. Only copies started
# from -Folders are closed (they hold files that are replaced or removed);
# a copy started from anywhere else is left alone. Setup runs it elevated
# from its own temporary folder, before anything is installed; the
# uninstaller from the install folder.
#
#   -Folders         folders, separated by |
#   -SetupProcessId  Setup's process: the account that ran it is the person
#                    whose copies are "mine" (Get-SetupPerson)
#
# Prints one "running-copy:" line per copy and, last,
#   running-copies: mine=<n> other=<n> closed=none|ok|failed|error person=known|unknown start=yes|no
# where mine and other count the copies to close that are the person's and
# another account's (with person=unknown, the account that ran Setup
# couldn't be told, and every copy counts as other), and start says whether
# Setup can start a program as that person. closed=error: the script itself
# failed, and Setup goes on without it (Windows' Restart Manager still
# closes a copy that holds a file Setup replaces).
#
# Keep this file ASCII (see InstallerFunctions.ps1).

param(
    [string]$Folders = '',
    [int]$SetupProcessId = 0,
    [switch]$Close
)

$ErrorActionPreference = 'Continue'
# Modules load only from Windows PowerShell's own folder: the inherited
# module path starts with the profile of the user who approved the
# elevation, where that user's own programs could put a module.
$env:PSModulePath = Join-Path $PSHOME 'Modules'
$line = 'running-copies: mine=0 other=0 closed=error person=unknown start=yes'
try {
    . ([ScriptBlock]::Create([IO.File]::ReadAllText((Join-Path $PSScriptRoot 'InstallerFunctions.ps1'))))
    $StsLogPrefix = '[running-copies] '
    $folderList = @($Folders -split '\|' | Where-Object { $_.Trim() } | ForEach-Object { $_.Trim() })
    $person = Get-SetupPerson -SetupProcessId $SetupProcessId
    $plan = @(Get-CopyPlan -Copies @(Get-AppCopies) -Folders $folderList -PersonSid $person.Sid)
    foreach ($c in $plan) {
        Write-Host ("running-copy: pid={0} mine={1} close={2} path={3}" -f $c.ProcessId, [int]$c.Mine, [int]$c.Close, $c.Path)
        if (-not $c.Close) { Write-Step "Leaving the copy started from $($c.Path) running: it isn't the installed one." }
    }
    $toClose = @($plan | Where-Object { $_.Close })
    $mine = @($toClose | Where-Object { $_.Mine }).Count
    $other = $toClose.Count - $mine
    $closed = 'none'
    if ($Close -and $toClose.Count) {
        $still = @(Stop-AppCopies $toClose)
        $closed = if ($still.Count) { 'failed' } else { 'ok' }
        foreach ($c in $still) { Write-Step "Process $($c.ProcessId) is still running." }
    }
    $known = if ($person.Sid) { 'known' } else { 'unknown' }
    $start = if ($person.CanStartAs) { 'yes' } else { 'no' }
    $line = "running-copies: mine=$mine other=$other closed=$closed person=$known start=$start"
} catch {
    Write-Host "Find-RunningCopies failed: $($_.Exception.Message)"
}
Write-Host $line
exit 0
