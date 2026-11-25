<#
.SYNOPSIS
    One phase of the installer lifecycle test (installer-lifecycle.yml).

.DESCRIPTION
    Runs an installer or the uninstaller silently and checks the machine
    state it should leave behind, failing on anything the installer itself
    controls: installed files (driver files byte-equal to the build's driver
    record), the Apps & Features entry, shortcuts, start-at-sign-in, the
    test certificate in the machine trust stores, the driver package in the
    driver store, the root-enumerated device (exactly one), the Windows
    Firewall rule, the rights on the install folder, and a user-data
    sentinel in %APPDATA%\StreamToSpeaker, which no phase may touch
    (uninstall leaves user data in place).

    Hosted runners run Windows Server without test signing, so the driver
    never starts here, and every current-build run passes /ALLOWSERVER and
    /ALLOWTESTBUILD (Setup refuses both otherwise, which phases
    refuse-server and refuse-test-build check). What is required is that
    Setup says what happened: its exit code (20 when it couldn't finish
    setting up the sound driver) and the "driver-step-result:" line in its
    log must agree with the device Windows has afterwards. Behaviour that needs the driver to run (the
    sound output, its name, the restart after a driver change) can't be
    seen here; the scripts' own tests (phase units) cover the decisions.

    Each current-build setup run must add one Install-Driver.ps1 run to
    <install dir>\logs\install.log, leave install.log in
    %LOCALAPPDATA%\StreamToSpeaker (where earlier releases kept it) alone,
    and compile its Windows helpers in Windows' temp folder, not in the
    temp folder setup inherits from the user's profile.

    State shared between phases (sentinel hash, certificate thumbprint,
    device identity) lives in a JSON file under -WorkDir.

.PARAMETER Phase
    units           run the scripts' own tests in Windows PowerShell
    env             report the runner; create the sentinel
    refuse-server   run -Installer without /ALLOWSERVER: it must refuse
    refuse-test-build  run -Installer (test build) without /ALLOWTESTBUILD:
                    it must refuse
    refuse-dir      run -Installer with /DIR=-InstallDir: it must refuse
    install         run -Installer on a clean machine
    repair          delete two installed files, re-run -Installer
    running-app     re-run -Installer while the app runs in this account
    other-account   re-run -Installer while the app runs in another account
    tasks           start-at-sign-in through /TASKS (install, then two re-runs)
    uninstall       run the registered uninstaller
    previous        run -Installer (an older release, into -InstallDir when
                    given) with lighter checks
    plant           put a file of the person's and a fake uninstaller (it
                    writes a marker when run) in the earlier install folder
    upgrade         run -Installer over the previous install; with
                    -EarlierDir, check what Setup did with that folder
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateSet("units", "env", "refuse-server", "refuse-test-build", "refuse-dir", "install", "repair",
                 "running-app", "other-account", "tasks", "uninstall", "previous", "plant", "upgrade")]
    [string]$Phase,
    # Installer to run.
    [string]$Installer = "",
    # The installer's <name>.driver.json build record (current-build phases).
    [string]$DriverRecord = "",
    # DisplayVersion the Apps & Features entry must show afterwards.
    [string]$ExpectVersion = "",
    # The service exe the installer should have put in place (hash compare).
    [string]$ExpectExe = "",
    # Install folder (/DIR=) for an earlier release, or the folder refuse-dir
    # asks for; the default folder when empty.
    [string]$InstallDir = "",
    # upgrade: the folder an earlier install was in, which Setup moves from.
    [string]$EarlierDir = "",
    # uninstall: delete the installed test certificate file first, as an
    # interrupted earlier update could have.
    [switch]$DeleteCertificateFile,
    [string]$WorkDir = (Join-Path $env:RUNNER_TEMP "lifecycle")
)

$ErrorActionPreference = "Stop"

$AppName = "Stream To Speaker"
$ExeName = "stream-to-speaker.exe"
$HardwareId = "Root\StreamToSpeaker"
$UninstallKey = "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\{8A6C7F92-3E1B-4A2B-9F1C-77F1B6B2A6E0}_is1"
$UninstallKeyWow = "HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\{8A6C7F92-3E1B-4A2B-9F1C-77F1B6B2A6E0}_is1"
$InstallerKey = "HKLM:\SOFTWARE\StreamToSpeaker"
$DefaultAppDir = Join-Path $env:ProgramFiles $AppName
$ExpectedAppDir = if ($InstallDir) { $InstallDir.TrimEnd("\") } else { $DefaultAppDir }
$StartMenuDir = Join-Path ([Environment]::GetFolderPath("CommonPrograms")) $AppName
$StartMenuLink = Join-Path $StartMenuDir "$AppName.lnk"
$DesktopLink = Join-Path ([Environment]::GetFolderPath("CommonDesktopDirectory")) "$AppName.lnk"
$RunKey = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run"
$FirewallRuleName = "StreamToSpeaker-Inbound"
$ConfigDir = Join-Path $env:APPDATA "StreamToSpeaker"
$Sentinel = Join-Path $ConfigDir "lifecycle-sentinel.txt"
$LogDir = Join-Path $WorkDir "logs"
$StateFile = Join-Path $WorkDir "state.json"
$RepoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
# Where earlier releases keep install.log: the profile of the account that
# runs setup.
$ProfileLogDir = Join-Path $env:LOCALAPPDATA "StreamToSpeaker"
# Where Install-Driver.ps1 compiles its Windows helpers: Windows' own temp
# folder, not the temp folder setup inherits from this process, which is in
# the profile of the account that runs it.
$WindowsTemp = Join-Path ([Environment]::GetFolderPath("Windows")) "Temp"
$ProfileTemp = [IO.Path]::GetTempPath()
$WindowsPowerShell = Join-Path $env:SystemRoot "System32\WindowsPowerShell\v1.0\powershell.exe"
# Setup's exit codes (installer\StreamToSpeaker.iss): installed, but the
# sound driver isn't set up; a silent install that needs a restart; a /DIR
# that isn't Program Files (Inno's "failed to initialize").
$NotWorkingExitCode = 20
$RestartExitCode = 3010
$DirRefusedExitCode = 1
$AppUserModelID = "StreamToSpeaker.App"
# The marker the fake uninstaller of phase plant writes when it is run.
$FakeUninstallerMarker = Join-Path $WorkDir "fake-uninstaller-ran.txt"

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
function Write-State($s) { $s | ConvertTo-Json -Depth 5 | Set-Content $StateFile -Encoding utf8 }

function Get-Sha256([string]$path) { (Get-FileHash -Algorithm SHA256 -LiteralPath $path).Hash.ToLower() }

# Published names (oemNN.inf) of every package in the driver store that
# declares our hardware ID, the method scripts\InstallerFunctions.ps1 uses
# (pnputil's labels are localised, the INF copies are not).
function Get-DriverPackages {
    @(Get-ChildItem -Path (Join-Path $env:windir "INF") -Filter "oem*.inf" -ErrorAction SilentlyContinue |
        Where-Object { Select-String -LiteralPath $_.FullName -Pattern $HardwareId -SimpleMatch -Quiet -ErrorAction SilentlyContinue })
}

function Get-OurDevices {
    @(Get-CimInstance Win32_PnPEntity -ErrorAction SilentlyContinue |
        Where-Object { $_.HardwareID -and ($_.HardwareID -contains $HardwareId) })
}

function Get-DeviceProperty([string]$id, [string]$key) {
    try { (Get-PnpDeviceProperty -InstanceId $id -KeyName $key -ErrorAction Stop).Data } catch { $null }
}

# What identifies a device node across setup runs: recreating the device
# gives it a new first-install time even when Windows reuses the instance ID.
function Get-DeviceIdentity {
    @(Get-OurDevices | ForEach-Object {
        $first = Get-DeviceProperty $_.PNPDeviceID "DEVPKEY_Device_FirstInstallDate"
        "$($_.PNPDeviceID)|$(if ($first) { ([datetime]$first).ToUniversalTime().ToString('o') } else { 'unknown' })"
    })
}

# Windows' audio services: whether each runs, and in which process. Setup
# must leave them alone when it doesn't rebuild the sound output.
function Get-AudioServices {
    @(foreach ($n in "AudioEndpointBuilder", "AudioSrv") {
        $s = Get-CimInstance Win32_Service -Filter "Name='$n'" -ErrorAction SilentlyContinue
        if ($s) { "$n=$($s.State)/$($s.ProcessId)" } else { "$n=absent" }
    }) -join ", "
}

function Get-DriverVer([string]$infPath) {
    $line = Select-String -LiteralPath $infPath -Pattern '^\s*DriverVer\s*=' | Select-Object -First 1
    if ($line) { ($line.Line -split "=", 2)[1].Trim() } else { "" }
}

# A command line taken apart: the program (quoted: up to the closing quote;
# unquoted: up to the first space) and what follows it.
function Split-CommandLine([string]$line) {
    $m = [regex]::Match("$line".Trim(), '^(?:"([^"]*)"|(\S+))\s*(.*)$')
    @{
        program   = if ($m.Success) { "$($m.Groups[1].Value)$($m.Groups[2].Value)" } else { "" }
        arguments = if ($m.Success) { $m.Groups[3].Value.Trim() } else { "" }
    }
}

# Whether a value that came from the registry is a plain full path on a
# drive: no quotes, wildcards or second colon, and no "." or ".." part.
function Test-PlainAbsolutePath([string]$path) {
    if ($path -notmatch '^[A-Za-z]:\\[^"*?<>|/:]+$') { return $false }
    @($path -split '\\' | Where-Object { $_ -in ".", ".." }).Count -eq 0
}

# The install folder as registered. It comes from the registry and becomes
# part of paths this test reads, deletes and runs, so anything but a plain
# full path stops the phase.
function Get-AppDir {
    $k = Get-ItemProperty -Path $UninstallKey -ErrorAction SilentlyContinue
    if ($k -and $k.PSObject.Properties["InstallLocation"] -and $k.InstallLocation) {
        $dir = "$($k.InstallLocation)".TrimEnd("\")
        if (-not (Test-PlainAbsolutePath $dir)) { throw "the registered install folder is not a plain full path: '$dir'" }
        return $dir
    }
    $ExpectedAppDir
}

# A phase that installs on a PC without the app says so when an earlier
# phase left something behind: what it then shows is an update, not an
# install, and the phase counts as failed.
function Assert-CleanStart {
    $left = @()
    if (Test-Path $UninstallKey) { $left += "the Apps & Features entry" }
    if (@(Get-OurDevices).Count) { $left += "a $HardwareId device" }
    if (@(Get-DriverPackages).Count) { $left += "a driver package" }
    Check ($left.Count -eq 0) "the phase starts on a PC without $AppName (an earlier phase left: $($left -join ', '))"
}

# Test certificates with our subject in the machine trust stores.
function Get-TrustedTestCertificates {
    @(foreach ($store in "Root", "TrustedPublisher") {
        Get-ChildItem "Cert:\LocalMachine\$store" -ErrorAction SilentlyContinue |
            Where-Object { $_.Subject -eq "CN=Stream To Speaker (test sign)" } |
            ForEach-Object { "$store\$($_.Thumbprint)" }
    })
}

function Assert-Sentinel {
    $s = Read-State
    if (-not $s.ContainsKey("sentinel")) { Fail "no sentinel recorded (env phase not run?)"; return }
    if (-not (Test-Path $Sentinel)) { Fail "user-data sentinel $Sentinel is gone"; return }
    Check ((Get-Sha256 $Sentinel) -eq $s.sentinel) "user-data sentinel unchanged"
}

# Ends a process and everything it started (Setup's loader starts Setup
# itself), by process ID.
function Stop-ProcessTree([int]$ProcessId) {
    foreach ($child in @(Get-CimInstance Win32_Process -Filter "ParentProcessId=$ProcessId" -ErrorAction SilentlyContinue)) {
        Stop-ProcessTree $child.ProcessId
    }
    Stop-Process -Id $ProcessId -Force -ErrorAction SilentlyContinue
}

# Runs an installer silently; returns its exit code ($null when it didn't
# finish in time, after ending it).
function Invoke-Setup {
    param([string]$exe, [string]$label, [string[]]$Extra = @(), [int]$TimeoutMinutes = 15, [string[]]$Omit = @())
    if (-not (Test-Path -LiteralPath $exe)) { throw "installer not found: $exe" }
    $log = Join-Path $LogDir "$label-setup.log"
    $setupArgs = @("/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/SP-", "/LOG=`"$log`"")
    # The runner is Windows Server without test signing: Setup refuses both
    # unless told this is a test.
    foreach ($switch in "ALLOWSERVER", "ALLOWTESTBUILD") {
        if ($Omit -notcontains $switch) { $setupArgs += "/$switch" }
    }
    if ($InstallDir) { $setupArgs += "/DIR=`"$InstallDir`"" }
    $setupArgs += $Extra
    Write-Host "Running $(Split-Path $exe -Leaf) $($setupArgs -join ' ')"
    $p = Start-Process -FilePath $exe -PassThru -ArgumentList $setupArgs
    # Touching the handle keeps it open, so ExitCode is still readable after
    # the process exits (otherwise it can come back $null).
    $null = $p.Handle
    if (-not $p.WaitForExit($TimeoutMinutes * 60 * 1000)) {
        Stop-ProcessTree $p.Id
        Fail "setup did not finish within $TimeoutMinutes minutes (a question waiting for an answer nobody can give?)"
        return $null
    }
    Write-Host "  setup exit code $($p.ExitCode)"
    $p.ExitCode
}

# A file's text; "" when it is missing or empty.
function Read-Text([string]$path) {
    if (-not $path -or -not (Test-Path -LiteralPath $path)) { return "" }
    "$(Get-Content -LiteralPath $path -Raw)"
}

function Get-InstallLog { Join-Path (Get-AppDir) "logs\install.log" }

function Get-TranscriptStarts {
    [regex]::Matches((Read-Text (Get-InstallLog)), '==== Install-Driver started at ').Count
}

# What the last Install-Driver.ps1 run in the transcript says about its
# Windows helpers: the folder it compiled them in and whether it loaded them.
function Get-WrapperCompile {
    $text = Read-Text (Get-InstallLog)
    $start = $text.LastIndexOf("==== Install-Driver started at ")
    $run = if ($start -ge 0) { $text.Substring($start) } else { "" }
    $m = [regex]::Match($run, '(?m)^Compiling the Windows helpers in (.*?)\r?$')
    @{
        folder = if ($m.Success) { $m.Groups[1].Value.Trim() } else { "" }
        loaded = $run.Contains("Loaded the Windows helpers.")
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

# install.log in the per-user folder, where earlier releases kept it. Only
# that file: the app writes its own log and startup.txt in the same folder
# whenever it starts, also when Setup starts it again.
function Get-ProfileLogSnapshot {
    $file = Join-Path $ProfileLogDir "install.log"
    if (-not (Test-Path -LiteralPath $file)) { return "(absent)" }
    $item = Get-Item -LiteralPath $file -Force
    "$($item.Length)|$($item.LastWriteTimeUtc.Ticks)"
}

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

# Setup's exit code and the result line in its log must agree with the
# device Windows has afterwards: "ok" only for one device that works (exit
# code 0, or 3010 when a restart is due), 20 and the reason otherwise.
function Assert-SetupOutcome($code, [string]$label) {
    $log = Read-Text (Join-Path $LogDir "$label-setup.log")
    $m = [regex]::Match($log, 'driver-step-result: (\S+) restart=(\d) .*?problem=(\d+)')
    $devs = @(Get-OurDevices)
    $working = $devs.Count -eq 1 -and [int]$devs[0].ConfigManagerErrorCode -eq 0
    $state = if (-not $devs.Count) { "no device" } else { ($devs | ForEach-Object { "$($_.PNPDeviceID) problem $($_.ConfigManagerErrorCode)" }) -join "; " }
    if (-not $m.Success) {
        Fail "Setup's log has no driver-step result: Setup did not check its driver step (exit code $code; Windows has: $state)"
        return $null
    }
    $result = $m.Groups[1].Value
    $restart = $m.Groups[2].Value -eq "1"
    Write-Host "  driver step: $result, restart $restart, problem $($m.Groups[3].Value); Windows has: $state"
    Check (($result -eq "ok") -eq $working -or ($restart -and $result -eq "ok")) "Setup's result '$result' agrees with the device ($state)"
    $want = if ($result -ne "ok") { $NotWorkingExitCode } elseif ($restart) { $RestartExitCode } else { 0 }
    Check ($code -eq $want) "setup exit code $code (expected $want for '$result'$(if ($restart) { ', restart due' }))"
    $loaded = Get-CimInstance Win32_SystemDriver -Filter "Name='StreamToSpeaker'" -ErrorAction SilentlyContinue
    if (-not ($loaded -and $loaded.State -eq "Running") -and -not $log.Contains("Windows asked for a restart")) {
        Check (-not $restart) "no restart asked while the driver isn't loaded and Windows asked for none"
    }
    @{ result = $result; restart = $restart; log = $log }
}

# Runs this build's installer with the checks every such run gets.
function Invoke-CurrentSetup([string]$label, [string[]]$Extra = @(), [int]$TimeoutMinutes = 15) {
    $before = Get-TranscriptStarts
    $profileBefore = Get-ProfileLogSnapshot
    $watch = Start-CompileWatch $ProfileTemp
    $compiledInProfile = $null
    $code = $null
    try {
        $code = Invoke-Setup $Installer $label -Extra $Extra -TimeoutMinutes $TimeoutMinutes
    } finally {
        $compiledInProfile = Stop-CompileWatch $watch
    }
    Save-InstallLog $label
    if ($null -eq $code) { return $null }
    $after = Get-TranscriptStarts
    Check ($after -eq $before + 1) "Install-Driver.ps1 logged to $(Get-InstallLog) (runs recorded: $before before setup, $after after)"
    Check ((Get-ProfileLogSnapshot) -eq $profileBefore) "setup left $ProfileLogDir\install.log alone"
    $compile = Get-WrapperCompile
    Check ($compile.folder.TrimEnd("\") -eq $WindowsTemp) "Install-Driver.ps1 compiled its Windows helpers in $WindowsTemp, outside the user's profile (logged: '$($compile.folder)')"
    Check $compile.loaded "Install-Driver.ps1 loaded the helpers it compiled"
    if ($null -eq $compiledInProfile) {
        Warn "could not watch $ProfileTemp during setup; whether anything was compiled there is unknown"
    } else {
        Check ($compiledInProfile.Count -eq 0) "nothing was compiled in $ProfileTemp during setup (compiler inputs seen: $($compiledInProfile -join ', '))"
    }
    Assert-SetupOutcome $code $label
}

# Runs Diagnose.ps1 as people do: from the install directory, as a copy in
# another folder, and from 32-bit Windows PowerShell. It must find the
# driver and the device, print the tail of the install log, and leave out
# the account's and the computer's names and the runner's addresses.
function Assert-DiagnoseReport([string]$label) {
    $installed = Join-Path (Get-AppDir) "scripts\Diagnose.ps1"
    $copyDir = Join-Path $WorkDir "diagnose-copy"
    New-Item -ItemType Directory -Force -Path $copyDir | Out-Null
    Copy-Item -LiteralPath $installed $copyDir -Force
    $copy = Join-Path $copyDir "Diagnose.ps1"
    $ps32 = Join-Path $env:SystemRoot "SysWOW64\WindowsPowerShell\v1.0\powershell.exe"
    $runs = [ordered]@{
        "from the install directory"                        = $WindowsPowerShell, $installed
        "from another folder"                               = $WindowsPowerShell, $copy
        "from another folder, in 32-bit Windows PowerShell" = $ps32, $copy
    }
    $image = (Get-ItemProperty "HKLM:\SYSTEM\CurrentControlSet\Services\StreamToSpeaker" -ErrorAction SilentlyContinue).ImagePath
    $imageFile = if ($image) { $image -replace '^\\SystemRoot\\', "$($env:windir.TrimEnd('\'))\" -replace '^\\\?\?\\', '' } else { "" }
    $addresses = @(Get-NetIPAddress -ErrorAction SilentlyContinue |
        Where-Object { $_.IPAddress -notin "127.0.0.1", "::1" } | ForEach-Object { ($_.IPAddress -split '%')[0] })
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
            Stop-ProcessTree $p.Id
            Fail "Diagnose.ps1 $where did not finish within 5 minutes"
            continue
        }
        $text = Read-Text $out
        Check ($p.ExitCode -eq 0) "Diagnose.ps1 $where ran under Windows PowerShell (exit code $($p.ExitCode))"
        Check ($text.Contains("==== Install-Driver finished at ")) "Diagnose.ps1 $where printed the tail of $(Get-InstallLog)"
        Check ($text.Contains("Read the report through before you post it")) "Diagnose.ps1 $where asks for the report to be read before it is posted"
        if ($imageFile) {
            Check ($text.Contains($imageFile)) "Diagnose.ps1 $where names the driver file the service loads ($imageFile)"
        }
        foreach ($d in @(Get-OurDevices)) {
            Check ($text.Contains($d.PNPDeviceID)) "Diagnose.ps1 $where names the device $($d.PNPDeviceID)"
        }
        # A name counts wherever it stands, also joined to others by a dash
        # or an underscore; an address wherever it isn't part of a longer one.
        foreach ($private in @($env:USERNAME, $env:COMPUTERNAME)) {
            if (-not $private) { continue }
            $found = [regex]::IsMatch($text, '(?<![\p{L}\p{N}])' + [regex]::Escape($private) + '(?![\p{L}\p{N}])', 'IgnoreCase')
            Check (-not $found) "Diagnose.ps1 $where leaves out '$private'"
        }
        foreach ($private in $addresses) {
            $found = [regex]::IsMatch($text, '(?<![\w.:])' + [regex]::Escape($private) + '(?![\w:]|\.\d)', 'IgnoreCase')
            Check (-not $found) "Diagnose.ps1 $where leaves out the address '$private'"
        }
    }
    Remove-Item -LiteralPath $copyDir -Recurse -Force
}

function Invoke-Uninstall([string]$label) {
    $k = Get-ItemProperty -Path $UninstallKey -ErrorAction SilentlyContinue
    if (-not $k) { throw "no uninstall entry at $UninstallKey" }
    $dir = Get-AppDir
    # What Apps & Features would run. Setup registers the uninstaller with
    # the switch that makes it keep a log: "<folder>\unins000.exe" /LOG. The
    # value comes from the registry, so it is taken apart and checked before
    # anything runs: only unins###.exe in the registered install folder,
    # followed by that switch or by nothing.
    $registered = "$($k.UninstallString)"
    $command = Split-CommandLine $registered
    $exe = $command.program
    if (-not (Test-PlainAbsolutePath $exe) -or [IO.Path]::GetDirectoryName($exe) -ine $dir -or
        [IO.Path]::GetFileName($exe) -notmatch '^unins\d{3}\.exe$' -or $command.arguments -notin "", "/LOG") {
        throw "the registered uninstall command is not unins###.exe in $dir, with /LOG or nothing after it: $registered"
    }
    if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) { throw "the registered uninstaller $exe is not there" }
    $log = Join-Path $LogDir "$label-uninstall.log"
    # The registered switch first: of two /LOG switches the later one, which
    # names the file, counts.
    $switches = @($command.arguments | Where-Object { $_ }) + @("/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/LOG=`"$log`"")
    Write-Host "Running $exe $($switches -join ' ')"
    $p = Start-Process -FilePath $exe -PassThru -ArgumentList $switches
    $null = $p.Handle
    if (-not $p.WaitForExit(15 * 60 * 1000)) {
        Stop-ProcessTree $p.Id
        throw "uninstaller did not finish within 15 minutes"
    }
    Check ($p.ExitCode -eq 0) "uninstaller exit code $($p.ExitCode)"
    # The uninstaller re-launches itself from %TEMP% and the first process
    # can return before the work is done: wait for its entry and exe to go.
    $deadline = (Get-Date).AddMinutes(5)
    while ((Test-Path $UninstallKey) -or (Test-Path -LiteralPath $exe)) {
        if ((Get-Date) -gt $deadline) { Fail "uninstall still in progress after 5 minutes"; break }
        Start-Sleep -Seconds 2
    }
    $deadline = (Get-Date).AddSeconds(15)
    while ((Test-Path -LiteralPath $dir) -and (Get-Date) -lt $deadline) { Start-Sleep -Seconds 1 }
    $log
}

function Get-FirewallRulesForApp([string]$exe) {
    @(Get-NetFirewallApplicationFilter -ErrorAction SilentlyContinue |
        Where-Object { $_.Program -and ([Environment]::ExpandEnvironmentVariables($_.Program).Trim('"') -ieq $exe) } |
        Get-NetFirewallRule -ErrorAction SilentlyContinue)
}

# The rule Setup adds: inbound, allowed, for the app, on private and work
# networks only.
function Assert-FirewallRule([string]$app) {
    $exe = Join-Path $app $ExeName
    $rule = Get-NetFirewallRule -Name $FirewallRuleName -ErrorAction SilentlyContinue
    if (-not $rule) { Fail "no Windows Firewall rule '$FirewallRuleName' for the app"; return }
    $filter = $rule | Get-NetFirewallApplicationFilter
    Check ($rule.Enabled -eq "True" -and $rule.Direction -eq "Inbound" -and $rule.Action -eq "Allow") "firewall rule is an enabled inbound allow rule ($($rule.Enabled), $($rule.Direction), $($rule.Action))"
    Check ($filter.Program -ieq $exe) "firewall rule is for $exe (is: $($filter.Program))"
    $profiles = "$($rule.Profile)"
    Check ($profiles -match "Private" -and $profiles -match "Domain" -and $profiles -notmatch "Public|Any") "firewall rule covers private and work networks only ($profiles)"
}

# What releases put in their install folder, by name (as
# scripts\InstallerFunctions.ps1 lists them for moving an earlier install).
$ReleaseFileNames = @(
    "stream-to-speaker.exe", "README.md", "LICENSE", "LICENSE-BINARIES.md", "LICENSE-INSTALLER.txt",
    "unins000.exe", "unins000.dat", "unins000.msg",
    "driver\StreamToSpeaker.sys", "driver\StreamToSpeaker.inf", "driver\StreamToSpeaker.cat",
    "driver\StreamToSpeaker.cer", "driver\devcon.exe",
    "scripts\Pre-Install.ps1", "scripts\Rename-Endpoint.ps1", "scripts\Reset-Install.ps1", "scripts\Diagnose.ps1",
    "scripts\Uninstall-Driver.ps1", "scripts\Remove-TestCertificate.ps1", "logs\install.log")

# Whether a shortcut carries this AppUserModelID: the .lnk file keeps it in
# its property store, as UTF-16 text.
function Test-ShortcutAppId([string]$link, [string]$id) {
    if (-not (Test-Path -LiteralPath $link)) { return $false }
    [Text.Encoding]::Unicode.GetString([IO.File]::ReadAllBytes($link)).Contains($id) -or
        [Text.Encoding]::Unicode.GetString([IO.File]::ReadAllBytes($link), 1, (Get-Item -LiteralPath $link).Length - 1).Contains($id)
}

# The thumbprints Setup recorded for the uninstaller.
function Get-RecordedCertificates {
    @((Get-ItemProperty -Path "$InstallerKey\Installer" -Name TestCertificates -ErrorAction SilentlyContinue).TestCertificates |
        Where-Object { $_ } | ForEach-Object { $_.ToUpperInvariant() })
}

function Get-ShortcutTarget([string]$link) {
    try { (New-Object -ComObject WScript.Shell).CreateShortcut($link).TargetPath } catch { "" }
}

# Everything an install must leave behind. -Full adds the current build's
# expectations (exact payload list, driver files equal to the build record,
# test certificate trust, the firewall rule).
function Assert-Installed {
    param([switch]$Full, [switch]$SignInOn, $Outcome = $null)
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
    Check ($app -eq $ExpectedAppDir) "installed to $ExpectedAppDir ($app)"

    $required = @($ExeName, "unins000.exe", "unins000.dat", "driver\StreamToSpeaker.inf", "driver\StreamToSpeaker.sys")
    if ($Full) {
        $required += @(
            "driver\StreamToSpeaker.cat", "driver\devcon.exe",
            "scripts\InstallerFunctions.ps1", "scripts\Install-Driver.ps1", "scripts\Rename-Endpoint.ps1",
            "scripts\Reset-Install.ps1", "scripts\Diagnose.ps1", "scripts\Uninstall-Driver.ps1",
            "scripts\Remove-TestCertificate.ps1", "scripts\Set-FirewallRule.ps1", "scripts\Remove-SignInEntries.ps1",
            "scripts\Find-RunningCopies.ps1", "scripts\Remove-EarlierInstall.ps1",
            "README.md", "LICENSE", "LICENSE-BINARIES.md", "LICENSE-INSTALLER.txt")
        Check (-not (Test-Path -LiteralPath (Join-Path $app "scripts\Pre-Install.ps1"))) "no Pre-Install.ps1 left from an earlier version"
    }
    foreach ($f in $required) { Check (Test-Path -LiteralPath (Join-Path $app $f)) "installed $f" }

    if ($ExpectExe) {
        Check ((Get-Sha256 (Join-Path $app $ExeName)) -eq (Get-Sha256 $ExpectExe)) "installed $ExeName is this build's"
    }

    $s = Read-State
    $rec = $null
    if ($DriverRecord) { $rec = Get-Content $DriverRecord -Raw | ConvertFrom-Json }
    if ($Full -and $rec) {
        foreach ($p in $rec.files.PSObject.Properties) {
            # A name from the build record becomes part of a path.
            if ($p.Name -notmatch '^[A-Za-z0-9._-]+$') {
                Fail "the build record lists a driver file that is not a plain file name: '$($p.Name)'"
                continue
            }
            $installed = Join-Path $app "driver\$($p.Name)"
            if (Test-Path -LiteralPath $installed) {
                Check ((Get-Sha256 $installed) -eq $p.Value) "driver\$($p.Name) matches the build record"
            } else {
                Fail "driver\$($p.Name) listed in the build record but not installed"
            }
        }
        $cer = Join-Path $app "driver\StreamToSpeaker.cer"
        if ($rec.test_cert) {
            Check (Test-Path -LiteralPath $cer) "test certificate shipped (test-signed build)"
            if (Test-Path -LiteralPath $cer) {
                $thumb = (New-Object System.Security.Cryptography.X509Certificates.X509Certificate2 $cer).Thumbprint
                $s.cert = $thumb
                Check (Test-Path "Cert:\LocalMachine\Root\$thumb") "test certificate in LocalMachine\Root"
                Check (Test-Path "Cert:\LocalMachine\TrustedPublisher\$thumb") "test certificate in LocalMachine\TrustedPublisher"
            }
        } else {
            Check (-not (Test-Path -LiteralPath $cer)) "no test certificate file after a Microsoft-signed install"
            # The earlier test build's certificate may stay while its driver
            # package does (Setup keeps that until the new driver runs, which
            # it never does on this runner); the check below makes sure the
            # uninstaller can still find it.
        }
        # Every Stream To Speaker test certificate Windows trusts is recorded
        # for the uninstaller.
        $recorded = @(Get-RecordedCertificates)
        $unrecorded = @(Get-TrustedTestCertificates | ForEach-Object { ($_ -split '\\')[-1].ToUpperInvariant() } |
            Where-Object { $recorded -notcontains $_ } | Select-Object -Unique)
        Check ($unrecorded.Count -eq 0) "every trusted test certificate is recorded for the uninstaller (not recorded: $($unrecorded -join ', '))"
        Assert-FirewallRule $app
    }

    Check (Test-Path -LiteralPath $StartMenuLink) "Start menu shortcut"
    if ($Full) {
        Check (Test-ShortcutAppId $StartMenuLink $AppUserModelID) "the Start-menu shortcut carries the AppUserModelID $AppUserModelID"
        $target = Get-ShortcutTarget $StartMenuLink
        Check ($target -ieq (Join-Path $app $ExeName)) "the Start-menu shortcut starts $app\$ExeName (starts: '$target')"
    }
    Check (-not (Test-Path -LiteralPath $DesktopLink)) "no desktop shortcut (task off by default)"
    $run = (Get-ItemProperty -Path $RunKey -Name $AppName -ErrorAction SilentlyContinue).$AppName
    if ($SignInOn) {
        $command = Split-CommandLine "$run"
        Check ($command.program -ieq (Join-Path $app $ExeName) -and $command.arguments -eq "--startup") "start-at-sign-in entry starts $app\$ExeName --startup (entry: $run)"
    } else {
        Check (-not $run) "no start-at-sign-in entry (task off by default)"
    }

    # Driver store and device. Loading the driver needs test signing (test-
    # signed builds) or a client OS (Microsoft-signed client drivers),
    # neither of which a hosted runner has, so the device isn't required to
    # work; there must be exactly one, and Setup must have said what it is
    # (Assert-SetupOutcome).
    $pkgs = @(Get-DriverPackages)
    Write-Host "  driver store: $(@($pkgs | ForEach-Object Name) -join ', ')"
    if ($Full -and $rec) {
        $want = Get-DriverVer (Join-Path $app "driver\StreamToSpeaker.inf")
        $have = @($pkgs | ForEach-Object { Get-DriverVer $_.FullName })
        if ($rec.driver.kind -eq "test-signed") {
            Check ($have -contains $want) "this build's driver package is in the store (DriverVer $want; store: $($have -join ', '))"
        } elseif (-not $pkgs.Count) {
            Check ($Outcome -and $Outcome.result -ne "ok") "Setup reports a problem when no driver package is in the store"
        }
        if ($pkgs.Count -gt 1) {
            # Earlier packages stay only until the new driver runs, and Setup says so.
            Check ($Outcome -and "$($Outcome.log)".Contains("Keeping the earlier driver package")) "$($pkgs.Count) driver packages in the store, and Setup says it keeps the earlier one until the new driver runs"
        }
    } elseif ($pkgs.Count -eq 0) {
        Warn "no driver package staged by this installer on this runner"
    } elseif ($pkgs.Count -gt 1) {
        Fail "$($pkgs.Count) driver packages staged; the install should replace, not add"
    }
    $devs = @(Get-OurDevices)
    foreach ($d in $devs) {
        Write-Host "  device $($d.PNPDeviceID): status=$($d.Status) problem=$($d.ConfigManagerErrorCode)"
    }
    if ($Full) {
        Check ($devs.Count -eq 1) "exactly one $HardwareId device (found $($devs.Count))"
    } else {
        Check ($devs.Count -le 1) "at most one $HardwareId device (found $($devs.Count))"
    }
    $svc = Get-ItemProperty "HKLM:\SYSTEM\CurrentControlSet\Services\StreamToSpeaker" -ErrorAction SilentlyContinue
    Write-Host "  kernel service key: $(if ($svc) { "present (Start=$($svc.Start))" } else { 'absent' })"

    $s.device = @(Get-DeviceIdentity)
    Write-State $s
}

function Assert-Removed([string]$app, [string]$uninstallLog) {
    Check (-not (Test-Path $UninstallKey)) "Apps & Features entry removed"
    Check (-not (Test-Path -LiteralPath $app)) "install directory removed"
    if (Test-Path -LiteralPath $app) {
        Get-ChildItem -LiteralPath $app -Recurse | ForEach-Object { Write-Host "  left behind: $($_.FullName)" }
    }
    Check (-not (Test-Path -LiteralPath $StartMenuLink)) "Start menu shortcut removed"
    Check (-not (Test-Path -LiteralPath $StartMenuDir)) "Start menu folder removed"
    Check (-not (Test-Path -LiteralPath $DesktopLink)) "no desktop shortcut"
    Check ($null -eq (Get-ItemProperty -Path $RunKey -Name $AppName -ErrorAction SilentlyContinue)) "no start-at-sign-in entry"
    $pkgs = @(Get-DriverPackages)
    Check ($pkgs.Count -eq 0) "driver package(s) removed from the store (left: $(@($pkgs | ForEach-Object Name) -join ', '))"
    $devs = @(Get-OurDevices)
    Check ($devs.Count -eq 0) "$HardwareId device removed (left: $(@($devs | ForEach-Object PNPDeviceID) -join ', '))"
    $certs = @(Get-TrustedTestCertificates)
    Check ($certs.Count -eq 0) "no Stream To Speaker test certificate trusted after uninstall (left: $($certs -join ', '))"
    Check (-not (Test-Path $InstallerKey)) "the installer's record ($InstallerKey) removed"
    $rules = @(Get-FirewallRulesForApp (Join-Path $app $ExeName)) + @(Get-NetFirewallRule -Name $FirewallRuleName -ErrorAction SilentlyContinue)
    Check ($rules.Count -eq 0) "no Windows Firewall rule for the app left (left: $(@($rules | ForEach-Object DisplayName) -join ', '))"
    $text = Read-Text $uninstallLog
    Check ($text.Contains("[uninstall-driver]")) "the uninstall log records the driver removal"
    Assert-Sentinel
}

# Starts the installed app with no window, no speakers and a sine source,
# optionally as another account; returns the process.
function Start-HeadlessApp([string]$app, [int]$port, [pscredential]$Credential = $null) {
    $exe = Join-Path $app $ExeName
    $argList = @("--headless", "--source", "sine", "--no-discovery", "--no-airplay", "--no-sendspin", "--no-interactive", "--port", "$port")
    $p = if ($Credential) {
        Start-Process -FilePath $exe -ArgumentList $argList -Credential $Credential -LoadUserProfile -PassThru -WorkingDirectory $app
    } else {
        Start-Process -FilePath $exe -ArgumentList $argList -PassThru -WindowStyle Hidden
    }
    $null = $p.Handle
    Start-Sleep -Seconds 5
    if ($p.HasExited) { throw "the app exited at once (exit code $($p.ExitCode)); the case can't be set up" }
    Write-Host "  app running as process $($p.Id)"
    $p
}

# Every copy of the app started from $app after $since (Setup starts it
# again after a silent update); returns their process IDs.
function Get-AppStartedSince([string]$app, [datetime]$since) {
    $exe = Join-Path $app $ExeName
    @(Get-CimInstance Win32_Process -Filter "Name='$ExeName'" -ErrorAction SilentlyContinue |
        Where-Object { $_.ExecutablePath -ieq $exe -and $_.CreationDate -gt $since } | ForEach-Object { $_.ProcessId })
}

# Re-runs this build's installer while a copy of the app runs: Setup must
# close it without asking (silent) and finish. It starts the app again only
# for the account whose copy it closed: this one's, or none when the copy
# was another account's. Nothing here depends on what the app writes when it
# starts.
function Invoke-SetupOverRunningApp([string]$label, [pscredential]$Credential = $null) {
    $app = Get-AppDir
    $port = if ($Credential) { 59012 } else { 59011 }
    $p = Start-HeadlessApp $app $port $Credential
    $since = Get-Date
    try {
        $outcome = Invoke-CurrentSetup $label -TimeoutMinutes 5
        if ($null -eq $outcome) { return }
        Check $p.HasExited "Setup closed the running app (process $($p.Id))"
        $log = "$($outcome.log)"
        $started = @(Get-AppStartedSince $app $since)
        if ($Credential) {
            Check ($log -match 'running-copies: mine=0 other=1') "Setup saw the copy as another account's"
            Check (-not $log.Contains("Started Stream To Speaker again.") -and -not $started.Count) "Setup started no copy in this account, where none ran (started: $($started -join ', '))"
            Check ($log.Contains("the copy Setup closed ran in another account")) "Setup's log says why it started none"
        } else {
            Check ($log -match 'running-copies: mine=1 other=0') "Setup saw the copy as this account's"
            Check ($log.Contains("Started Stream To Speaker again.")) "Setup started the app again after the silent update"
        }
        Assert-Installed -Full -Outcome $outcome
    } finally {
        if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue }
        foreach ($id in @(Get-AppStartedSince $app $since)) {
            Write-Host "  ending the copy Setup started (process $id)"
            Stop-Process -Id $id -Force -ErrorAction SilentlyContinue
        }
    }
}

# After an update moved an earlier install out of -EarlierDir: nothing in
# that folder was run, nothing a release put there is left, and the file the
# person keeps there is untouched.
function Assert-EarlierFolder([string]$dir, $outcome) {
    $s = Read-State
    Check (-not (Test-Path -LiteralPath $FakeUninstallerMarker)) "nothing in the earlier folder was run (the uninstaller planted there didn't run)"
    $left = @($ReleaseFileNames | Where-Object { Test-Path -LiteralPath (Join-Path $dir $_) })
    Check ($left.Count -eq 0) "the earlier folder holds nothing a release put there (left: $($left -join ', '))"
    $notes = Join-Path $dir "notes.txt"
    Check ((Test-Path -LiteralPath $notes) -and (Get-Sha256 $notes) -eq $s.planted) "the person's own file in the earlier folder is untouched"
    Check ("$($outcome.log)" -match 'earlier-install-result: not-empty') "Setup says the earlier folder stays because it holds other files"
}

function Write-PhaseResult {
    if ($script:Failures.Count -gt 0) {
        Write-Host ""
        Write-Host "$($script:Failures.Count) check(s) failed in phase '$Phase':"
        $script:Failures | ForEach-Object { Write-Host "  - $_" }
        exit 1
    }
    Write-Host "Phase '$Phase' passed."
}

switch ($Phase) {
    "units" {
        foreach ($t in @(Get-ChildItem -LiteralPath (Join-Path $RepoRoot "scripts\tests") -Filter "*.Tests.ps1")) {
            Write-Host "--- $($t.Name) (Windows PowerShell)"
            & $WindowsPowerShell -NoProfile -NonInteractive -ExecutionPolicy Bypass -File $t.FullName
            Check ($LASTEXITCODE -eq 0) "$($t.Name): $LASTEXITCODE test(s) failed"
        }
    }
    "env" {
        $os = Get-CimInstance Win32_OperatingSystem
        Write-Host "OS: $($os.Caption) $($os.Version) (product type $($os.ProductType))"
        $opts = (Get-ItemProperty "HKLM:\SYSTEM\CurrentControlSet\Control" -Name SystemStartOptions).SystemStartOptions
        Write-Host "Boot options: $opts"
        $sb = try { if (Confirm-SecureBootUEFI) { "on" } else { "off" } } catch { "unsupported/unknown" }
        Write-Host "Secure Boot: $sb"
        Write-Host "Audio services: $(Get-AudioServices)"
        Check (-not (Test-Path $UninstallKey)) "runner starts without an install"
        Check (@(Get-DriverPackages).Count -eq 0) "runner starts with no driver package"
        New-Item -ItemType Directory -Force -Path $ConfigDir | Out-Null
        "lifecycle sentinel $([guid]::NewGuid())" | Set-Content -Path $Sentinel -Encoding ascii
        $s = Read-State
        $s.sentinel = Get-Sha256 $Sentinel
        Write-State $s
        Ok "sentinel written to $Sentinel"
    }
    "refuse-server" {
        Assert-CleanStart
        $certsBefore = @(Get-TrustedTestCertificates)
        $code = Invoke-Setup $Installer "refuse-server" -Omit "ALLOWSERVER" -TimeoutMinutes 5
        Check ($code -eq 7) "Setup refused Windows Server before changing anything (exit code $code, expected 7)"
        Check (-not (Test-Path $UninstallKey)) "nothing installed"
        Check (-not (Test-Path -LiteralPath $DefaultAppDir)) "no install folder"
        Check (@(Get-DriverPackages).Count -eq 0) "no driver package"
        Check (@(Get-OurDevices).Count -eq 0) "no device"
        Check ((($certsBefore | Sort-Object) -join ',') -eq ((@(Get-TrustedTestCertificates) | Sort-Object) -join ',')) "no certificate trusted"
        if (Test-Path $UninstallKey) {
            Write-Host "Setup installed anyway; uninstalling so that the next phases start clean."
            $null = Invoke-Uninstall "refuse-server-cleanup"
        }
    }
    "refuse-test-build" {
        $certsBefore = @(Get-TrustedTestCertificates)
        $code = Invoke-Setup $Installer "refuse-test-build" -Omit "ALLOWTESTBUILD" -TimeoutMinutes 5
        Check ($code -eq 7) "Setup refused a test build on a PC without test signing before changing anything (exit code $code, expected 7)"
        Check ("$(Read-Text (Join-Path $LogDir 'refuse-test-build-setup.log'))".Contains("This is a test build")) "Setup's log says why"
        Check (-not (Test-Path $UninstallKey)) "nothing installed"
        Check (-not (Test-Path -LiteralPath $DefaultAppDir)) "no install folder"
        Check (@(Get-DriverPackages).Count -eq 0) "no driver package"
        Check (@(Get-OurDevices).Count -eq 0) "no device"
        Check ((($certsBefore | Sort-Object) -join ',') -eq ((@(Get-TrustedTestCertificates) | Sort-Object) -join ',')) "no certificate trusted"
        if (Test-Path $UninstallKey) {
            Write-Host "Setup installed anyway; uninstalling so that the next phases start clean."
            $null = Invoke-Uninstall "refuse-test-build-cleanup"
        }
    }
    "refuse-dir" {
        Assert-CleanStart
        $certsBefore = @(Get-TrustedTestCertificates)
        $code = Invoke-Setup $Installer "refuse-dir" -TimeoutMinutes 5
        Check ($code -eq $DirRefusedExitCode) "Setup refused /DIR=$InstallDir before changing anything (exit code $code, expected $DirRefusedExitCode)"
        Check (-not (Test-Path $UninstallKey)) "nothing installed"
        Check (-not (Test-Path -LiteralPath $InstallDir)) "no folder $InstallDir"
        Check (-not (Test-Path -LiteralPath $DefaultAppDir)) "no install folder in Program Files"
        Check (@(Get-DriverPackages).Count -eq 0) "no driver package"
        Check (@(Get-OurDevices).Count -eq 0) "no device"
        Check ((($certsBefore | Sort-Object) -join ',') -eq ((@(Get-TrustedTestCertificates) | Sort-Object) -join ',')) "no certificate trusted"
        if (Test-Path $UninstallKey) {
            Write-Host "Setup installed anyway; uninstalling so that the next phases start clean."
            $null = Invoke-Uninstall "refuse-dir-cleanup"
        }
    }
    "install" {
        Assert-CleanStart
        $audio = Get-AudioServices
        $outcome = Invoke-CurrentSetup "install"
        Assert-Installed -Full -Outcome $outcome
        Check ((Get-AudioServices) -eq $audio) "Windows' audio services left alone on a first install ($audio)"
        Assert-DiagnoseReport "install"
        Assert-Sentinel
    }
    "repair" {
        $app = Get-AppDir
        # Two installed files are deleted below: only in the folder this
        # test installed to.
        if ($app -ine $DefaultAppDir) { throw "the install to repair is registered in '$app', not in $DefaultAppDir" }
        $before = @((Read-State).device)
        $audio = Get-AudioServices
        $victims = @("README.md", "scripts\Diagnose.ps1")
        foreach ($v in $victims) { Remove-Item -LiteralPath (Join-Path $app $v) -Force }
        $outcome = Invoke-CurrentSetup "repair"
        foreach ($v in $victims) { Check (Test-Path -LiteralPath (Join-Path $app $v)) "repair restored $v" }
        $after = @(Get-DeviceIdentity)
        Check (($before -join ';') -eq ($after -join ';')) "the device was kept, not recreated (before: $($before -join '; '); after: $($after -join '; '))"
        Check ((Get-AudioServices) -eq $audio) "Windows' audio services left alone ($audio)"
        Assert-Installed -Full -Outcome $outcome
        Assert-Sentinel
    }
    "running-app" {
        Invoke-SetupOverRunningApp "running-app"
        Assert-Sentinel
    }
    "other-account" {
        $user = "stslifecycle"
        $password = "Sts-" + [guid]::NewGuid().ToString("N").Substring(0, 16) + "!a1"
        & net.exe user $user $password /add /comment:"installer lifecycle test" | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "could not create the local account $user" }
        try {
            Start-Service seclogon -ErrorAction SilentlyContinue
            $cred = New-Object System.Management.Automation.PSCredential($user, (ConvertTo-SecureString $password -AsPlainText -Force))
            Invoke-SetupOverRunningApp "other-account" $cred
        } finally {
            & net.exe user $user /delete | Out-Null
            Get-CimInstance Win32_UserProfile -ErrorAction SilentlyContinue |
                Where-Object { $_.LocalPath -and (Split-Path -Leaf $_.LocalPath) -eq $user } |
                Remove-CimInstance -ErrorAction SilentlyContinue
        }
        Assert-Sentinel
    }
    "tasks" {
        Assert-CleanStart
        $tasks = "/TASKS=autostart"
        $outcome = Invoke-CurrentSetup "tasks-install" -Extra @($tasks)
        Assert-Installed -Full -SignInOn -Outcome $outcome
        # Switched off in the app (or in Windows), then Setup run again with
        # the task asked for: it must be on again.
        Remove-ItemProperty -Path $RunKey -Name $AppName -ErrorAction SilentlyContinue
        $outcome = Invoke-CurrentSetup "tasks-again" -Extra @($tasks)
        Assert-Installed -Full -SignInOn -Outcome $outcome
        # Switched off, then a silent update that names no tasks: it stays off.
        Remove-ItemProperty -Path $RunKey -Name $AppName -ErrorAction SilentlyContinue
        $outcome = Invoke-CurrentSetup "tasks-none"
        Assert-Installed -Full -Outcome $outcome
        # On again for the uninstall phase, which must remove it.
        $outcome = Invoke-CurrentSetup "tasks-on" -Extra @($tasks)
        Assert-Installed -Full -SignInOn -Outcome $outcome
        Assert-Sentinel
    }
    "uninstall" {
        $app = Get-AppDir
        if ($DeleteCertificateFile) {
            $cer = Join-Path $app "driver\StreamToSpeaker.cer"
            if (Test-Path -LiteralPath $cer) {
                Remove-Item -LiteralPath $cer -Force
                Write-Host "Deleted ${cer}; the uninstaller must still remove the certificate it stands for."
            }
        }
        $log = Invoke-Uninstall "uninstall-$((Get-Date).ToString('HHmmss'))"
        Assert-Removed $app $log
    }
    "previous" {
        Assert-CleanStart
        $code = Invoke-Setup $Installer "previous-$((Get-Date).ToString('HHmmss'))"
        Write-Host "  earlier release's setup exit code $code"
        Save-InstallLog "previous"
        Assert-Installed
        Assert-Sentinel
    }
    "plant" {
        # What a person (or another program) left in the earlier install's
        # folder: a file of their own, and an uninstaller that isn't the one
        # the release installed; Setup must not run it.
        $s = Read-State
        $notes = Join-Path $InstallDir "notes.txt"
        Set-Content -LiteralPath $notes -Value "A file of the person's, kept in the install folder." -Encoding ascii
        $s.planted = Get-Sha256 $notes
        Write-State $s
        if (Test-Path -LiteralPath $FakeUninstallerMarker) { Remove-Item -LiteralPath $FakeUninstallerMarker -Force }
        $fake = Join-Path $WorkDir "fake-unins000.exe"
        $maker = Join-Path $WorkDir "make-fake-uninstaller.ps1"
        $source = @(
            'Add-Type -OutputType ConsoleApplication -OutputAssembly $args[0] -TypeDefinition @"',
            'public static class FakeUninstaller {',
            '    public static void Main() {',
            '        System.IO.File.WriteAllText(@"$($args[1])", "The uninstaller planted in the earlier folder ran.");',
            '    }',
            '}',
            '"@')
        Set-Content -LiteralPath $maker -Value $source -Encoding ascii
        & $WindowsPowerShell -NoProfile -NonInteractive -ExecutionPolicy Bypass -File $maker $fake $FakeUninstallerMarker
        if (-not (Test-Path -LiteralPath $fake)) { throw "could not build the fake uninstaller" }
        Copy-Item -LiteralPath $fake -Destination (Join-Path $InstallDir "unins000.exe") -Force
        Ok "planted $notes and a fake unins000.exe in $InstallDir"
    }
    "upgrade" {
        $outcome = Invoke-CurrentSetup "upgrade-$((Get-Date).ToString('HHmmss'))"
        Assert-Installed -Full -Outcome $outcome
        if ($EarlierDir) { Assert-EarlierFolder $EarlierDir $outcome }
        Assert-Sentinel
    }
}

Write-PhaseResult
