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

Test 'a name joined to others by a dash or an underscore is still replaced' {
    Assert-Equal 'hotspot <computer>-5G' (Public 'hotspot LAPTOP-JANE-5G') 'dash after'
    Assert-Equal 'DIRECT-xy-<computer>' (Public 'DIRECT-xy-LAPTOP-JANE') 'dash before'
    Assert-Equal '<computer>_2' (Public 'LAPTOP-JANE_2') 'underscore'
}

Test 'the names to replace include the short form of a long name and the organisation''s domain' {
    $n = Get-PrivateNames -UserName 'Jane Smith' -ComputerName 'LAPTOP-JANE' -UserDomain 'SMITHS' -DnsDomain 'CORP.CONTOSO.COM' -LogonServer '\\DC01' -Identity 'SMITHS\Jane Smith'
    Assert-Equal '<account>' $n['JANESM~'] 'short form of the account'
    Assert-Equal '<domain>' $n['CONTOSO.COM'] 'the organisation''s domain'
    Assert-Equal '<server>' $n['DC01'] 'the sign-in server'
    $names2 = $n
    Assert-Equal 'D:\<account>\Stream To Speaker' (ConvertTo-PublicText -Text 'D:\JANESM~1\Stream To Speaker' -PrivateNames $names2 -UserProfile '') 'short form in a path'
    Assert-Equal 'mail <e-mail>, site <domain>' (ConvertTo-PublicText -Text 'mail jane.smith@corp.contoso.com, site contoso.com' -PrivateNames $names2 -UserProfile '') 'e-mail and domain'
}

Test 'servers and shares, a work OneDrive''s organisation and account IDs are replaced' {
    Assert-Equal '\\<server>\<share>\StreamToSpeaker\x.exe' (Public '\\NAS-SMITH\apps\StreamToSpeaker\x.exe') 'network path'
    Assert-Equal '\\?\C:\Windows\x.sys and \\.\pipe\x' (Public '\\?\C:\Windows\x.sys and \\.\pipe\x') 'paths on this PC stay'
    Assert-Equal 'C:\Users\<user>\OneDrive - <organisation>\x' (Public 'C:\Users\Bob\OneDrive - Contoso Ltd\x') 'OneDrive'
    Assert-Equal 'owner <account id> and S-1-5-18' (Public 'owner S-1-5-21-1004336348-1177238915-682003330-1001 and S-1-5-18') 'SID'
}

Test 'every IPv4 address but 127.0.0.1 is replaced, and IPv6 and MACs in every form' {
    Assert-Equal 'public <address> cgnat <address> loopback 127.0.0.1' (Public 'public 81.2.69.160 cgnat 100.64.1.2 loopback 127.0.0.1') 'IPv4'
    Assert-Equal 'to <address>.' (Public 'to 10.0.0.7.') 'at the end of a sentence'
    Assert-Equal 'mapped <address> prefix <address> global <address>' (Public 'mapped ::ffff:203.0.113.9 prefix 2a02:1810:: global 3ffe:1900:4545:3:200:f8ff:fe21:67cf') 'IPv6'
    Assert-Equal 'cisco <address> sonos RINCON_<address>' (Public 'cisco a483.e712.3456 sonos RINCON_A483E7123456') 'MAC'
    Assert-Equal 'Version      : 1.1.1.211, 41984 bytes' (Public 'Version      : 1.1.1.211, 41984 bytes') 'a file version'
    Assert-Equal '[StreamToSpeaker.SetupNative]::SetDefault and [IO.Directory]::Delete' (Public '[StreamToSpeaker.SetupNative]::SetDefault and [IO.Directory]::Delete') 'PowerShell'
    Assert-Equal 'thumbprint 3C9A1F0E7B2D4C6A8E0F1A2B3C4D5E6F7A8B9C0D' (Public 'thumbprint 3C9A1F0E7B2D4C6A8E0F1A2B3C4D5E6F7A8B9C0D') 'a thumbprint'
}

Test 'a speaker name with a double quote in it is replaced whole' {
    Assert-Equal "Name       : Stream To Speaker $arrow <speaker>" (Public "Name       : Stream To Speaker $arrow 55`" TV") 'quote'
}

Test 'a name chosen for the output in Sound settings isn''t shown' {
    Assert-Equal '(a name chosen in Sound settings)' (Format-OutputName 'Jane''s speakers') 'chosen'
    Assert-Equal "Stream To Speaker $arrow Kitchen" (Format-OutputName "Stream To Speaker $arrow Kitchen") 'the app''s'
}

Test 'the blocks PowerShell writes at a transcript''s start and end are left out, in any language' {
    $lines = @('**********************', 'Debut de la transcription de Windows PowerShell', 'Nom d''utilisateur : SMITHS\Admin', '**********************',
               '[install-driver] Plan: Keep', '**********************', 'Fin de la transcription', '**********************')
    Assert-Equal '[install-driver] Plan: Keep' ((Select-ReportLogLines $lines) -join '|') 'kept lines'
}

Test 'the report asks to be read through before it is posted' {
    Assert-Equal $true ($DiagReadFirst.StartsWith('Read the report through before you post it.')) 'the sentence'
    Assert-Equal $true ($DiagReadFirst.Contains('networks') -and $DiagReadFirst.Contains('addresses')) 'what to look for'
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
