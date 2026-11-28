# Stream To Speaker: a report of how its sound driver and sound output are
# set up on this PC, to paste into a bug report.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File "C:\Program Files\Stream To Speaker\scripts\Diagnose.ps1"
#
# It changes nothing and needs no administrator rights (run elevated, it can
# also read Windows' driver log). Account, computer, domain and speaker
# names, user folders, servers and shares, account IDs, e-mail addresses and
# network and hardware addresses are replaced by placeholders such as
# <account>; of the network it prints only whether it is public or private.
# No list of patterns catches every name, so the report asks, first and
# last, to be read through before it is posted.
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

# The short (8.3) name Windows gives a long file or folder name, without
# the number after its "~": spaces go, and every dot but the last; of the
# part before the last dot the first six characters stay, of the part after
# it the first three; + , ; = [ ] become _ (a dash or an underscore stays).
# "Ann-Marie" gives ANN-MA~ (ANN-MA~1), "jane.smith" JANE~.SMI (JANE~1.SMI).
function Get-ShortNameForm([string]$Name) {
    $n = "$Name".ToUpperInvariant() -replace ' ', ''
    $dot = $n.LastIndexOf('.')
    $base = if ($dot -gt 0) { $n.Substring(0, $dot) } else { $n }
    $ext = if ($dot -gt 0) { $n.Substring($dot + 1) } else { '' }
    $base = ($base -replace '\.', '') -replace '[+,;=\[\]]', '_'
    $ext = $ext -replace '[+,;=\[\]]', '_'
    if ($base.Length -gt 6) { $base = $base.Substring(0, 6) }
    if ($ext.Length -gt 3) { $ext = $ext.Substring(0, 3) }
    if ($base.Length -lt 2) { return '' }
    if ($ext) { "$base~.$ext" } else { "$base~" }
}

# The names on this PC that identify a person or the machine, with the
# placeholder each is replaced by: as they are, and in the short (8.3) form
# Windows gives a long folder name.
function Get-PrivateNames {
    param(
        [string]$UserName = $env:USERNAME,
        [string]$ComputerName = $env:COMPUTERNAME,
        [string]$UserDomain = $env:USERDOMAIN,
        [string]$DnsDomain = $env:USERDNSDOMAIN,
        [string]$LogonServer = $env:LOGONSERVER,
        [string]$Identity = $(try { [Security.Principal.WindowsIdentity]::GetCurrent().Name } catch { '' })
    )
    $pairs = @(
        @($UserName, '<account>'),
        @((("$Identity" -split '\\')[-1]), '<account>'),
        @($ComputerName, '<computer>'),
        @($UserDomain, '<domain>'),
        @($DnsDomain, '<domain>'),
        @($LogonServer, '<server>'))
    # The organisation's own domain too: CORP.CONTOSO.COM also gives CONTOSO.COM.
    $labels = @("$DnsDomain".Trim('.') -split '\.' | Where-Object { $_ })
    for ($i = 1; $i -le $labels.Count - 2; $i++) { $pairs += , @(($labels[$i..($labels.Count - 1)] -join '.'), '<domain>') }
    $names = [ordered]@{}
    foreach ($pair in $pairs) {
        $value = "$($pair[0])".Trim().TrimStart('\')
        if ($value.Length -ge 2 -and -not $names.Contains($value)) { $names[$value] = $pair[1] }
    }
    foreach ($value in @($names.Keys)) {
        $form = Get-ShortNameForm $value
        if ($form -and ($value -match '[^A-Za-z0-9]' -or $value.Length -gt 8) -and -not $names.Contains($form)) {
            $names[$form] = $names[$value]
        }
    }
    $names
}

# A line as it may be shown in public: no account, computer or domain name,
# no user folder, server or share, no speaker name, no account ID, no e-mail,
# network or hardware address.
function ConvertTo-PublicText {
    param([string]$Text, $PrivateNames = [ordered]@{}, [string]$UserProfile = $env:USERPROFILE)
    $t = "$Text"
    # Upper and lower case are matched the same way whatever language
    # Windows uses (in Turkish, "I" and "i" are otherwise different letters).
    $anyCase = [System.Text.RegularExpressions.RegexOptions]'IgnoreCase, CultureInvariant'
    if ($UserProfile -and $UserProfile.Length -ge 4) {
        $t = [regex]::Replace($t, [regex]::Escape($UserProfile), '<user folder>', $anyCase)
    }
    # Any user's folder, in any form Windows writes it, and the organisation
    # in a work OneDrive's folder name.
    $t = [regex]::Replace($t, '(\\Users\\|\\Documents and Settings\\)[^\\/:*?"<>|\r\n]+', '$1<user>', $anyCase)
    $t = [regex]::Replace($t, '(OneDrive - )[^\\/:*?"<>|\r\n]+', '$1<organisation>', $anyCase)
    # A network path's server and share (not \\?\ and \\.\, which are this PC's).
    $t = [regex]::Replace($t, '(?<![\\\w])\\\\(?![?.]\\)[^\\/:*?"<>|\s]+\\[^\\/:*?"<>|\r\n]+', '\\<server>\<share>')
    # E-mail addresses, before the names they may contain.
    $t = [regex]::Replace($t, '[\w.+-]+@[\w-]+(?:\.[\w-]+)+', '<e-mail>')
    # A name joined to other letters or digits is another word; joined by a
    # space, a dash, an underscore or anything else, it is the name. A short
    # form (Get-ShortNameForm) has its number after the "~". A placeholder
    # that is already in the line stays as it is, also when an account is
    # called "user" or "server".
    $placeholders = '(?<placeholder><(?:account|computer|domain|server|user folder|user|organisation|share|e-mail|account id|speaker|address)>)'
    foreach ($name in $PrivateNames.Keys) {
        $tilde = if ($name.EndsWith('~') -or $name.Contains('~.')) { $name.LastIndexOf('~') } else { -1 }
        $body = if ($tilde -ge 0) {
            [regex]::Escape($name.Substring(0, $tilde)) + '~\d+' + [regex]::Escape($name.Substring($tilde + 1))
        } else {
            [regex]::Escape($name)
        }
        $with = $PrivateNames[$name]
        $replace = [System.Text.RegularExpressions.MatchEvaluator] {
            param($m)
            if ($m.Groups['placeholder'].Success) { $m.Value } else { $with }
        }
        $t = [regex]::Replace($t, $placeholders + '|(?<![\p{L}\p{N}])' + $body + '(?![\p{L}\p{N}])', $replace, $anyCase)
    }
    # Account IDs of a person: local and domain accounts, and work or school
    # (Microsoft Entra) accounts.
    $t = [regex]::Replace($t, '(?i)\bS-1-(?:5-21|12-1)(?:-\d+)+\b', '<account id>')
    # The app names its sound output after the speaker it plays on.
    # The name ends where a single quote closes it, or at the end of the line.
    $t = [regex]::Replace($t, '(Stream To Speaker ' + [regex]::Escape($DiagArrow) + ' ).*?(?=''[):,]|''\s*$|$)', '$1<speaker>')
    # Hardware (MAC) addresses: with colons or dashes, dotted, and as 12 hex
    # digits in a row (not the last part of a GUID).
    $t = [regex]::Replace($t, '(?i)\b(?:[0-9a-f]{2}[:-]){5}[0-9a-f]{2}\b', '<address>')
    $t = [regex]::Replace($t, '(?i)\b[0-9a-f]{4}\.[0-9a-f]{4}\.[0-9a-f]{4}\b', '<address>')
    $t = [regex]::Replace($t, '(?i)(?<![0-9a-f-])[0-9a-f]{12}(?![0-9a-f])', '<address>')
    # IPv6: anything with "::" or with three or more groups (not a time, and
    # not what follows a "]", which is PowerShell such as [Type]::Add).
    $t = [regex]::Replace($t, '(?i)(?<![\w:.\]])(?=[0-9a-f:.]*::|(?:[0-9a-f]{1,4}:){3})(?:[0-9a-f]{0,4}:){2,7}(?:\d{1,3}(?:\.\d{1,3}){3}|[0-9a-f]{1,4})?(?:%[\w.-]+)?(?![\w:])', '<address>')
    # IPv4, every address but this PC's own 127.0.0.1; a version number
    # (after "version" or after the comma of a DriverVer) stays.
    $t = [regex]::Replace($t, '(?<![\w.,])(?<!version\W{0,12})(?!127\.0\.0\.1(?![\d.]))(?:\d{1,3}\.){3}\d{1,3}(?!\w|\.\d)', '<address>', $anyCase)
    $t
}

# PowerShell writes who ran it, on which computer, at the top of every
# transcript; those lines are left out of the report.
function Test-TranscriptHeaderLine([string]$Line) {
    $Line -match '^\s*(Username|RunAs User|Machine|Host Application|Process ID|Configuration Name)\s*:'
}

# The lines of a transcript that go into the report: not the blocks between
# two lines of asterisks that PowerShell writes at a transcript's start and
# end (in whatever language Windows uses), nor the header lines above.
function Select-ReportLogLines([string[]]$Lines = @()) {
    $inHeader = $false
    foreach ($line in $Lines) {
        if ($line -match '^\*{10,}\s*$') {
            $inHeader = -not $inHeader
            continue
        }
        if ($inHeader -or (Test-TranscriptHeaderLine $line)) { continue }
        $line
    }
}

# Asked first and last in every report.
$DiagReadFirst = 'Read the report through before you post it. It replaces the names of accounts, computers, speakers and networks, and e-mail and network addresses, where it finds them; replace any it missed.'

# A sound output's name as the report shows it: the app's own names (whose
# speaker part is replaced later); a name chosen in Sound settings isn't shown.
function Format-OutputName([string]$Name) {
    if (-not $Name) { return '(none)' }
    if ($Name.IndexOf('Stream To Speaker', [StringComparison]::OrdinalIgnoreCase) -ge 0) { return $Name }
    '(a name chosen in Sound settings)'
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

    $DiagReadFirst
    Write-Section 'About this report'
    'Made by Diagnose.ps1. Names, user folders, account IDs and addresses are replaced by'
    'placeholders such as <account>; of the network, only whether it is public or private is shown.'
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
    "Build        : $(if (-not $appDir) { '(not installed)' } elseif ($testBuild) { 'test build (its driver carries a test signature)' } else { 'release (Microsoft-signed driver)' })"
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
            # A driver file may carry no version.
            $version = [string]$item.VersionInfo.FileVersion
            "Version      : $(if ($version) { $version } else { 'none' }), $($item.Length) bytes, modified $($item.LastWriteTime.ToString('yyyy-MM-dd HH:mm'))"
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
        "  Name       : $(Format-OutputName $fname)"
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
        @(Select-ReportLogLines @(Get-Content -LiteralPath $log -ErrorAction SilentlyContinue)) |
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
- No sound output although the device works: check that the Windows services
  "Windows Audio" and "Windows Audio Endpoint Builder" are running (in Services), then
  run Setup again, which makes a new device and output when Windows has none. If it's
  still missing, run Reset-Install.ps1 (below), restart, and run Setup again.
- A firewall rule blocks the app, or the network in use is Public: speakers can't reach
  it. Make the network private (Settings > Network & internet), or remove the blocking rule
  in Windows Defender Firewall.

Commands, each from an elevated PowerShell (Run as administrator):
'@
    $scripts = if ($appDir) { Join-Path $appDir 'scripts' } else { 'C:\Program Files\Stream To Speaker\scripts' }
    "  Name the output again:  powershell -NoProfile -ExecutionPolicy Bypass -File `"$scripts\Rename-Endpoint.ps1`""
    "  Clear the driver:       powershell -NoProfile -ExecutionPolicy Bypass -File `"$scripts\Reset-Install.ps1`""
    ''
    $DiagReadFirst
}

if (-not (Get-Variable -Name DiagnoseLoadOnly -ValueOnly -ErrorAction SilentlyContinue)) {
    # 32-bit PowerShell sees another registry and System32: run the 64-bit one.
    if ([Environment]::Is64BitOperatingSystem -and -not [Environment]::Is64BitProcess) {
        $native = Join-Path $env:windir 'sysnative\WindowsPowerShell\v1.0\powershell.exe'
        & $native -NoProfile -ExecutionPolicy Bypass -File $PSCommandPath
        exit $LASTEXITCODE
    }
    # Windows PowerShell's commands load from its own folder. Started by a
    # program that passes PowerShell 7's environment on (PowerShell 7's own
    # Start-Process does), this script inherits that version's module path,
    # and Windows PowerShell can't load the modules it finds there:
    # Get-AuthenticodeSignature then fails and the report has no signature.
    # The path is put back afterwards, for a PowerShell window the script
    # was run in.
    $modulePath = $env:PSModulePath
    if ($PSVersionTable.PSEdition -ne 'Core') { $env:PSModulePath = Join-Path $PSHOME 'Modules' }
    try {
        $private = Get-PrivateNames
        Get-Report | ForEach-Object { ConvertTo-PublicText -Text $_ -PrivateNames $private }
    } finally {
        $env:PSModulePath = $modulePath
    }
}
