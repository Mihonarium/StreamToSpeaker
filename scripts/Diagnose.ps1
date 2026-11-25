# Stream To Speaker: a report of how its sound driver and sound output are
# set up on this PC, to paste into a bug report.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File "C:\Program Files\Stream To Speaker\scripts\Diagnose.ps1"
#
# It changes nothing and needs no administrator rights (run elevated, it can
# also read Windows' driver log). Account names, the computer's name, user
# folders and speaker names are replaced by placeholders such as <account>,
# and it prints nothing about networks (no network names, no addresses), so
# the output can be posted in public. Read it before you post it.
#
# Keep this file ASCII. Windows PowerShell reads a script without a
# byte-order mark in the system's ANSI code page; in the single-byte
# Windows code pages a UTF-8 dash turns into three characters, one of them
# a curly quote that ends a string early, and the whole script fails to
# parse.

$ErrorActionPreference = 'Continue'

$DiagHardwareId = 'Root\StreamToSpeaker'
$DiagAppId = '{8A6C7F92-3E1B-4A2B-9F1C-77F1B6B2A6E0}_is1'
$DiagArrow = [string][char]0x2192

# ---------------------------------------------------------------------------
# Keeping private details out of the report
# ---------------------------------------------------------------------------

# The names on this PC that identify a person or the machine, with the
# placeholder each is replaced by.
function Get-PrivateNames {
    $names = [ordered]@{}
    foreach ($pair in @(
            @($env:USERNAME, '<account>'),
            @($env:COMPUTERNAME, '<computer>'),
            @($env:USERDOMAIN, '<domain>'),
            @($env:USERDNSDOMAIN, '<domain>'),
            @($env:LOGONSERVER, '<server>'))) {
        $value = "$($pair[0])".Trim().TrimStart('\')
        if ($value.Length -ge 2 -and -not $names.Contains($value)) { $names[$value] = $pair[1] }
    }
    try {
        $full = ([Security.Principal.WindowsIdentity]::GetCurrent().Name -split '\\')[-1]
        if ($full.Length -ge 2 -and -not $names.Contains($full)) { $names[$full] = '<account>' }
    } catch { }
    $names
}

# A line as it may be shown in public: no account, computer or domain name,
# no user folder, no speaker name, no network address.
function ConvertTo-PublicText {
    param([string]$Text, $PrivateNames = [ordered]@{}, [string]$UserProfile = $env:USERPROFILE)
    $t = "$Text"
    if ($UserProfile -and $UserProfile.Length -ge 4) {
        $t = [regex]::Replace($t, [regex]::Escape($UserProfile), '<user folder>', 'IgnoreCase')
    }
    # Any user's folder, in any form Windows writes it.
    $t = [regex]::Replace($t, '(?i)(\\Users\\|\\Documents and Settings\\)[^\\/:*?"<>|\r\n]+', '$1<user>')
    foreach ($name in $PrivateNames.Keys) {
        $t = [regex]::Replace($t, '(?<![\w-])' + [regex]::Escape($name) + '(?![\w-])', $PrivateNames[$name], 'IgnoreCase')
    }
    # The app names its sound output after the speaker it plays on.
    # The name ends where a quote closes it, or at the end of the line.
    $t = [regex]::Replace($t, '(Stream To Speaker ' + [regex]::Escape($DiagArrow) + ' ).*?(?=''[):,]|''\s*$|"|$)', '$1<speaker>')
    # Addresses: IPv4 in the ranges home and office networks use, IPv6, and
    # hardware (MAC) addresses.
    $t = [regex]::Replace($t, '\b(10|127|169\.254|192\.168|172\.(1[6-9]|2[0-9]|3[01]))(\.\d{1,3}){2,3}\b', '<address>')
    $t = [regex]::Replace($t, '(?i)(?<![\w:])(?:fe80|f[cd][0-9a-f]{2}|2[0-9a-f]{3}):[0-9a-f:]*[0-9a-f](?:%\d+)?(?![\w:])', '<address>')
    $t = [regex]::Replace($t, '(?i)\b(?:[0-9a-f]{2}[:-]){5}[0-9a-f]{2}\b', '<address>')
    $t
}

# PowerShell writes who ran it, on which computer, at the top of every
# transcript; those lines are left out of the report.
function Test-TranscriptHeaderLine([string]$Line) {
    $Line -match '^\s*(Username|RunAs User|Machine|Host Application|Process ID|Configuration Name)\s*:'
}

# ---------------------------------------------------------------------------
# Reading the PC
# ---------------------------------------------------------------------------

function Test-TestSigningOn([string]$SystemStartOptions) {
    @("$SystemStartOptions" -split '\s+' | Where-Object { $_ -ieq 'TESTSIGNING' }).Count -gt 0
}

function ConvertTo-WindowsPath([string]$Path) {
    if (-not $Path) { return '' }
    $p = $Path.Trim().Trim('"')
    $win = $env:windir.TrimEnd('\')
    if ($p.StartsWith('\??\') -or $p.StartsWith('\\?\')) { return $p.Substring(4) }
    if ($p -match '^\\SystemRoot\\(.*)$') { return "$win\$($Matches[1])" }
    if ($p -match '^%SystemRoot%\\(.*)$') { return "$win\$($Matches[1])" }
    if ($p -match '^System32\\') { return "$win\$p" }
    $p
}

function Get-ProblemText([int]$Code) {
    switch ($Code) {
        0 { 'working'; break }
        10 { 'cannot start'; break }
        14 { 'needs a restart'; break }
        22 { 'turned off in Device Manager'; break }
        28 { 'no driver installed'; break }
        31 { 'not working properly'; break }
        38 { 'a previous copy of the driver is still loaded: restart Windows'; break }
        39 { 'driver could not be loaded'; break }
        48 { 'driver blocked by a policy'; break }
        52 { 'Windows did not accept the driver''s signature'; break }
        default { "problem code $Code" }
    }
}

function Get-InstallDirectory {
    $here = Split-Path -Parent $PSScriptRoot
    if ($here -and (Test-Path -LiteralPath (Join-Path $here 'stream-to-speaker.exe'))) { return $here }
    try {
        $hklm = [Microsoft.Win32.RegistryKey]::OpenBaseKey('LocalMachine', 'Registry64')
        $entry = $hklm.OpenSubKey("SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\$DiagAppId")
        if ($entry) {
            $location = [string]$entry.GetValue('InstallLocation')
            $entry.Close()
            if ($location) { return $location.TrimEnd('\') }
        }
        $hklm.Close()
    } catch { }
    ''
}

function Write-Section([string]$Title) {
    ''
    "=== $Title ==="
}

function Get-Report {
    $appDir = Get-InstallDirectory
    $elevated = (New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())).IsInRole(
        [Security.Principal.WindowsBuiltInRole]::Administrator)

    Write-Section 'About this report'
    'Made by Diagnose.ps1. Account, computer and speaker names and user folders are replaced by'
    'placeholders such as <account>; nothing about networks is included. Read it before posting it.'
    "Run as administrator: $(if ($elevated) { 'yes' } else { 'no (Windows'' driver log is left out)' })"

    Write-Section 'Windows'
    $os = Get-CimInstance Win32_OperatingSystem -ErrorAction SilentlyContinue
    $nt = Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion' -ErrorAction SilentlyContinue
    "Edition    : $($os.Caption) ($(if (-not $os) { 'unknown' } elseif ($os.ProductType -eq 1) { 'desktop' } else { 'server: the driver does not run on Windows Server' }))"
    "Version    : $($nt.DisplayVersion), build $($nt.CurrentBuildNumber).$($nt.UBR)"
    "64-bit     : $([Environment]::Is64BitOperatingSystem)"

    Write-Section 'Build and test signing'
    $cer = if ($appDir) { Join-Path $appDir 'driver\StreamToSpeaker.cer' } else { '' }
    $testBuild = $cer -and (Test-Path -LiteralPath $cer)
    $options = (Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control' -Name SystemStartOptions -ErrorAction SilentlyContinue).SystemStartOptions
    $testSigning = Test-TestSigningOn $options
    $secureBoot = (Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\SecureBoot\State' -Name UEFISecureBootEnabled -ErrorAction SilentlyContinue).UEFISecureBootEnabled
    "Installed in : $(if ($appDir) { $appDir } else { '(Stream To Speaker is not installed)' })"
    "Build        : $(if ($testBuild) { 'test build (its driver carries a test signature)' } else { 'release (Microsoft-signed driver)' })"
    "Test signing : $(if ($testSigning) { 'on' } else { 'off' })$(if ($testBuild -and -not $testSigning) { ' - a test build''s driver does not start like this' })"
    "Secure Boot  : $(if ($null -eq $secureBoot) { 'unknown' } elseif ($secureBoot -eq 1) { 'on' } else { 'off' })"

    Write-Section 'Driver'
    $svc = Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Services\StreamToSpeaker' -ErrorAction SilentlyContinue
    if (-not $svc -or -not $svc.ImagePath) {
        'Service      : not installed'
    } else {
        $image = ConvertTo-WindowsPath $svc.ImagePath
        "Driver file  : $image"
        if (Test-Path -LiteralPath $image) {
            $item = Get-Item -LiteralPath $image
            "Version      : $($item.VersionInfo.FileVersion), $($item.Length) bytes, modified $($item.LastWriteTime.ToString('yyyy-MM-dd HH:mm'))"
            $sig = Get-AuthenticodeSignature -LiteralPath $image -ErrorAction SilentlyContinue
            "Signature    : $($sig.Status)$(if ($sig.SignerCertificate) { ', ' + $sig.SignerCertificate.Subject })"
        } else {
            'Version      : the file is missing (its driver package was deleted)'
        }
        $running = Get-CimInstance Win32_SystemDriver -Filter "Name='StreamToSpeaker'" -ErrorAction SilentlyContinue
        "Loaded       : $(if ($running -and $running.State -eq 'Running') { 'yes' } else { 'no' })"
    }
    $packages = @(Get-ChildItem -LiteralPath (Join-Path $env:windir 'INF') -Filter 'oem*.inf' -File -ErrorAction SilentlyContinue |
        Where-Object { Select-String -LiteralPath $_.FullName -Pattern $DiagHardwareId -SimpleMatch -Quiet -ErrorAction SilentlyContinue })
    if (-not $packages.Count) { 'Driver store : no Stream To Speaker package' }
    foreach ($p in $packages) {
        $ver = Select-String -LiteralPath $p.FullName -Pattern '^\s*DriverVer\s*=' | Select-Object -First 1
        "Driver store : $($p.Name), $(if ($ver) { ($ver.Line -split '=', 2)[1].Trim() } else { 'no DriverVer' })"
    }

    Write-Section 'Device'
    $devices = @(Get-CimInstance Win32_PnPEntity -ErrorAction SilentlyContinue |
        Where-Object { $_.HardwareID -and (@($_.HardwareID) -contains $DiagHardwareId) })
    if (-not $devices.Count) { 'No Stream To Speaker device. Run Setup again to create it.' }
    foreach ($d in $devices) {
        "$($d.PNPDeviceID): $(Get-ProblemText ([int]$d.ConfigManagerErrorCode))"
    }
    if ($devices.Count -gt 1) { 'More than one device: Windows shows a Stream To Speaker output for each, and only one plays.' }

    Write-Section 'Sound output in Windows'
    $base = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render'
    $states = @{ 1 = 'on'; 2 = 'turned off in Sound settings'; 4 = 'not present (its device is gone)'; 8 = 'unplugged' }
    $found = 0
    foreach ($key in @(Get-ChildItem -LiteralPath $base -ErrorAction SilentlyContinue)) {
        $props = Get-ItemProperty -LiteralPath (Join-Path $key.PSPath 'Properties') -ErrorAction SilentlyContinue
        if (-not $props) { continue }
        $fname = $props.'{a45c254e-df1c-4efd-8020-67d146a850e0},14'
        $desc = $props.'{a45c254e-df1c-4efd-8020-67d146a850e0},2'
        $iface = $props.'{b3f8fa53-0004-438e-9003-51a46e139bfc},6'
        if (-not (@($fname, $desc, $iface) | Where-Object { $_ -and "$_".IndexOf('Stream To Speaker', [StringComparison]::OrdinalIgnoreCase) -ge 0 })) { continue }
        $found++
        $state = (Get-ItemProperty -LiteralPath $key.PSPath -Name DeviceState -ErrorAction SilentlyContinue).DeviceState
        $low = if ($null -ne $state) { ([int64]$state) -band 0xF } else { 0 }
        ''
        "Output       : $($key.PSChildName)"
        "  State      : $(if ($states.ContainsKey([int]$low)) { $states[[int]$low] } else { "0x{0:X}" -f $state })"
        "  Name       : $fname"
        "  Device     : $($props.'{b3f8fa53-0004-438e-9003-51a46e139bfc},2')"
        "  Form factor: $($props.'{1da5d803-d492-4edd-8c23-e0c0ffee7f0e},0') (1 = speakers)"
        "  Association: $($props.'{1da5d803-d492-4edd-8c23-e0c0ffee7f0e},2')"
        $enable = $props.'{f3e80bef-1723-4ff2-bcc4-7f83dc5e46d4},4'
        "  On by default: $(if ($null -ne $enable) { '0x{0:X8}' -f [int64]$enable } else { 'not set' }) (0x00000101 = yes)"
    }
    if (-not $found) { 'Windows has no Stream To Speaker sound output.' }

    Write-Section 'Windows Firewall'
    $exe = if ($appDir) { Join-Path $appDir 'stream-to-speaker.exe' } else { '' }
    if ($exe) {
        $rules = @(Get-NetFirewallApplicationFilter -ErrorAction SilentlyContinue |
            Where-Object { $_.Program -and ([Environment]::ExpandEnvironmentVariables($_.Program).Trim('"') -ieq $exe) } |
            Get-NetFirewallRule -ErrorAction SilentlyContinue)
        if (-not $rules.Count) { 'No rule for the app: Windows asks at its first start, and speakers may not reach it.' }
        foreach ($r in $rules) {
            "$($r.DisplayName): $($r.Direction) $($r.Action), $(if ($r.Enabled -eq 'True') { 'on' } else { 'off' }), networks: $($r.Profile)"
        }
        if (@($rules | Where-Object { $_.Action -eq 'Block' -and $_.Direction -eq 'Inbound' -and $_.Enabled -eq 'True' }).Count) {
            'A rule blocks the app: speakers on those networks cannot fetch its sound.'
        }
    }
    foreach ($c in @(Get-NetConnectionProfile -ErrorAction SilentlyContinue)) {
        "Network in use: $($c.NetworkCategory) (rules for 'Private' apply to private networks)"
    }

    Write-Section 'Install log (last 80 lines)'
    $log = if ($appDir) { Join-Path $appDir 'logs\install.log' } else { '' }
    if ($log -and (Test-Path -LiteralPath $log)) {
        @(Get-Content -LiteralPath $log -ErrorAction SilentlyContinue | Where-Object { -not (Test-TranscriptHeaderLine $_) }) |
            Select-Object -Last 80 | ForEach-Object { "  $_" }
    } else {
        "  No install log at $log."
        # Only named, never read: that folder is in a user profile.
        '  (Installs by earlier versions kept it in %LOCALAPPDATA%\StreamToSpeaker of the account that approved the install.)'
    }

    Write-Section "Windows' driver log (last Stream To Speaker lines)"
    $setupapi = Join-Path $env:windir 'INF\setupapi.dev.log'
    if (-not $elevated) {
        '  Needs administrator rights.'
    } elseif (Test-Path -LiteralPath $setupapi) {
        Select-String -LiteralPath $setupapi -Pattern 'StreamToSpeaker' -SimpleMatch -ErrorAction SilentlyContinue |
            Select-Object -Last 15 | ForEach-Object { "  $($_.Line)" }
    } else {
        '  Not found.'
    }

    Write-Section 'What it means'
    @'
Read from the top; the first line that fits is the one that matters.

- Edition says server: the driver doesn't run on Windows Server. Use Windows 10 or 11.
- Build is a test build and test signing is off: its driver can't start. Install the
  release from https://github.com/Mihonarium/StreamToSpeaker/releases/latest instead.
- No device, or "Service: not installed": the driver isn't installed. Run Setup again.
- The device shows a problem: "needs a restart" or "a previous copy ... is still loaded"
  -> restart Windows. "turned off in Device Manager" -> turn it on there. Anything else:
  restart Windows and run Setup again.
- Sound output "turned off in Sound settings": open Sound settings, choose Stream To
  Speaker and allow it (or enable it).
- No sound output although the device works: restart Windows. If it's still missing,
  run Reset-Install.ps1 (below), restart, and run Setup again.
- A firewall rule blocks the app, or the network in use is Public: speakers can't reach
  it. Make the network private (Settings > Network & internet), or remove the blocking rule
  in Windows Defender Firewall.

Commands, each from an elevated PowerShell (Run as administrator):
'@
    $scripts = if ($appDir) { Join-Path $appDir 'scripts' } else { 'C:\Program Files\Stream To Speaker\scripts' }
    "  Name the output again:  powershell -NoProfile -ExecutionPolicy Bypass -File `"$scripts\Rename-Endpoint.ps1`""
    "  Clear the driver:       powershell -NoProfile -ExecutionPolicy Bypass -File `"$scripts\Reset-Install.ps1`""
}

if (-not (Get-Variable -Name DiagnoseLoadOnly -ValueOnly -ErrorAction SilentlyContinue)) {
    # 32-bit PowerShell sees another registry and System32: run the 64-bit one.
    if ([Environment]::Is64BitOperatingSystem -and -not [Environment]::Is64BitProcess) {
        $native = Join-Path $env:windir 'sysnative\WindowsPowerShell\v1.0\powershell.exe'
        & $native -NoProfile -ExecutionPolicy Bypass -File $PSCommandPath
        exit $LASTEXITCODE
    }
    $private = Get-PrivateNames
    Get-Report | ForEach-Object { ConvertTo-PublicText -Text $_ -PrivateNames $private }
}
