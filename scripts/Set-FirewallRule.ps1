# Adds (or, with -Remove, removes) the Windows Firewall rule that lets
# speakers on private and work networks reach Stream To Speaker. Setup and
# the uninstaller run it elevated. Without the rule, Windows asks at the
# app's first start; answered from a standard account, or with Cancel, that
# question leaves rules that block every speaker, and it isn't asked again.
# What the rule allows, and why, is in InstallerFunctions.ps1
# (Get-FirewallRuleSpec).
#
# -Remove also removes the rules Windows' question made for the program,
# and prints a "left:" line for each rule that is still there afterwards,
# which the uninstaller names in its last message.
#
# Prints "firewall-rule-result: ok" or "firewall-rule-result: failed" last.
#
# Keep this file ASCII (see InstallerFunctions.ps1).

param(
    [string]$Program = '',
    [switch]$Remove
)

$ErrorActionPreference = 'Continue'
# Modules load only from Windows PowerShell's own folder: the inherited
# module path starts with the profile of the user who approved the
# elevation, where that user's own programs could put a module.
$env:PSModulePath = Join-Path $PSHOME 'Modules'
$ok = $false
try {
    . ([ScriptBlock]::Create([IO.File]::ReadAllText((Join-Path $PSScriptRoot 'InstallerFunctions.ps1'))))
    $StsLogPrefix = '[firewall] '
    if (-not $Program) { $Program = Join-Path (Split-Path -Parent $PSScriptRoot) $StsExeName }
    if ($Remove) {
        $n = Remove-AppFirewallRules $Program
        if (-not $n) { Write-Step "No Windows Firewall rule for $Program." }
        foreach ($rule in @(Get-AppFirewallRules $Program)) {
            Write-Host "left: the Windows Firewall rule '$($rule.DisplayName)' for $Program"
        }
    } else {
        Set-AppFirewallRule $Program
    }
    $ok = $true
} catch {
    Write-Host "Windows Firewall: $($_.Exception.Message)"
    if ($Remove) { Write-Host "left: the Windows Firewall rule for Stream To Speaker (removing it failed: $($_.Exception.Message))" }
}
Write-Host "firewall-rule-result: $(if ($ok) { 'ok' } else { 'failed' })"
exit $(if ($ok) { 0 } else { 1 })
