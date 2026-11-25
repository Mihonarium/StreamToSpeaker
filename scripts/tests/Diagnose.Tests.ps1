# Tests that Diagnose.ps1's report keeps private details out, with made-up
# names, folders and addresses.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\tests\Diagnose.Tests.ps1
#
# Runs in Windows PowerShell 5.1. Loads Diagnose.ps1's functions without
# running the report. Exits with the number of failed tests.
#
# Keep this file ASCII.

$ErrorActionPreference = 'Continue'
$DiagnoseLoadOnly = $true
. ([ScriptBlock]::Create([IO.File]::ReadAllText((Join-Path (Split-Path -Parent $PSScriptRoot) 'Diagnose.ps1'))))

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
    if ("$Expected" -cne "$Actual") { throw "${What}: expected [$Expected], got [$Actual]" }
}

$arrow = [string][char]0x2192
$names = [ordered]@{ 'Jane Smith' = '<account>'; 'LAPTOP-JANE' = '<computer>'; 'SMITHS' = '<domain>' }
function Public([string]$Text) { ConvertTo-PublicText -Text $Text -PrivateNames $names -UserProfile 'C:\Users\Jane Smith' }

Test 'the lines PowerShell writes about who ran it are left out' {
    foreach ($line in 'Username: LAPTOP-JANE\Jane Smith', 'RunAs User: SMITHS\Jane Smith', 'Machine: LAPTOP-JANE (Microsoft Windows NT 10.0.22631.0)',
                      'Host Application: C:\WINDOWS\System32\WindowsPowerShell\v1.0\powershell.exe -File C:\Users\Jane Smith\x.ps1',
                      'Process ID: 4242', 'Configuration Name: ') {
        Assert-Equal $true (Test-TranscriptHeaderLine $line) "left out: $line"
    }
    Assert-Equal $false (Test-TranscriptHeaderLine '[install-driver] Plan: Keep') 'a line of the log'
}

Test 'account, computer and domain names are replaced' {
    Assert-Equal 'signed in as <domain>\<account> on <computer>' (Public 'signed in as SMITHS\Jane Smith on LAPTOP-JANE') 'names'
    Assert-Equal 'LAPTOP-JANET stays' (Public 'LAPTOP-JANET stays') 'a longer name that only starts the same'
}

Test 'user folders are replaced, whoever they belong to' {
    Assert-Equal '<user folder>\AppData\Local\Temp\x.cs' (Public 'C:\Users\Jane Smith\AppData\Local\Temp\x.cs') 'own profile'
    Assert-Equal 'D:\Users\<user>\Downloads\StreamToSpeakerSetup.exe' (Public 'D:\Users\Bob\Downloads\StreamToSpeakerSetup.exe') 'another profile'
    Assert-Equal '\??\C:\Users\<user>\x.inf' (Public '\??\C:\Users\Ann-Marie\x.inf') 'an NT path'
}

Test 'speaker names in the output''s name are replaced' {
    Assert-Equal "Name       : Stream To Speaker $arrow <speaker>" (Public "Name       : Stream To Speaker $arrow Jane's bedroom") 'speaker'
    Assert-Equal "('Stream To Speaker $arrow <speaker>')" (Public "('Stream To Speaker $arrow Kitchen')") 'quoted'
    Assert-Equal 'Stream To Speaker' (Public 'Stream To Speaker') 'no speaker'
}

Test 'network addresses are replaced; versions, times and GUIDs are kept' {
    Assert-Equal 'from <address> and <address>' (Public 'from 192.168.1.23 and 10.0.0.7') 'IPv4'
    Assert-Equal 'link <address>' (Public 'link fe80::1c2d:3e4f:5a6b:7c8d%12') 'IPv6'
    Assert-Equal 'mac <address>' (Public 'mac a4:83:e7:12:34:56') 'MAC'
    Assert-Equal 'DriverVer 05/22/2026,1.1.1.211' (Public 'DriverVer 05/22/2026,1.1.1.211') 'driver version'
    Assert-Equal 'build 10.0.22631.4460 at 23:54:12' (Public 'build 10.0.22631.4460 at 23:54:12') 'Windows build and time'
    Assert-Equal '{DFF21CE1-F70F-11D0-B917-00A0C9223196}' (Public '{DFF21CE1-F70F-11D0-B917-00A0C9223196}') 'GUID'
}

Test 'test signing is read from the boot options' {
    Assert-Equal $true (Test-TestSigningOn ' TESTSIGNING  NOEXECUTE=OPTIN') 'on'
    Assert-Equal $false (Test-TestSigningOn ' NOEXECUTE=OPTIN') 'off'
}

Test 'service paths become normal paths' {
    Assert-Equal "$($env:windir.TrimEnd('\'))\System32\DriverStore\FileRepository\a\StreamToSpeaker.sys" (ConvertTo-WindowsPath '\SystemRoot\System32\DriverStore\FileRepository\a\StreamToSpeaker.sys') 'SystemRoot'
}

Write-Host ''
Write-Host "$script:Passed passed, $($script:Failed.Count) failed"
foreach ($f in $script:Failed) { Write-Host "  failed: $f" }
exit $script:Failed.Count
