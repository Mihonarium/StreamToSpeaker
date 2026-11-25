<#
.SYNOPSIS
    Starts the app on this machine's desktop, clicks its tray icon with
    real mouse input, and keeps pictures of the desktop, a timeline of the
    panel's window and the foreground, and the app's log (panel-click.yml).

.DESCRIPTION
    What Windows does to the panel's window and its focus can only be seen
    on Windows. The clicks here go through Windows' input queue like a
    mouse's (SendInput), at the place the taskbar shows the icon; nothing is
    posted to the app's windows, so Windows decides about the focus as it
    does for people.

    1. Puts the icon on the taskbar or among the hidden icons (Windows 10's
       taskbar: the "show all icons" setting and a restart of Explorer;
       Windows 11's: the per-program setting Explorer makes when an icon is
       first shown, after a first start).
    2. Starts the app: with -Start menu by a double click on a shortcut on
       the desktop, as a person starts it (the panel opens by itself, and
       Windows lets the app take the foreground as it does after a click);
       with -Start sign-in as the sign-in entry does (--startup: it waits
       for a click, except at first run). Then pictures every 100 ms for
       3 s.
    3. Waits for the icon, takes a picture, opens the hidden icons when the
       icon is there, and clicks the icon once or twice at its centre. The
       icon is looked for only among the notification area's buttons (at
       the place the taskbar gives it) and the hidden icons' buttons, and
       only an element named as the tray icon is ("Stream To Speaker", an
       en dash, its state) counts: no taskbar button of the app's windows.
       Found nowhere, the run fails instead of clicking something else.
       The timeline and the step summary say what was clicked.
    4. Pictures every 100 ms for 2 s; then, if the panel's window is on
       screen, a click inside it near its top edge and pictures every
       100 ms for 1 s.
    5. Asks the app to close, and keeps its log.

    The machine has no sound device and no driver. Started as it is, the
    app would look for its driver, fall back to recording the default
    output, find none and stop with "couldn't start"; so it reads a test
    tone instead (--source sine), which changes nothing in the windows.
    It has no graphics driver either, and the app's windows need more
    OpenGL than Windows draws without one: panel-click.yml puts the app
    next to Mesa's opengl32.dll, which this script tells to draw in
    software (GALLIUM_DRIVER). RUST_LOG adds egui-winit's and eframe's own
    lines about the windows to the app's log: every command a window gets
    (shown, hidden, focused), and windows made and closed; and it puts the
    shell's lines at the debug level, where they name any program that
    takes the foreground (a release's log names only Windows' shell and
    the app).
    With -Network quiet (the default) the app opens no port to the network
    and searches for no speakers (--bind 127.0.0.1 --no-discovery
    --no-airplay --no-sendspin), so no firewall question comes up; with
    -Network full it starts with the network as installed, and a firewall
    question, if this machine asks one, is part of what happens.

    Everything lands in -Out: rig.log (what this script did and saw, with
    UTC times like the app's log), timeline.txt (one line per picture: the
    panel's window shown or hidden, where and its extended style, the root
    window shown or hidden and its extended style, the foreground window,
    the pointer), pictures\*.png, and the app's log and startup.txt. A
    summary goes to the job's step summary.

.PARAMETER Panel
    flyout: the panel as people get it. window: the small-window form, which
    the app reads from STREAM_TO_SPEAKER_PANEL only in a build with debug
    assertions (panel-click.yml builds such a one for it).
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Exe,
    [ValidateSet("single", "double")][string]$Click = "single",
    [ValidateSet("taskbar", "hidden")][string]$Icon = "taskbar",
    [ValidateSet("flyout", "window")][string]$Panel = "flyout",
    [ValidateSet("menu", "sign-in")][string]$Start = "sign-in",
    [ValidateSet("quiet", "full")][string]$Network = "quiet",
    [switch]$FirstRun,
    [Parameter(Mandatory)][string]$Out
)

$ErrorActionPreference = "Stop"

$PanelTitle = "Stream To Speaker"
$RootTitle = "Stream To Speaker (background)"
# The tray icon's tooltip, and so its name to UI Automation, starts with
# "Stream To Speaker", an en dash and a space; no window of the app is
# titled so (the panel's, the root's and the dialogs' titles have no dash).
# The dash is written as a character code: Windows PowerShell reads a
# script without a byte-order mark in the ANSI code page.
$TrayTip = "Stream To Speaker " + [char]0x2013 + " "
$TrayPattern = '^' + [regex]::Escape($TrayTip)
$ExeName = "stream-to-speaker.exe"
$ShortcutName = "Panel click app"
$SettingsDir = Join-Path $env:APPDATA "StreamToSpeaker"
$LogDir = Join-Path $env:LOCALAPPDATA "StreamToSpeaker"
$PictureDir = Join-Path $Out "pictures"
$RigLog = Join-Path $Out "rig.log"
New-Item -ItemType Directory -Force -Path $Out, $PictureDir | Out-Null

function Say([string]$msg) {
    $line = "{0} {1}" -f [DateTime]::UtcNow.ToString("yyyy-MM-ddTHH:mm:ss.fffZ"), $msg
    Write-Host $line
    Add-Content -LiteralPath $RigLog -Value $line -Encoding UTF8
}

# What ran otherwise than the case asked, for rig.log and the step summary.
$script:Warnings = @()
function Warn([string]$msg) {
    Say "WARNING: $msg"
    $script:Warnings += $msg
}

# What the click on the icon hit, for the step summary.
$script:Clicked = "nothing"

# The app's environment. A start by hand goes through Explorer, which takes
# the user's variables when they change, so they are set for the user too.
$PanelSwitch = $null
if ($Panel -eq "window") { $PanelSwitch = "window" }
$AppEnvironment = [ordered]@{
    "RUST_LOG"                = "egui_winit=trace,eframe=debug,stream_to_speaker::shell=debug"
    "GALLIUM_DRIVER"          = "llvmpipe"
    "STREAM_TO_SPEAKER_PANEL" = $PanelSwitch
}

function Set-AppEnvironment([bool]$on) {
    foreach ($name in $AppEnvironment.Keys) {
        $value = $null
        if ($on) { $value = $AppEnvironment[$name] }
        [Environment]::SetEnvironmentVariable($name, $value, "Process")
        [Environment]::SetEnvironmentVariable($name, $value, "User")
    }
}

Add-Type -Path (Join-Path $PSScriptRoot "PanelRig.cs")
Add-Type -AssemblyName WindowsBase
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes

# ---------------------------------------------------------------------------
# UI Automation: the names the taskbar gives its buttons (a tray icon's is
# its tooltip), and where they are.

function Get-UiaTopLevel([string]$class) {
    $cond = [System.Windows.Automation.PropertyCondition]::new(
        [System.Windows.Automation.AutomationElement]::ClassNameProperty, $class)
    [System.Windows.Automation.AutomationElement]::RootElement.FindFirst(
        [System.Windows.Automation.TreeScope]::Children, $cond)
}

# An element's name, class, control type and place on the desktop; $null
# when it has no place there.
function Get-UiaInfo($e) {
    $name = $null
    $rect = $null
    try {
        $name = $e.Current.Name
        $rect = $e.Current.BoundingRectangle
        $class = $e.Current.ClassName
        $type = $e.Current.ControlType.ProgrammaticName
    } catch {
        return $null
    }
    if ($rect.IsEmpty -or $rect.Width -le 0 -or $rect.Height -le 0) { return $null }
    $box = [PanelRig.Box]::new([int]$rect.X, [int]$rect.Y, [int]($rect.X + $rect.Width), [int]($rect.Y + $rect.Height))
    [pscustomobject]@{
        Name  = "$name"
        Class = "$class"
        Type  = "$type"
        X     = $box.CenterX
        Y     = $box.CenterY
        Box   = $box
        Rect  = "$box"
        What  = "'$name' ($class, $type) $box"
    }
}

# Elements under $under whose name matches $pattern and that have a place
# on the desktop.
function Find-UiaNamed($under, [string]$pattern) {
    $found = @()
    if ($null -eq $under) { return $found }
    $all = $under.FindAll([System.Windows.Automation.TreeScope]::Descendants,
        [System.Windows.Automation.Condition]::TrueCondition)
    foreach ($e in $all) {
        $info = Get-UiaInfo $e
        if ($info -and $info.Name -and ($info.Name -match $pattern)) { $found += $info }
    }
    $found
}

# The app's tray icon at (x, y), by UI Automation: the element there, or one
# inside it whose place holds the point, if it is named as the tray icon is;
# $null otherwise. What else is there goes to rig.log.
function Get-TrayIconAt([int]$x, [int]$y) {
    $e = $null
    try {
        $e = [System.Windows.Automation.AutomationElement]::FromPoint([System.Windows.Point]::new($x, $y))
    } catch {
        Say "Nothing UI Automation knows of at $x,$y"
        return $null
    }
    $there = Get-UiaInfo $e
    if ($there -and ($there.Name -match $TrayPattern)) { return $there }
    $inner = @(Find-UiaNamed $e $TrayPattern | Where-Object { $_.Box.Contains($x, $y) }) | Select-Object -First 1
    if ($inner) { return $inner }
    Say "At $x,$y: $(if ($there) { $there.What } else { 'an element with no place' }), not the app's tray icon"
    $null
}

# ---------------------------------------------------------------------------
# The app

function Get-AppArguments([bool]$startup) {
    $argv = @("--source", "sine")
    if ($Network -eq "quiet") {
        $argv += @("--bind", "127.0.0.1", "--no-discovery", "--no-airplay", "--no-sendspin")
    }
    if ($startup) { $argv += "--startup" }
    $argv
}

# The app's process started after $since, waiting up to $seconds for it.
function Wait-AppProcess([datetime]$since, [int]$seconds) {
    $deadline = (Get-Date).AddSeconds($seconds)
    while ((Get-Date) -lt $deadline) {
        $p = @(Get-Process -Name ([IO.Path]::GetFileNameWithoutExtension($ExeName)) -ErrorAction SilentlyContinue |
            Where-Object { $_.StartTime -ge $since }) | Select-Object -First 1
        if ($p) {
            $null = $p.Handle
            return $p
        }
        Start-Sleep -Milliseconds 100
    }
    $null
}

# Start the app as the sign-in entry does: by a program, with --startup
# when $startup.
function Start-AppByProgram([bool]$startup, [string]$label) {
    $argv = Get-AppArguments $startup
    $p = Start-Process -FilePath $Exe -ArgumentList $argv -PassThru
    $null = $p.Handle
    Say "Started the app ($label) as a program would: pid $($p.Id), arguments: $($argv -join ' ')"
    $p
}

# Start the app as a person does: a double click on its shortcut on the
# desktop. Falls back to starting it as a program when the shortcut can't
# be found on the desktop.
function Start-AppByHand([string]$label) {
    $desktop = [Environment]::GetFolderPath("Desktop")
    $lnkPath = Join-Path $desktop "$ShortcutName.lnk"
    $argv = Get-AppArguments $false
    $shell = New-Object -ComObject WScript.Shell
    $lnk = $shell.CreateShortcut($lnkPath)
    $lnk.TargetPath = $Exe
    $lnk.Arguments = $argv -join ' '
    $lnk.WorkingDirectory = Split-Path -Parent $Exe
    $lnk.Save()
    Say "Shortcut on the desktop: $lnkPath ($($argv -join ' '))"
    $item = $null
    $deadline = (Get-Date).AddSeconds(15)
    while (-not $item -and (Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 500
        foreach ($class in "Progman", "WorkerW") {
            $top = Get-UiaTopLevel $class
            $item = @(Find-UiaNamed $top "^$([regex]::Escape($ShortcutName))$") | Select-Object -First 1
            if ($item) { break }
        }
    }
    if (-not $item) {
        Warn "the shortcut doesn't show on the desktop; the app was started as a program instead"
        return Start-AppByProgram $false $label
    }
    $since = Get-Date
    $at = [PanelRig.Mouse]::Click($item.X, $item.Y, 2)
    Say "Double-clicked the shortcut '$($item.Name)' ($($item.Rect)) at $at"
    $p = Wait-AppProcess $since.AddSeconds(-1) 20
    if (-not $p) { throw "the app didn't start from its shortcut" }
    Say "Started the app ($label) by hand: pid $($p.Id)"
    $p
}

function Find-TrayWindow([int]$appPid) {
    foreach ($w in [PanelRig.Windows]::Of([uint32]$appPid)) {
        if ($w.Class -eq "tray_icon_app") { return $w }
    }
    $null
}

# Where the taskbar says the app's icon is (null when it doesn't say).
function Get-IconRect([int]$appPid) {
    $w = Find-TrayWindow $appPid
    if (-not $w) { return $null }
    foreach ($uid in 1..8) {
        $r = [PanelRig.Windows]::TrayIconRect($w.Handle, [uint32]$uid)
        if ($r) { return $r }
    }
    $null
}

function Wait-TrayIcon($proc, [int]$seconds) {
    $deadline = (Get-Date).AddSeconds($seconds)
    while ((Get-Date) -lt $deadline) {
        if ($proc.HasExited) { throw "the app ended (exit code $($proc.ExitCode)) before its tray icon appeared" }
        if (Find-TrayWindow $proc.Id) {
            # The icon goes to the taskbar right after the window is made.
            Start-Sleep -Milliseconds 1500
            Say "The app's tray icon is there"
            return
        }
        Start-Sleep -Milliseconds 250
    }
    throw "no tray icon after $seconds s"
}

# Ask the app to close (its main window gets a close request, as at sign-out),
# then end it if it doesn't.
function Stop-App($proc) {
    if ($proc.HasExited) {
        Say "The app had ended already (exit code $($proc.ExitCode))"
        return
    }
    $asked = $proc.CloseMainWindow()
    if (-not $asked -or -not $proc.WaitForExit(15000)) {
        Say "The app didn't close when asked (the request $(if ($asked) { 'went out' } else { 'had no window to go to' })); ending it"
        $proc.Kill()
        $null = $proc.WaitForExit(5000)
    }
    Say "The app ended (exit code $($proc.ExitCode))"
}

function Save-AppLog([string]$label) {
    foreach ($name in "stream-to-speaker.log", "startup.txt") {
        $src = Join-Path $LogDir $name
        if (Test-Path -LiteralPath $src) {
            Copy-Item -LiteralPath $src (Join-Path $Out "$label-$name") -Force
        }
    }
}

# ---------------------------------------------------------------------------
# Where the icon sits

function Restart-Explorer {
    Say "Restarting Explorer"
    Get-Process explorer -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep -Seconds 3
    if (-not (Get-Process explorer -ErrorAction SilentlyContinue)) { Start-Process explorer.exe }
    $deadline = (Get-Date).AddSeconds(30)
    while ([PanelRig.Windows]::Taskbar() -eq [IntPtr]::Zero -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 500 }
    # The notification area fills in after the taskbar appears.
    Start-Sleep -Seconds 5
    Say "Explorer is back: taskbar $([PanelRig.Windows]::RectOf([PanelRig.Windows]::Taskbar()))"
}

# Windows 11: the per-program "show on the taskbar" switch Explorer keeps
# for every icon it has shown.
function Set-Promoted([int]$value) {
    $n = 0
    foreach ($k in @(Get-ChildItem -Path "HKCU:\Control Panel\NotifyIconSettings" -ErrorAction SilentlyContinue)) {
        $p = Get-ItemProperty -LiteralPath $k.PSPath
        $path = "$($p.ExecutablePath)"
        if ($path -and ([IO.Path]::GetFileName($path) -ieq $ExeName)) {
            Set-ItemProperty -LiteralPath $k.PSPath -Name IsPromoted -Value $value -Type DWord
            Say "NotifyIconSettings\$($k.PSChildName) ($path): IsPromoted = $value"
            $n++
        }
    }
    $n
}

# Where the icon is now. On the taskbar: the place the taskbar gives the
# app's icon (Shell_NotifyIconGetRect), and only if what UI Automation finds
# there is named as the tray icon is (a button of the notification area,
# never a taskbar button of one of the app's windows); else among the hidden
# icons. Throws rather than return any other place to click.
function Get-IconPlace([int]$appPid) {
    $bar = Get-UiaTopLevel "Shell_TrayWnd"
    $named = @(Find-UiaNamed $bar $TrayPattern)
    $rect = Get-IconRect $appPid
    $barRect = [PanelRig.Windows]::RectOf([PanelRig.Windows]::Taskbar())
    $rectOnBar = $rect -and $barRect -and $barRect.Contains($rect.CenterX, $rect.CenterY)
    Say "Icon: the taskbar's elements named as the tray icon: $(@($named | ForEach-Object { $_.What }) -join '; '); its place from the taskbar: $rect$(if ($rectOnBar) { ' (on the taskbar)' } else { '' })"
    # A hidden icon's place can be the button that shows the hidden icons:
    # that is not the icon on the taskbar.
    $chevron = @(Find-UiaNamed $bar '(?i)chevron|hidden icons') | Select-Object -First 1
    if ($rectOnBar -and $chevron -and $chevron.Box.Contains($rect.CenterX, $rect.CenterY)) {
        Say "The taskbar gives the icon the place of $($chevron.What)"
        $rectOnBar = $false
    }
    if (-not $rectOnBar) {
        return [pscustomobject]@{ Where = "hidden"; X = 0; Y = 0; What = "" }
    }
    $icon = Get-TrayIconAt $rect.CenterX $rect.CenterY
    if (-not $icon) {
        throw "the taskbar gives the app's icon the place $rect, but nothing there is named as the tray icon is ('$TrayTip...'): not clicking"
    }
    [pscustomobject]@{ Where = "taskbar"; X = $rect.CenterX; Y = $rect.CenterY; What = "the notification area's button $($icon.What)" }
}

# Click the button that shows the hidden icons; where the app's icon is
# among them: an element of the hidden icons' window named as the tray icon
# is, and found there again by its place.
function Open-HiddenIcons($rec) {
    $bar = Get-UiaTopLevel "Shell_TrayWnd"
    $chevron = @(Find-UiaNamed $bar '(?i)chevron|hidden icons') | Select-Object -First 1
    if (-not $chevron) { throw "found no button for the hidden icons on the taskbar" }
    $at = [PanelRig.Mouse]::Click($chevron.X, $chevron.Y, 1)
    $rec.Note("clicked $($chevron.What) at $at")
    Say "Clicked $($chevron.What) at $at"
    $deadline = (Get-Date).AddSeconds(5)
    while ((Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 300
        foreach ($class in "NotifyIconOverflowWindow", "TopLevelWindowForOverflowXamlIsland") {
            foreach ($hit in @(Find-UiaNamed (Get-UiaTopLevel $class) $TrayPattern)) {
                $icon = Get-TrayIconAt $hit.X $hit.Y
                if ($icon) {
                    return [pscustomobject]@{ Where = "hidden"; X = $icon.X; Y = $icon.Y; What = "the hidden icons' button $($icon.What) in $class" }
                }
            }
        }
    }
    $rec.Shot("hidden-icons-without-the-app")
    throw "the hidden icons opened without the app's icon among them ('$TrayTip...')"
}

# ---------------------------------------------------------------------------

$os = Get-CimInstance Win32_OperatingSystem
$build = [int]$os.BuildNumber
$win11Taskbar = $build -ge 22000
Say "Variation: click $Click, icon $Icon, panel $Panel, start $Start, network $Network, first run $([bool]$FirstRun)"
Say "Windows: $($os.Caption) $($os.Version), the taskbar of Windows $(if ($win11Taskbar) { '11' } else { '10' })"
Say "This job: session $((Get-Process -Id $PID).SessionId); $([PanelRig.Desk]::BecomeDpiAware())"
Say "Explorer: $(@(Get-Process explorer -ErrorAction SilentlyContinue | ForEach-Object { "pid $($_.Id) in session $($_.SessionId)" }) -join '; ')"
if ([PanelRig.Windows]::Taskbar() -eq [IntPtr]::Zero) {
    throw "this job's desktop has no taskbar (no Shell_TrayWnd window): there is no tray icon to click here"
}
Say "Desktop $([PanelRig.Desk]::Bounds()); taskbar $([PanelRig.Windows]::RectOf([PanelRig.Windows]::Taskbar())); double-click time $([PanelRig.Native]::GetDoubleClickTime()) ms"
Say "Foreground: $([PanelRig.Windows]::Foreground())"
if (-not (Test-Path -LiteralPath $Exe)) { throw "no app at $Exe" }

$app = $null
$rec = [PanelRig.Recorder]::new($PanelTitle, $RootTitle)
try {
    $rec.Shot("desktop-at-the-start")

    Set-AppEnvironment $true
    Say "The app's environment: $(@($AppEnvironment.Keys | ForEach-Object { "$_=$($AppEnvironment[$_])" }) -join '; ')"

    # Settings: none (first run), or those of someone who finished first run.
    if (Test-Path -LiteralPath $SettingsDir) { Remove-Item -LiteralPath $SettingsDir -Recurse -Force }
    if (-not $FirstRun) {
        New-Item -ItemType Directory -Force -Path $SettingsDir | Out-Null
        Set-Content -LiteralPath (Join-Path $SettingsDir "config.json") -Value '{"onboarding_dismissed": true}' -Encoding Ascii
        Say "Settings: first run finished before"
    } else {
        Say "Settings: none (first run)"
    }
    if (Test-Path -LiteralPath $LogDir) { Remove-Item -LiteralPath $LogDir -Recurse -Force }

    # Where the icon will sit.
    if (-not $win11Taskbar) {
        $key = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Explorer"
        $have = (Get-ItemProperty -Path $key -Name EnableAutoTray -ErrorAction SilentlyContinue).EnableAutoTray
        if ($null -eq $have) { $have = 1 }
        $want = if ($Icon -eq "taskbar") { 0 } else { 1 }
        Say "Windows 10's 'hide icons' setting (EnableAutoTray): $have, wanted $want"
        if ($have -ne $want) {
            Set-ItemProperty -Path $key -Name EnableAutoTray -Value $want -Type DWord
            Restart-Explorer
        }
    } else {
        Say "Windows 11 keeps a setting per program for whether its icon shows on the taskbar; Explorer makes it the first time it shows the icon: a first start for it"
        $prime = Start-AppByProgram $true "first start, for the icon's setting"
        Wait-TrayIcon $prime 90
        Start-Sleep -Seconds 2
        $n = Set-Promoted $(if ($Icon -eq "taskbar") { 1 } else { 0 })
        Stop-App $prime
        Save-AppLog "first-start"
        if ($n -eq 0) { Warn "Explorer made no setting for the icon; it stays where Windows puts new icons" }
        Remove-Item -LiteralPath $LogDir -Recurse -Force -ErrorAction SilentlyContinue
        Start-Sleep -Seconds 2
    }

    # Start.
    if ($Start -eq "menu") {
        $app = Start-AppByHand "the run"
        $rec.Follow([uint32]$app.Id)
        $rec.Note("the app started by hand, pid $($app.Id)")
    } else {
        $app = Start-AppByProgram $true "the run"
        $rec.Follow([uint32]$app.Id)
        $rec.Note("the app started with --startup, pid $($app.Id)")
    }
    # The panel opens by itself when started by hand, and at first run.
    $rec.Burst("start", 30, 100)
    $rec.Save($PictureDir)
    Wait-TrayIcon $app 90
    Start-Sleep -Seconds 2
    $place = Get-IconPlace $app.Id
    Say "The icon is $(if ($place.Where -eq 'taskbar') { 'on the taskbar' } else { 'among the hidden icons' }) (wanted: $Icon)"
    if ($place.Where -ne $Icon) { Warn "the icon is $(if ($place.Where -eq 'taskbar') { 'on the taskbar' } else { 'among the hidden icons' }), not where this case wanted it; clicked where it is" }
    $rec.Shot("before-the-click")

    # The click.
    $target = $place
    if ($place.Where -eq "hidden") {
        $target = Open-HiddenIcons $rec
        $rec.Shot("hidden-icons-open")
    }
    $times = if ($Click -eq "double") { 2 } else { 1 }
    $at = [PanelRig.Mouse]::Click($target.X, $target.Y, $times)
    $script:Clicked = "$($target.What), $times time(s), with the pointer at $at"
    $rec.Note("clicked $script:Clicked")
    Say "Clicked $script:Clicked"
    $rec.Burst("after-the-click", 21, 100)
    $rec.Save($PictureDir)

    # A click inside the panel, near the top of its inside.
    $inside = $rec.PanelClient()
    if ($rec.PanelShown() -and $inside -and -not $inside.IsEmpty) {
        $x = $inside.CenterX
        $y = $inside.Top + 4
        $at = [PanelRig.Mouse]::Click($x, $y, 1)
        $rec.Note("clicked inside the panel at $at")
        Say "Clicked inside the panel ($inside) at $at"
        $rec.Burst("after-the-click-inside", 11, 100)
        $rec.Save($PictureDir)
    } else {
        Say "The panel's window isn't on screen two seconds after the click: no click inside it"
    }
    $rec.Shot("the-end")
} finally {
    try { $rec.Save($PictureDir) } catch { Say "Couldn't save the pictures: $_" }
    $rec.Lines | Set-Content -LiteralPath (Join-Path $Out "timeline.txt") -Encoding UTF8
    if ($app) {
        try { Stop-App $app } catch { Say "Couldn't stop the app: $_" }
    }
    Save-AppLog "app"
    Set-AppEnvironment $false
    Remove-Item -LiteralPath (Join-Path ([Environment]::GetFolderPath("Desktop")) "$ShortcutName.lnk") -ErrorAction SilentlyContinue

    # The step summary: the variation, what the pictures show, the commands
    # the windows got, the shell's lines from the app's log.
    if ($env:GITHUB_STEP_SUMMARY) {
        $appLog = Join-Path $Out "app-stream-to-speaker.log"
        $shellLines = @()
        $commands = "no log"
        if (Test-Path -LiteralPath $appLog) {
            $shellLines = @(Select-String -LiteralPath $appLog -Pattern '::shell' | ForEach-Object { $_.Line })
            $counts = foreach ($c in "Visible(true)", "Visible(false)", "Focus") {
                "$c $(@(Select-String -LiteralPath $appLog -SimpleMatch -Pattern "Processing ViewportCommand::$c").Count)"
            }
            $commands = $counts -join ", "
        }
        $summary = @(
            "### Panel click: $Click click, icon $Icon, $Panel, start $Start, network $Network$(if ($FirstRun) { ', first run' })"
            ""
            "$($os.Caption) (build $build). Pictures with the panel's window on screen: $($rec.ShownCount) of $($rec.PictureCount). Commands the app's windows got: $commands."
            ""
            "Clicked: $script:Clicked."
            ""
        ) + @($script:Warnings | ForEach-Object { "- Not as the case asked: $_" }) + @(
            ""
            "The shell's lines in the app's log:"
            ""
            '```'
        ) + $shellLines + @('```')
        $summary | Add-Content -LiteralPath $env:GITHUB_STEP_SUMMARY -Encoding UTF8
    }
}
Say "Done"
