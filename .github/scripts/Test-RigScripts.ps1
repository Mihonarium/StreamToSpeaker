<#
.SYNOPSIS
    Parses the PowerShell the Panel click workflow runs, and with
    -CompileRig compiles the rig's C# as the rig does, so that a mistake
    there fails a run in a minute instead of after a build and every case
    (panel-click.yml).

.DESCRIPTION
    Parses, with the PowerShell this script runs in, every script file the
    workflow runs (Test-PanelClick.ps1 and this one) and the inline script of
    every step of panel-click.yml that runs in PowerShell (pwsh or
    powershell). Errors give the file, line, column and parser message and go to
    the step summary. For inline scripts the error also gives the line where
    the step starts. The error position comes from the trimmed script with
    folded lines joined, offset to its start in the workflow. It can differ
    from the position in the YAML.

    With -CompileRig (on Windows), PanelRig.cs is compiled the way
    Test-PanelClick.ps1 compiles it, with Add-Type in Windows PowerShell,
    in a separate process so that nothing of it stays loaded here; the
    compiler's errors are printed and go to the step summary.

    Exits with 1 when anything fails.

.PARAMETER CompileRig
    Also compile PanelRig.cs in a Windows PowerShell process.
#>
[CmdletBinding()]
param(
    [switch]$CompileRig
)

$ErrorActionPreference = "Stop"

$Root = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$Workflow = [IO.Path]::Combine($Root, ".github", "workflows", "panel-click.yml")
$Files = @("Test-PanelClick.ps1", "Test-RigScripts.ps1")
$Edition = "PowerShell $($PSVersionTable.PSVersion) ($($PSVersionTable.PSEdition))"
$Problems = New-Object System.Collections.Generic.List[string]

function Add-ParseErrors($errors, [string]$where, [int]$firstLine) {
    foreach ($e in $errors) {
        $line = $e.Extent.StartLineNumber + $firstLine - 1
        $Problems.Add(("{0}, line {1}, column {2}: {3}" -f $where, $line, $e.Extent.StartColumnNumber, $e.Message))
    }
}

# The script files.
foreach ($name in $Files) {
    $path = [IO.Path]::Combine($PSScriptRoot, $name)
    $tokens = $null
    $errors = $null
    $null = [System.Management.Automation.Language.Parser]::ParseFile($path, [ref]$tokens, [ref]$errors)
    Add-ParseErrors $errors (".github/scripts/" + $name) 1
    Write-Host ("Parsed .github/scripts/{0} with {1}: {2} error(s)" -f $name, $Edition, @($errors).Count)
}

# The inline scripts of the workflow's steps that run in PowerShell. A step
# starts at a line "- " and goes on while lines are indented at least as far
# as its keys; its script is the value of "run:" (a block after "|" or ">",
# whose lines ">" joins with spaces).
$lines = @(Get-Content -LiteralPath $Workflow)
$i = 0
$inline = 0
while ($i -lt $lines.Count) {
    $dash = [regex]::Match($lines[$i], '^(\s*)-\s')
    if (-not $dash.Success) {
        $i++
        continue
    }
    $keys = $dash.Groups[1].Length + 2
    $start = $i
    $j = $i + 1
    while ($j -lt $lines.Count -and ($lines[$j].Trim() -eq "" -or ($lines[$j].Length - $lines[$j].TrimStart().Length) -ge $keys)) {
        $j++
    }
    $step = @($lines[$start..($j - 1)])
    $step[0] = (" " * $keys) + $step[0].TrimStart().Substring(2)
    $shell = ""
    $script = $null
    $scriptLine = 0
    for ($k = 0; $k -lt $step.Count; $k++) {
        $text = $step[$k]
        $indent = $text.Length - $text.TrimStart().Length
        if ($indent -ne $keys) { continue }
        if ($text -match '^\s*shell:\s*(\S+)') { $shell = $Matches[1] }
        if ($text -match '^\s*run:\s*(.*?)\s*$') {
            $value = $Matches[1]
            if ($value -in @("|", "|-", ">", ">-")) {
                $body = New-Object System.Collections.Generic.List[string]
                for ($m = $k + 1; $m -lt $step.Count; $m++) {
                    $b = $step[$m]
                    if ($b.Trim() -ne "" -and ($b.Length - $b.TrimStart().Length) -le $keys) { break }
                    $body.Add($b.Trim())
                }
                $joiner = "`n"
                if ($value.StartsWith(">")) { $joiner = " " }
                $script = ($body -join $joiner)
                $scriptLine = $start + $k + 2
            } else {
                $script = $value
                $scriptLine = $start + $k + 1
            }
        }
    }
    if ($null -ne $script -and $shell -in @("pwsh", "powershell")) {
        $tokens = $null
        $errors = $null
        $null = [System.Management.Automation.Language.Parser]::ParseInput($script, [ref]$tokens, [ref]$errors)
        $where = ".github/workflows/panel-click.yml, the step at line {0}" -f ($start + 1)
        Add-ParseErrors $errors $where $scriptLine
        $inline++
    }
    $i = $j
}
Write-Host ("Parsed the {0} inline PowerShell scripts of panel-click.yml with {1}" -f $inline, $Edition)
if ($inline -eq 0) { $Problems.Add("found no inline PowerShell script in panel-click.yml: the reader of its steps is wrong") }

# The rig's C#, as the rig compiles it, in a separate Windows PowerShell.
if ($CompileRig) {
    $cs = [IO.Path]::Combine($PSScriptRoot, "PanelRig.cs")
    $windowsPowerShell = [IO.Path]::Combine($env:SystemRoot, "System32", "WindowsPowerShell", "v1.0", "powershell.exe")
    $ErrorActionPreference = "Continue"
    $output = @(& $windowsPowerShell -NoProfile -NonInteractive -Command "Add-Type -Path '$cs'" 2>&1 | ForEach-Object { "$_" })
    $code = $LASTEXITCODE
    $ErrorActionPreference = "Stop"
    if ($code -ne 0) {
        $Problems.Add(".github/scripts/PanelRig.cs doesn't compile as the rig compiles it (Add-Type in Windows PowerShell, exit code ${code}):")
        foreach ($o in $output) { $Problems.Add("    " + $o) }
    }
    Write-Host ("Compiled .github/scripts/PanelRig.cs with Add-Type in Windows PowerShell: exit code {0}" -f $code)
}

if ($Problems.Count -gt 0) {
    foreach ($p in $Problems) { Write-Host "::error::$p" }
    if ($env:GITHUB_STEP_SUMMARY) {
        $summary = @(
            "### The rig failed before any case ran: its scripts don't parse or compile"
            ""
            "Checked with $Edition. Nothing about the panel was tried or judged."
            ""
            '```'
        ) + @($Problems) + @('```')
        $summary | Add-Content -LiteralPath $env:GITHUB_STEP_SUMMARY -Encoding UTF8
    }
    exit 1
}
Write-Host "The rig's scripts parse$(if ($CompileRig) { ' and its C# compiles' })."
