<#
.SYNOPSIS
    One phase of the installer lifecycle test (installer-lifecycle.yml).

.DESCRIPTION
    Runs an installer or the uninstaller silently and checks the machine
    state it should leave behind: installed files (driver files byte-equal
    to the build's driver record), the Apps & Features entry, shortcuts,
    the autostart value, the test certificate in the machine trust stores,
    the driver package in the driver store, the root-enumerated device and
    a user-data sentinel in %APPDATA%\StreamToSpeaker, which no phase may
    touch (uninstall leaves user data in place).

    In each setup run of the current build, Pre-Install.ps1 and
    Rename-Endpoint.ps1 must each add one run to
    <install dir>\logs\install.log, and nothing may change in
    %LOCALAPPDATA%\StreamToSpeaker, where earlier releases keep that log;
    Rename-Endpoint.ps1 must compile its COM wrapper in Windows' temp
    folder, not in the temp folder setup inherits from the user's profile,
    and load it. After the fresh install, Diagnose.ps1 must find the log.
    Each setup run's copy of the log goes with the uploaded logs, since
    uninstall deletes it.

    Hosted runners have no test-signing and run Windows Server, so the
    driver is not expected to *load*: the device's status is reported, not
    required. What is required is everything the installer itself controls.

    State shared between phases (sentinel hash, certificate thumbprint)
    lives in a JSON file under -WorkDir.

.PARAMETER Phase
    env       report test-signing / Secure Boot / OS; create the sentinel
    install   run -Installer on a clean machine
    repair    delete two installed files, re-run -Installer, expect them back
    uninstall run the registered uninstaller
    previous  run -Installer (an older release) with lighter file checks
    upgrade   run -Installer over the previous release
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateSet("env", "install", "repair", "uninstall", "previous", "upgrade")]
    [string]$Phase,
    # Installer to run (install / repair / previous / upgrade).
    [string]$Installer = "",
    # The installer's <name>.driver.json build record (current-build phases).
    [string]$DriverRecord = "",
    # DisplayVersion the Apps & Features entry must show afterwards.
    [string]$ExpectVersion = "",
    # The service exe the installer should have put in place (hash compare).
    [string]$ExpectExe = "",
    [string]$WorkDir = (Join-Path $env:RUNNER_TEMP "lifecycle")
)

$ErrorActionPreference = "Stop"

$AppName = "Stream To Speaker"
$ExeName = "stream-to-speaker.exe"
$HardwareId = "Root\StreamToSpeaker"
$UninstallKey = "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\{8A6C7F92-3E1B-4A2B-9F1C-77F1B6B2A6E0}_is1"
$UninstallKeyWow = "HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\{8A6C7F92-3E1B-4A2B-9F1C-77F1B6B2A6E0}_is1"
$DefaultAppDir = Join-Path $env:ProgramFiles $AppName
$StartMenuDir = Join-Path ([Environment]::GetFolderPath("CommonPrograms")) $AppName
$StartMenuLink = Join-Path $StartMenuDir "$AppName.lnk"
$DesktopLink = Join-Path ([Environment]::GetFolderPath("CommonDesktopDirectory")) "$AppName.lnk"
$RunKey = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run"
$ConfigDir = Join-Path $env:APPDATA "StreamToSpeaker"
$Sentinel = Join-Path $ConfigDir "lifecycle-sentinel.txt"
$LogDir = Join-Path $WorkDir "logs"
$StateFile = Join-Path $WorkDir "state.json"
# Where earlier releases keep install.log: the profile of the account that
# runs setup.
$ProfileLogDir = Join-Path $env:LOCALAPPDATA "StreamToSpeaker"
# Where Rename-Endpoint.ps1 compiles its IPolicyConfig wrapper: Windows' own
# temp folder, not the temp folder setup inherits from this process, which
# is in the profile of the account that runs it.
$WindowsTemp = Join-Path ([Environment]::GetFolderPath("Windows")) "Temp"
$ProfileTemp = [IO.Path]::GetTempPath()

New-Item -ItemType Directory -Force -Path $WorkDir, $LogDir | Out-Null

$script:Failures = @()
function Fail([string]$msg) {
    Write-Host "::error::[$Phase] $msg"
    $script:Failures += $msg
}
function Warn([string]$msg) { Write-Host "::warning::[$Phase] $msg" }
function Ok([string]$msg) { Write-Host "  ok: $msg" }
function Check([bool]$cond, [string]$msg) { if ($cond) { Ok $msg } else { Fail $msg } }

function Read-State {
    if (Test-Path $StateFile) { Get-Content $StateFile -Raw | ConvertFrom-Json -AsHashtable } else { @{} }
}
function Write-State($s) { $s | ConvertTo-Json | Set-Content $StateFile -Encoding utf8 }

function Get-Sha256([string]$path) { (Get-FileHash -Algorithm SHA256 -Path $path).Hash.ToLower() }

# Published names (oemNN.inf) of every package in the driver store that
# declares our hardware ID — the method installer/Uninstall-Driver.ps1 uses
# (pnputil's labels are localised, the INF copies are not).
function Get-DriverPackages {
    @(Get-ChildItem -Path (Join-Path $env:windir "INF") -Filter "oem*.inf" -ErrorAction SilentlyContinue |
        Where-Object { Select-String -Path $_.FullName -Pattern $HardwareId -SimpleMatch -Quiet -ErrorAction SilentlyContinue })
}

function Get-OurDevices {
    @(Get-CimInstance Win32_PnPEntity -ErrorAction SilentlyContinue |
        Where-Object { $_.HardwareID -and ($_.HardwareID -contains $HardwareId) })
}

function Get-DriverVer([string]$infPath) {
    $line = Select-String -Path $infPath -Pattern '^\s*DriverVer\s*=' | Select-Object -First 1
    if ($line) { ($line.Line -split "=", 2)[1].Trim() } else { "" }
}

function Get-AppDir {
    $k = Get-ItemProperty -Path $UninstallKey -ErrorAction SilentlyContinue
    if ($k -and $k.PSObject.Properties["InstallLocation"] -and $k.InstallLocation) {
        return $k.InstallLocation.TrimEnd("\")
    }
    $DefaultAppDir
}

function Assert-Sentinel {
    $s = Read-State
    if (-not $s.ContainsKey("sentinel")) { Fail "no sentinel recorded (env phase not run?)"; return }
    if (-not (Test-Path $Sentinel)) { Fail "user-data sentinel $Sentinel is gone"; return }
    Check ((Get-Sha256 $Sentinel) -eq $s.sentinel) "user-data sentinel unchanged"
}

function Invoke-Setup([string]$exe, [string]$label) {
    if (-not (Test-Path $exe)) { throw "installer not found: $exe" }
    $log = Join-Path $LogDir "$label-setup.log"
    Write-Host "Running $(Split-Path $exe -Leaf) silently (log: $log)"
    $p = Start-Process -FilePath $exe -PassThru -ArgumentList @(
        "/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/SP-", "/LOG=`"$log`"")
    # Touching the handle keeps it open, so ExitCode is still readable after
    # the process exits (otherwise it can come back $null).
    $null = $p.Handle
    if (-not $p.WaitForExit(15 * 60 * 1000)) {
        $p | Stop-Process -Force -ErrorAction SilentlyContinue
        throw "setup did not finish within 15 minutes (a dialog waiting for input?)"
    }
    Check ($p.ExitCode -eq 0) "setup exit code $($p.ExitCode)"
}

# A file's text; "" when it is missing or empty (Get-Content -Raw then
# returns nothing, and a [string] cast of nothing is $null).
function Read-Text([string]$path) {
    if (-not (Test-Path -LiteralPath $path)) { return "" }
    "$(Get-Content -LiteralPath $path -Raw)"
}

# The transcript Pre-Install.ps1 and Rename-Endpoint.ps1 append to.
function Get-InstallLog { Join-Path (Get-AppDir) "logs\install.log" }

# How many runs of Pre-Install.ps1 and Rename-Endpoint.ps1 the transcript
# records: each writes a "started" line as soon as its transcript is open.
function Get-TranscriptStarts {
    $text = Read-Text (Get-InstallLog)
    [ordered]@{
        "Pre-Install.ps1"     = [regex]::Matches($text, '==== Pre-Install started at ').Count
        "Rename-Endpoint.ps1" = [regex]::Matches($text, '==== Rename-Endpoint started at ').Count
    }
}

# What the last Rename-Endpoint.ps1 run in the transcript says about its
# IPolicyConfig wrapper: the folder it compiled it in ("" when that run
# logged none) and whether it then loaded it.
function Get-WrapperCompile {
    $text = Read-Text (Get-InstallLog)
    $start = $text.LastIndexOf("==== Rename-Endpoint started at ")
    $run = if ($start -ge 0) { $text.Substring($start) } else { "" }
    $m = [regex]::Match($run, '(?m)^Compiling the IPolicyConfig wrapper in (.*?)\r?$')
    @{
        folder = if ($m.Success) { $m.Groups[1].Value.Trim() } else { "" }
        loaded = $run.Contains("Loaded IPolicyConfig wrapper.")
    }
}

# Watches $dir and its subfolders for the compiler inputs Windows
# PowerShell's Add-Type writes into its temp folder, <random>.cmdline and
# <random>.0.cs, which it deletes again when the compile ends. A probe file
# shows that the watch sees such files.
function Start-CompileWatch([string]$dir) {
    $w = [System.IO.FileSystemWatcher]::new($dir)
    $w.Filters.Add("*.cmdline")
    $w.Filters.Add("*.0.cs")
    $w.IncludeSubdirectories = $true
    $w.InternalBufferSize = 64KB
    $null = Register-ObjectEvent -InputObject $w -EventName Created -SourceIdentifier "compile-watch"
    $null = Register-ObjectEvent -InputObject $w -EventName Error -SourceIdentifier "compile-watch-error"
    $w.EnableRaisingEvents = $true
    $probe = Join-Path $dir "lifecycle-compile-watch-probe.cmdline"
    Set-Content -LiteralPath $probe -Value "probe"
    Remove-Item -LiteralPath $probe
    @{ watcher = $w; probe = $probe }
}

# The compiler inputs the watch saw besides its probe; $null when it can't
# tell (it missed the probe or lost notifications).
function Stop-CompileWatch($watch) {
    Start-Sleep -Seconds 1
    $watch.watcher.EnableRaisingEvents = $false
    $seen = @(Get-Event -SourceIdentifier "compile-watch" -ErrorAction SilentlyContinue |
        ForEach-Object { $_.SourceEventArgs.FullPath })
    $lost = @(Get-Event -SourceIdentifier "compile-watch-error" -ErrorAction SilentlyContinue).Count -gt 0
    foreach ($id in "compile-watch", "compile-watch-error") {
        Unregister-Event -SourceIdentifier $id -ErrorAction SilentlyContinue
        Remove-Event -SourceIdentifier $id -ErrorAction SilentlyContinue
    }
    $watch.watcher.Dispose()
    if ($lost -or $seen -notcontains $watch.probe) { return $null }
    , @($seen | Where-Object { $_ -ne $watch.probe })
}

# Every file and folder under the old per-user log folder with its size and
# write time, to show that a setup run left it alone.
function Get-ProfileLogSnapshot {
    if (-not (Test-Path -LiteralPath $ProfileLogDir)) { return "(absent)" }
    @(Get-ChildItem -LiteralPath $ProfileLogDir -Recurse -Force |
        ForEach-Object { "$($_.FullName)|$($_.Length)|$($_.LastWriteTimeUtc.Ticks)" } |
        Sort-Object) -join "`n"
}

# Keeps a copy of the install-time transcript with the uploaded logs, since
# uninstall deletes it: from the install directory, or from the profile for
# a release that still logs there.
function Save-InstallLog([string]$label) {
    $copies = [ordered]@{
        "$label-install.log"         = Get-InstallLog
        "$label-install-profile.log" = Join-Path $ProfileLogDir "install.log"
    }
    foreach ($name in $copies.Keys) {
        if (Test-Path -LiteralPath $copies[$name]) {
            Copy-Item -LiteralPath $copies[$name] (Join-Path $LogDir $name) -Force
        }
    }
}

# Runs this build's installer. Pre-Install.ps1 and Rename-Endpoint.ps1
# must each add one run to <install dir>\logs\install.log, nothing may
# change in the per-user folder, and Rename-Endpoint.ps1 must compile its
# wrapper in Windows' temp folder and load it, with nothing compiled in the
# temp folder setup inherits: setup runs its scripts elevated, and the
# profile's folders are ones the user's own programs can redirect or write
# to.
function Invoke-CurrentSetup([string]$label) {
    $before = Get-TranscriptStarts
    $profileBefore = Get-ProfileLogSnapshot
    $watch = Start-CompileWatch $ProfileTemp
    $compiledInProfile = $null
    try {
        Invoke-Setup $Installer $label
    } finally {
        $compiledInProfile = Stop-CompileWatch $watch
    }
    Save-InstallLog $label
    $after = Get-TranscriptStarts
    foreach ($name in $before.Keys) {
        Check ($after[$name] -eq $before[$name] + 1) "$name logged to $(Get-InstallLog) (runs recorded: $($before[$name]) before setup, $($after[$name]) after)"
    }
    Check ((Get-ProfileLogSnapshot) -eq $profileBefore) "setup wrote nothing to $ProfileLogDir"
    $compile = Get-WrapperCompile
    Check ($compile.folder.TrimEnd("\") -eq $WindowsTemp) "Rename-Endpoint.ps1 compiled its IPolicyConfig wrapper in $WindowsTemp, outside the user's profile (logged: '$($compile.folder)')"
    Check $compile.loaded "Rename-Endpoint.ps1 loaded the wrapper it compiled"
    if ($null -eq $compiledInProfile) {
        Warn "could not watch $ProfileTemp during setup; whether anything was compiled there is unknown"
    } else {
        Check ($compiledInProfile.Count -eq 0) "nothing was compiled in $ProfileTemp during setup (compiler inputs seen: $($compiledInProfile -join ', '))"
    }
}

# Runs Diagnose.ps1 as users do (Windows PowerShell, elevated), from the
# install directory and as a copy in another folder (which finds the
# install through its Apps & Features entry, also from 32-bit Windows
# PowerShell, whose registry view doesn't show that entry by default): it
# must parse under Windows PowerShell and print the tail of
# <install dir>\logs\install.log.
function Assert-DiagnoseFindsInstallLog([string]$label) {
    $installed = Join-Path (Get-AppDir) "scripts\Diagnose.ps1"
    $copyDir = Join-Path $WorkDir "diagnose-copy"
    New-Item -ItemType Directory -Force -Path $copyDir | Out-Null
    Copy-Item -LiteralPath $installed $copyDir -Force
    $copy = Join-Path $copyDir "Diagnose.ps1"
    $ps = Join-Path $env:SystemRoot "System32\WindowsPowerShell\v1.0\powershell.exe"
    $ps32 = Join-Path $env:SystemRoot "SysWOW64\WindowsPowerShell\v1.0\powershell.exe"
    $runs = [ordered]@{
        "from the install directory"                        = $ps, $installed
        "from another folder"                               = $ps, $copy
        "from another folder, in 32-bit Windows PowerShell" = $ps32, $copy
    }
    $n = 0
    foreach ($where in $runs.Keys) {
        $n++
        $exe, $file = $runs[$where]
        $out = Join-Path $LogDir "$label-diagnose-$n.txt"
        $p = Start-Process -FilePath $exe -PassThru -NoNewWindow `
            -RedirectStandardOutput $out -RedirectStandardError "$out.stderr" -ArgumentList @(
            "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", "`"$file`"")
        $null = $p.Handle
        if (-not $p.WaitForExit(5 * 60 * 1000)) {
            $p | Stop-Process -Force -ErrorAction SilentlyContinue
            Fail "Diagnose.ps1 $where did not finish within 5 minutes"
            continue
        }
        $text = Read-Text $out
        Check ($p.ExitCode -eq 0) "Diagnose.ps1 $where ran under Windows PowerShell (exit code $($p.ExitCode))"
        Check ($text.Contains("==== Rename-Endpoint finished at ")) "Diagnose.ps1 $where printed the tail of $(Get-InstallLog)"
    }
    Remove-Item -LiteralPath $copyDir -Recurse -Force
}

function Invoke-Uninstall([string]$label) {
    $k = Get-ItemProperty -Path $UninstallKey -ErrorAction SilentlyContinue
    if (-not $k) { throw "no uninstall entry at $UninstallKey" }
    $exe = $k.UninstallString.Trim('"')
    $log = Join-Path $LogDir "$label-uninstall.log"
    Write-Host "Running $exe silently (log: $log)"
    $p = Start-Process -FilePath $exe -PassThru -ArgumentList @(
        "/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/LOG=`"$log`"")
    $null = $p.Handle
    if (-not $p.WaitForExit(15 * 60 * 1000)) {
        $p | Stop-Process -Force -ErrorAction SilentlyContinue
        throw "uninstaller did not finish within 15 minutes"
    }
    Check ($p.ExitCode -eq 0) "uninstaller exit code $($p.ExitCode)"
    # The uninstaller re-launches itself from %TEMP% and the first process
    # can return before the work is done: wait for its entry and exe to go.
    $deadline = (Get-Date).AddMinutes(5)
    while ((Test-Path $UninstallKey) -or (Test-Path $exe)) {
        if ((Get-Date) -gt $deadline) { Fail "uninstall still in progress after 5 minutes"; break }
        Start-Sleep -Seconds 2
    }
    # The empty install directory goes last.
    $deadline = (Get-Date).AddSeconds(15)
    while ((Test-Path $DefaultAppDir) -and (Get-Date) -lt $deadline) { Start-Sleep -Seconds 1 }
}

# Everything an install must leave behind. -Full adds the current build's
# expectations (exact payload list, driver files equal to the build record,
# test certificate trust).
function Assert-Installed([switch]$Full) {
    $k = Get-ItemProperty -Path $UninstallKey -ErrorAction SilentlyContinue
    Check ($null -ne $k) "Apps & Features entry present (64-bit view)"
    Check (-not (Test-Path $UninstallKeyWow)) "no 32-bit duplicate of the entry"
    if ($k) {
        Check ($k.DisplayName -like "$AppName*") "DisplayName '$($k.DisplayName)'"
        if ($ExpectVersion) {
            Check ($k.DisplayVersion -eq $ExpectVersion) "DisplayVersion '$($k.DisplayVersion)' (expected '$ExpectVersion')"
        }
    }
    $app = Get-AppDir
    Check ($app -eq $DefaultAppDir) "installed to the default directory ($app)"

    $required = @($ExeName, "unins000.exe", "unins000.dat", "driver\StreamToSpeaker.inf", "driver\StreamToSpeaker.sys")
    if ($Full) {
        $required += @(
            "driver\StreamToSpeaker.cat", "driver\devcon.exe",
            "scripts\Pre-Install.ps1", "scripts\Rename-Endpoint.ps1", "scripts\Reset-Install.ps1",
            "scripts\Diagnose.ps1", "scripts\Uninstall-Driver.ps1",
            "README.md", "LICENSE", "LICENSE-BINARIES.md", "LICENSE-INSTALLER.txt")
    }
    foreach ($f in $required) { Check (Test-Path (Join-Path $app $f)) "installed $f" }

    if ($ExpectExe) {
        Check ((Get-Sha256 (Join-Path $app $ExeName)) -eq (Get-Sha256 $ExpectExe)) "installed $ExeName is this build's"
    }

    $s = Read-State
    $rec = $null
    if ($DriverRecord) { $rec = Get-Content $DriverRecord -Raw | ConvertFrom-Json }
    if ($Full -and $rec) {
        foreach ($p in $rec.files.PSObject.Properties) {
            $installed = Join-Path $app "driver\$($p.Name)"
            if (Test-Path $installed) {
                Check ((Get-Sha256 $installed) -eq $p.Value) "driver\$($p.Name) matches the build record"
            } else {
                Fail "driver\$($p.Name) listed in the build record but not installed"
            }
        }
        $cer = Join-Path $app "driver\StreamToSpeaker.cer"
        if ($rec.test_cert) {
            Check (Test-Path $cer) "test certificate shipped (test-signed build)"
            if (Test-Path $cer) {
                $thumb = (New-Object System.Security.Cryptography.X509Certificates.X509Certificate2 $cer).Thumbprint
                $s.cert = $thumb
                Check (Test-Path "Cert:\LocalMachine\Root\$thumb") "test certificate in LocalMachine\Root"
                Check (Test-Path "Cert:\LocalMachine\TrustedPublisher\$thumb") "test certificate in LocalMachine\TrustedPublisher"
            }
        } elseif (Test-Path $cer) {
            # Inno doesn't delete files a newer package no longer ships, and
            # the .iss imports {app}\driver\StreamToSpeaker.cer whenever it
            # exists: a test certificate left by an earlier test-signed
            # install gets re-trusted by a production install.
            Warn "test certificate still at $cer after a production-driver install (left by the earlier test-signed install, and re-imported by this one)"
        } else {
            Ok "no test certificate (production driver)"
        }
    }

    Check (Test-Path $StartMenuLink) "Start menu shortcut (default task)"
    Check (-not (Test-Path $DesktopLink)) "no desktop shortcut (task off by default)"
    $run = Get-ItemProperty -Path $RunKey -Name $AppName -ErrorAction SilentlyContinue
    Check ($null -eq $run) "no sign-in autostart value (task off by default)"

    # Driver store + device. Loading the driver needs test-signing (test-
    # signed builds) or a client OS (Microsoft-signed client drivers),
    # neither of which a hosted runner has, so the device is reported, not
    # required to be healthy. Staging a test-signed package only needs its
    # certificate trusted, which the installer does, so it's required for
    # the test-signed build; whether this server OS accepts a package
    # signed for client Windows is reported only.
    $pkgs = @(Get-DriverPackages)
    Write-Host "  driver store: $(@($pkgs | ForEach-Object Name) -join ', ')"
    if ($Full -and $rec -and $rec.driver.kind -eq "test-signed") {
        Check ($pkgs.Count -eq 1) "exactly one driver package staged (found $($pkgs.Count))"
        if ($pkgs.Count -ge 1) {
            $want = Get-DriverVer (Join-Path $app "driver\StreamToSpeaker.inf")
            $have = @($pkgs | ForEach-Object { Get-DriverVer $_.FullName })
            Check ($have -contains $want) "staged package is this build's driver (DriverVer $want; store: $($have -join ', '))"
        }
    } elseif ($pkgs.Count -eq 0) {
        Warn "no driver package staged by this installer on this runner"
    } elseif ($pkgs.Count -gt 1) {
        Fail "$($pkgs.Count) driver packages staged; the install should replace, not add"
    }
    $devs = @(Get-OurDevices)
    Check ($devs.Count -le 1) "at most one $HardwareId device (found $($devs.Count))"
    foreach ($d in $devs) {
        Write-Host "  device $($d.PNPDeviceID): status=$($d.Status) problem=$($d.ConfigManagerErrorCode)"
    }
    if ($devs.Count -eq 0) { Warn "no $HardwareId device present after install" }
    $svc = Get-ItemProperty "HKLM:\SYSTEM\CurrentControlSet\Services\StreamToSpeaker" -ErrorAction SilentlyContinue
    Write-Host "  kernel service key: $(if ($svc) { "present (Start=$($svc.Start))" } else { 'absent' })"

    Write-State $s
}

function Assert-Removed {
    Check (-not (Test-Path $UninstallKey)) "Apps & Features entry removed"
    Check (-not (Test-Path $DefaultAppDir)) "install directory removed"
    if (Test-Path $DefaultAppDir) {
        Get-ChildItem -Recurse $DefaultAppDir | ForEach-Object { Write-Host "  left behind: $($_.FullName)" }
    }
    Check (-not (Test-Path $StartMenuLink)) "Start menu shortcut removed"
    Check (-not (Test-Path $StartMenuDir)) "Start menu folder removed"
    Check (-not (Test-Path $DesktopLink)) "no desktop shortcut"
    Check ($null -eq (Get-ItemProperty -Path $RunKey -Name $AppName -ErrorAction SilentlyContinue)) "no autostart value"
    $pkgs = @(Get-DriverPackages)
    Check ($pkgs.Count -eq 0) "driver package(s) removed from the store (left: $(@($pkgs | ForEach-Object Name) -join ', '))"
    $devs = @(Get-OurDevices)
    Check ($devs.Count -eq 0) "$HardwareId device removed (left: $(@($devs | ForEach-Object PNPDeviceID) -join ', '))"
    $s = Read-State
    if ($s.ContainsKey("cert") -and $s.cert) {
        foreach ($store in "Root", "TrustedPublisher") {
            if (Test-Path "Cert:\LocalMachine\$store\$($s.cert)") {
                # The uninstaller has no step for it today.
                Warn "test certificate $($s.cert) is still trusted in LocalMachine\$store after uninstall"
            }
        }
    }
    Assert-Sentinel
}

switch ($Phase) {
    "env" {
        $os = Get-CimInstance Win32_OperatingSystem
        Write-Host "OS: $($os.Caption) $($os.Version)"
        $bcd = (bcdedit /enum "{current}" | Out-String)
        $testsigning = $bcd -match '(?im)^testsigning\s+Yes'
        Write-Host "Test-signing: $(if ($testsigning) { 'on' } else { 'off' })"
        $sb = try { if (Confirm-SecureBootUEFI) { "on" } else { "off" } } catch { "unsupported/unknown" }
        Write-Host "Secure Boot: $sb"
        foreach ($svcName in "AudioSrv", "AudioEndpointBuilder") {
            $svc = Get-Service $svcName -ErrorAction SilentlyContinue
            Write-Host "$svcName : $(if ($svc) { "$($svc.Status) ($($svc.StartType))" } else { 'absent' })"
        }
        Check (-not (Test-Path $UninstallKey)) "runner starts without an install"
        Check (@(Get-DriverPackages).Count -eq 0) "runner starts with no driver package"
        New-Item -ItemType Directory -Force -Path $ConfigDir | Out-Null
        "lifecycle sentinel $([guid]::NewGuid())" | Set-Content -Path $Sentinel -Encoding ascii
        $s = Read-State
        $s.sentinel = Get-Sha256 $Sentinel
        Write-State $s
        Ok "sentinel written to $Sentinel"
    }
    "install" {
        Invoke-CurrentSetup "install"
        Assert-Installed -Full
        Assert-DiagnoseFindsInstallLog "install"
        Assert-Sentinel
    }
    "repair" {
        $app = Get-AppDir
        $victims = @("README.md", "scripts\Diagnose.ps1")
        foreach ($v in $victims) { Remove-Item (Join-Path $app $v) -Force }
        Invoke-CurrentSetup "repair"
        foreach ($v in $victims) { Check (Test-Path (Join-Path $app $v)) "repair restored $v" }
        Assert-Installed -Full
        Assert-Sentinel
    }
    "uninstall" {
        Invoke-Uninstall "uninstall-$((Get-Date).ToString('HHmmss'))"
        Assert-Removed
    }
    "previous" {
        Invoke-Setup $Installer "previous"
        Save-InstallLog "previous"
        Assert-Installed
        Assert-Sentinel
    }
    "upgrade" {
        Invoke-CurrentSetup "upgrade"
        Assert-Installed -Full
        Assert-Sentinel
    }
}

if ($script:Failures.Count -gt 0) {
    Write-Host ""
    Write-Host "$($script:Failures.Count) check(s) failed in phase '$Phase':"
    $script:Failures | ForEach-Object { Write-Host "  - $_" }
    exit 1
}
Write-Host "Phase '$Phase' passed."
