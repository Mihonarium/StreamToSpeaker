; -----------------------------------------------------------------------------
; Stream To Speaker — Inno Setup script.
;
; Produces a single-file installer that:
;   1. Copies stream-to-speaker.exe to Program Files\Stream To Speaker
;   2. Drops the driver package (.sys + .inf [+ .cat]) into a subdir
;   3. Runs `pnputil /add-driver` to stage + install the driver (Win10 1809+
;      auto-installs root-enumerated devices when the INF matches)
;   4. Runs the bundled PowerShell script to overwrite the cached
;      "Internal AUX Jack — Stream To Speaker" name in the registry
;   5. Creates a Start Menu shortcut and (opt-in) an auto-start entry
;   6. On uninstall: kills any running stream-to-speaker.exe, removes
;      the driver via pnputil (looked up by Original Name), deletes files
;
; Build with: ISCC.exe installer\StreamToSpeaker.iss
; Output:     installer\out\StreamToSpeakerSetup-<ver>.exe
;
; Requirements before running the installer on a target machine:
;   - Windows 10 1809+ (the driver INF targets that)
;   - Test signing on, Secure Boot off, HVCI off — unless the driver
;     binary is WHQL-signed (not yet)
; -----------------------------------------------------------------------------

#define MyAppName        "Stream To Speaker"
#define MyAppShortName   "StreamToSpeaker"
#define MyAppExeName     "stream-to-speaker.exe"
; AppVersion: free-form (e.g. "0.1.0", "0.1.0-rc.1", "0.1.0-abcdef0").
; Override on the command line: ISCC /DAppVersion=0.1.0
#ifndef AppVersion
  #define AppVersion     "0.1.0"
#endif
; VersionInfoVersion: strict X.Y.Z.W numeric — required by the Win32
; resource format. The build script / CI derives this from AppVersion's
; numeric prefix and passes it separately so pre-release suffixes
; ("-rc.1", "-abc1234") don't break the compile.
#ifndef VersionInfoVersion
  #define VersionInfoVersion "0.1.0.0"
#endif
#define MyAppPublisher   "Stream To Speaker"
#define MyAppURL         "https://github.com/Mihonarium/StreamToSpeaker"

[Setup]
; AppId is a fixed GUID so future installer versions detect us as an upgrade
; instead of installing side-by-side. Generated once; do not change.
AppId={{8A6C7F92-3E1B-4A2B-9F1C-77F1B6B2A6E0}
AppName={#MyAppName}
AppVersion={#AppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}/issues
AppUpdatesURL={#MyAppURL}/releases
VersionInfoVersion={#VersionInfoVersion}
DefaultDirName={autopf}\{#MyAppName}
DefaultGroupName={#MyAppName}
DisableProgramGroupPage=yes
LicenseFile={#SourcePath}\..\LICENSE
OutputDir={#SourcePath}\out
OutputBaseFilename={#MyAppShortName}Setup-{#AppVersion}
Compression=lzma2
SolidCompression=yes
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.17763
WizardStyle=modern
UninstallDisplayIcon={app}\{#MyAppExeName}
SetupLogging=yes
; pnputil + devcon sometimes set the system-wide "reboot needed" flag
; after staging / installing drivers, which Inno Setup detects and
; surfaces as a "Setup needs to restart your computer" prompt. For our
; install path we don't actually need a reboot — devcon creates the
; device live, Pre-Install.ps1 wipes the cached MMDevices entries,
; Rename-Endpoint.ps1 uses IPolicyConfig to apply state authoritatively
; through audiosrv. Suppress the prompt.
RestartIfNeededByRun=no
AlwaysRestart=no
; If an instance of stream-to-speaker.exe is running when the user
; reinstalls / upgrades, Setup needs to close it before it can replace
; the binary. force = show the "the following applications should be
; closed" page with Close-and-retry / Ignore controls AND actually run
; the close. The filter limits the Restart Manager scan to our binaries
; under {app}; without the filter Inno would scan the whole system.
; RestartApplications=no — we don't want Setup re-launching the GUI in
; the SYSTEM elevated context after install (that creates a permissions
; mess for the per-user "Run on sign-in" autostart entry).
CloseApplications=force
CloseApplicationsFilter=*.exe
RestartApplications=no
; Belt and braces — if the GUI somehow holds no files (e.g. it's running
; from a different path, or it has no handles open to {app}), Restart
; Manager won't find it. AppMutex matches the exact named mutex the
; service creates at startup (see service/src/main.rs); when it's held,
; Setup shows a "Stream To Speaker is running — close it and click OK"
; dialog with Cancel / Retry buttons. Global\ so the elevated installer
; can see a mutex created in the user session.
AppMutex=Global\StreamToSpeaker.Singleton

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "startmenuicon"; Description: "Create a Start Menu shortcut";  GroupDescription: "Shortcuts:"
Name: "desktopicon";   Description: "Create a desktop shortcut";       GroupDescription: "Shortcuts:"; Flags: unchecked
Name: "autostart";     Description: "Start {#MyAppName} when I sign in"; GroupDescription: "Startup:";   Flags: unchecked

[Files]
; All artifacts come from installer\staging\ — populated by
; installer\build-installer.ps1 (or the CI workflow's staging step)
; before ISCC runs. This decouples the .iss from WDK / Cargo output
; path conventions that vary by toolchain version.

; --- Service binary ---
Source: "{#SourcePath}\staging\{#MyAppExeName}"; DestDir: "{app}"; Flags: ignoreversion

; --- Driver package ---
Source: "{#SourcePath}\staging\StreamToSpeaker.sys"; DestDir: "{app}\driver"; Flags: ignoreversion
Source: "{#SourcePath}\staging\StreamToSpeaker.inf"; DestDir: "{app}\driver"; Flags: ignoreversion
; The .cat file only exists when the driver has been signed.
; skipifsourcedoesntexist means the installer still builds against an
; unsigned development build (pnputil install will fail on the user
; machine, but that's a separate concern).
Source: "{#SourcePath}\staging\StreamToSpeaker.cat"; DestDir: "{app}\driver"; Flags: ignoreversion skipifsourcedoesntexist

; --- Scripts ---
Source: "{#SourcePath}\..\scripts\Pre-Install.ps1";     DestDir: "{app}\scripts"; Flags: ignoreversion
Source: "{#SourcePath}\..\scripts\Rename-Endpoint.ps1"; DestDir: "{app}\scripts"; Flags: ignoreversion
Source: "{#SourcePath}\..\scripts\Reset-Install.ps1";   DestDir: "{app}\scripts"; Flags: ignoreversion
Source: "{#SourcePath}\..\scripts\Diagnose.ps1";        DestDir: "{app}\scripts"; Flags: ignoreversion
Source: "{#SourcePath}\Uninstall-Driver.ps1";           DestDir: "{app}\scripts"; Flags: ignoreversion

; --- devcon.exe (WDK redistributable, MIT) ---
; Used to add the root-enumerated StreamToSpeaker device after the
; driver package is staged. pnputil /add-driver /install stages
; drivers and matches them against EXISTING devices; root-enumerated
; devices have to be inserted explicitly with devcon (or the
; SetupDi API). Without this step the driver sits in the store but
; no audio endpoint ever appears in Sound Settings.
Source: "{#SourcePath}\staging\devcon.exe"; DestDir: "{app}\driver"; Flags: ignoreversion

; --- Test-signing certificate ---
; The CI build signs the driver with a self-generated test cert and
; ships the public .cer here. Imported to TrustedPublisher + Root on
; the target machine at install time so Windows (in test-signing
; mode) accepts our driver. skipifsourcedoesntexist so a local
; unsigned build still produces an installer.
Source: "{#SourcePath}\staging\StreamToSpeaker.cer"; DestDir: "{app}\driver"; Flags: ignoreversion skipifsourcedoesntexist

; --- Docs ---
Source: "{#SourcePath}\..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourcePath}\..\LICENSE";   DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#MyAppName}";         Filename: "{app}\{#MyAppExeName}"; Tasks: startmenuicon
Name: "{commondesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Registry]
; Opt-in autostart — runs the GUI when the user signs in. The default
; mode (no flags) opens the window + adds the system tray; users who
; ticked this task probably want minimised-to-tray UX. We add /no-tray
; OFF (i.e. default tray-on) and rely on the window being closable to
; the tray. A future iteration could add a --minimized flag.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; \
    ValueType: string; ValueName: "{#MyAppName}"; \
    ValueData: """{app}\{#MyAppExeName}"""; \
    Tasks: autostart; Flags: uninsdeletevalue

[Run]
; 0) Clean up any prior install BEFORE we touch the driver store /
;    create a new device. Idempotent: if nothing prior is installed,
;    silently no-ops. This is what unblocks the "upgrade keeps the
;    old INF properties (Internal AUX Jack label, disabled-by-default
;    flag, ...)" problem — the cached MMDevices entry has to be wiped
;    so the new INF takes effect.
Filename: "{sys}\WindowsPowerShell\v1.0\powershell.exe"; \
    Parameters: "-NoProfile -ExecutionPolicy Bypass -File ""{app}\scripts\Pre-Install.ps1"""; \
    StatusMsg: "Cleaning up previous install..."; \
    Flags: runhidden waituntilterminated

; 1) Import the test-signing certificate to TrustedPublisher AND Root
;    so Windows (in test-signing mode) accepts our test-signed driver.
;    Only runs if the .cer was bundled (CI build does; manual unsigned
;    local builds skip this step and just hope the user has their
;    own signing trust set up).
Filename: "{sys}\certutil.exe"; \
    Parameters: "-addstore -f TrustedPublisher ""{app}\driver\StreamToSpeaker.cer"""; \
    StatusMsg: "Trusting driver signature..."; \
    Flags: runhidden waituntilterminated skipifdoesntexist; \
    Check: FileExists(ExpandConstant('{app}\driver\StreamToSpeaker.cer'))
Filename: "{sys}\certutil.exe"; \
    Parameters: "-addstore -f Root ""{app}\driver\StreamToSpeaker.cer"""; \
    StatusMsg: "Trusting driver signature..."; \
    Flags: runhidden waituntilterminated skipifdoesntexist; \
    Check: FileExists(ExpandConstant('{app}\driver\StreamToSpeaker.cer'))

; 2) Stage the driver package in the DriverStore.
Filename: "{sys}\pnputil.exe"; \
    Parameters: "/add-driver ""{app}\driver\StreamToSpeaker.inf"" /install"; \
    StatusMsg: "Staging audio driver..."; \
    Flags: runhidden waituntilterminated

; 3) Insert the root-enumerated device. This is what makes "Stream To
;    Speaker" appear in Windows Sound Settings — pnputil only stages
;    the driver, the device still has to be created. devcon install
;    is idempotent: if the device already exists it just no-ops.
Filename: "{app}\driver\devcon.exe"; \
    Parameters: "install ""{app}\driver\StreamToSpeaker.inf"" Root\StreamToSpeaker"; \
    StatusMsg: "Creating audio device..."; \
    Flags: runhidden waituntilterminated

; 4) Overwrite the cached endpoint friendly name (the Windows registry
;    keeps the auto-generated "Internal AUX Jack ..." string across
;    reinstalls; this script writes the same slot Sound Settings'
;    Rename button writes to).
Filename: "{sys}\WindowsPowerShell\v1.0\powershell.exe"; \
    Parameters: "-NoProfile -ExecutionPolicy Bypass -File ""{app}\scripts\Rename-Endpoint.ps1"""; \
    StatusMsg: "Naming the audio endpoint..."; \
    Flags: runhidden waituntilterminated

; 5) Offer to launch the app at the end of install.
Filename: "{app}\{#MyAppExeName}"; Description: "Launch {#MyAppName}"; \
    Flags: nowait postinstall skipifsilent

[UninstallRun]
; Kill any running instance before yanking the driver out from under it.
Filename: "{sys}\taskkill.exe"; Parameters: "/F /IM {#MyAppExeName}"; \
    Flags: runhidden; RunOnceId: "KillService"

; Remove the device(s) so the driver isn't pinned by an active node.
Filename: "{app}\driver\devcon.exe"; Parameters: "remove Root\StreamToSpeaker"; \
    Flags: runhidden waituntilterminated; RunOnceId: "RemoveDevice"

; Remove the driver package. pnputil /delete-driver expects the OEM-
; assigned name (oemNN.inf) which we don't know up front, so we shell
; into a PowerShell that enumerates the driver store, finds the entry
; whose OriginalName matches StreamToSpeaker.inf, and uninstalls it.
Filename: "{sys}\WindowsPowerShell\v1.0\powershell.exe"; \
    Parameters: "-NoProfile -ExecutionPolicy Bypass -File ""{app}\scripts\Uninstall-Driver.ps1"""; \
    Flags: runhidden waituntilterminated; RunOnceId: "RemoveDriver"

[Code]
function InitializeSetup(): Boolean;
var
  Major, Minor, Build: Cardinal;
  Version: TWindowsVersion;
begin
  Result := True;
  GetWindowsVersionEx(Version);
  Major := Version.Major;
  Minor := Version.Minor;
  Build := Version.Build;
  if (Major < 10) or ((Major = 10) and (Build < 17763)) then begin
    MsgBox('Stream To Speaker requires Windows 10 build 17763 (1809) or later. ' + #13#10 +
           'Detected: ' + IntToStr(Major) + '.' + IntToStr(Minor) + '.' + IntToStr(Build),
           mbError, MB_OK);
    Result := False;
  end;
end;
