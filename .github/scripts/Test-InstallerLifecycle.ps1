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
    if (-not $p.WaitForExit(15 * 60 * 1000)) {
        $p | Stop-Process -Force -ErrorAction SilentlyContinue
        throw "setup did not finish within 15 minutes (a dialog waiting for input?)"
    }
    Check ($p.ExitCode -eq 0) "setup exit code $($p.ExitCode)"
}

function Invoke-Uninstall([string]$label) {
    $k = Get-ItemProperty -Path $UninstallKey -ErrorAction SilentlyContinue
    if (-not $k) { throw "no uninstall entry at $UninstallKey" }
    $exe = $k.UninstallString.Trim('"')
    $log = Join-Path $LogDir "$label-uninstall.log"
    Write-Host "Running $exe silently (log: $log)"
    $p = Start-Process -FilePath $exe -PassThru -ArgumentList @(
        "/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/LOG=`"$log`"")
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
        Invoke-Setup $Installer "install"
        Assert-Installed -Full
        Assert-Sentinel
    }
    "repair" {
        $app = Get-AppDir
        $victims = @("README.md", "scripts\Diagnose.ps1")
        foreach ($v in $victims) { Remove-Item (Join-Path $app $v) -Force }
        Invoke-Setup $Installer "repair"
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
        Assert-Installed
        Assert-Sentinel
    }
    "upgrade" {
        Invoke-Setup $Installer "upgrade"
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
