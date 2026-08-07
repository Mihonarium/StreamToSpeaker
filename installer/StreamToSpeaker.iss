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
;   - For TEST-SIGNED builds (staging contains StreamToSpeaker.cer):
;     test signing on, Secure Boot off, HVCI off
;   - For Microsoft-ATTESTED builds (release CI stages the attestation-
;     signed driver package and NO .cer): none of the above — the driver
;     loads on stock Windows 10/11 with Secure Boot on. The cert-import
;     steps below are gated on the .cer existing, so the same script
;     serves both variants. (Windows Server won't load attestation-signed
;     drivers; that was never a target.)
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
; The wizard's license page shows the terms for what this installer
; actually delivers: the signed binaries (all rights reserved — see
; LICENSE-BINARIES.md) plus the MPL-2.0 source-availability notice
; that MPL §3.2 requires for executable-form distribution. Showing
; the raw MPL here would misstate the user's rights over the signed
; binaries; showing the binary terms alone would omit the source
; notice. LICENSE-INSTALLER.txt covers both, plus third-party credits.
LicenseFile={#SourcePath}\LICENSE-INSTALLER.txt
OutputDir={#SourcePath}\out
OutputBaseFilename={#MyAppShortName}Setup-{#AppVersion}
Compression=lzma2
SolidCompression=yes
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.17763
WizardStyle=modern
; The setup executable's own icon, and the icon shown in Add/Remove
; Programs. The installed shortcuts pick theirs up from the exe's embedded
; resource (service/build.rs), so all four surfaces show the same mark.
SetupIconFile={#SourcePath}\..\assets\StreamToSpeaker.ico
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
; (AppMutex removed — replaced by the ClosePriorInstance Pascal
; function in [Code] below, which offers an explicit "Close it for me"
; button instead of Inno Setup's default "please close it manually"
; dialog. The mutex itself is still created by the service.)

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
; LICENSE (MPL-2.0) covers the source; LICENSE-BINARIES.md covers the
; signed artifacts; LICENSE-INSTALLER.txt is the combined summary the
; user accepted on the wizard's license page. Install all three so the
; accepted terms stay on disk.
Source: "{#SourcePath}\..\README.md";           DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourcePath}\..\LICENSE";             DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourcePath}\..\LICENSE-BINARIES.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourcePath}\LICENSE-INSTALLER.txt";  DestDir: "{app}"; Flags: ignoreversion

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

// Detects a running Stream To Speaker process via its named
// singleton mutex. Offers the user an explicit "Close it for me"
// choice instead of the default Inno-Setup "please close it
// manually" dialog. Returns False if the user cancels.
function ClosePriorInstance(): Boolean;
var
  ResultCode: Integer;
  Attempts: Integer;
begin
  Result := True;
  if not CheckForMutexes('Global\StreamToSpeaker.Singleton') then
    Exit; // Not running, nothing to do.
  case MsgBox('Stream To Speaker is currently running.' + #13#10 + #13#10 +
              'It must close before the installer can update the driver and binaries.' + #13#10 + #13#10 +
              'Close it now?',
              mbConfirmation, MB_YESNO) of
    IDYES:
    begin
      // Force-close via taskkill /F. We skip the graceful
      // (WM_CLOSE) path because the GUI handles WM_CLOSE by
      // popping its "Quit or minimise to tray?" modal — which
      // never gets dismissed while the user is over here in the
      // installer. /F is rude but reliable, and the app's only
      // persisted state (user_config, last_speaker_id) is saved
      // on every change rather than at exit, so nothing is lost.
      Exec(ExpandConstant('{sys}\taskkill.exe'),
           '/F /IM stream-to-speaker.exe',
           '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
      // Mutex release isn't synchronous; wait up to ~5 s.
      Attempts := 0;
      while CheckForMutexes('Global\StreamToSpeaker.Singleton') and (Attempts < 10) do begin
        Sleep(500);
        Attempts := Attempts + 1;
      end;
      Result := True;
    end;
    IDNO:
    begin
      Result := False;
    end;
  end;
end;

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
    Exit;
  end;
  if not ClosePriorInstance() then begin
    Result := False;
  end;
end;
