# Adds (or, with -Remove, removes) the Windows Firewall rule that lets
# speakers on private and work networks reach Stream To Speaker. Setup and
# the uninstaller run it elevated. Without the rule, Windows asks at the
# app's first start; answered from a standard account, or with Cancel, that
# question leaves rules that block every speaker, and it isn't asked again.
# What the rule allows, and why, is in InstallerFunctions.ps1
# (Get-FirewallRuleSpec).
#
# -Remove also removes the rules Windows' question made for the program.
#
# Prints "firewall-rule-result: ok" or "firewall-rule-result: failed" last.
#
# Keep this file ASCII (see InstallerFunctions.ps1).

param(
    [string]$Program = '',
    [switch]$Remove
)

$ErrorActionPreference = 'Continue'
$ok = $false
try {
    . ([ScriptBlock]::Create([IO.File]::ReadAllText((Join-Path $PSScriptRoot 'InstallerFunctions.ps1'))))
    $StsLogPrefix = '[firewall] '
    if (-not $Program) { $Program = Join-Path (Split-Path -Parent $PSScriptRoot) $StsExeName }
    if ($Remove) {
        $n = Remove-AppFirewallRules $Program
        if (-not $n) { Write-Step "No Windows Firewall rule for $Program." }
    } else {
        Set-AppFirewallRule $Program
    }
    $ok = $true
} catch {
    Write-Host "Windows Firewall: $($_.Exception.Message)"
}
Write-Host "firewall-rule-result: $(if ($ok) { 'ok' } else { 'failed' })"
exit $(if ($ok) { 0 } else { 1 })
