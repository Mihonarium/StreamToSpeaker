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
; Override on the command line: ISCC /DAppVersion=0.1.0
#ifndef AppVersion
  #define AppVersion     "0.1.0"
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
VersionInfoVersion={#AppVersion}
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
Source: "{#SourcePath}\..\scripts\Rename-Endpoint.ps1"; DestDir: "{app}\scripts"; Flags: ignoreversion
Source: "{#SourcePath}\Uninstall-Driver.ps1";           DestDir: "{app}\scripts"; Flags: ignoreversion

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
; 1) Install the driver package. pnputil stages it into DriverStore and,
;    when the INF describes a Root-enumerated device on Win10 1809+,
;    creates the device automatically. Output captured via /subst-args.
Filename: "{sys}\pnputil.exe"; \
    Parameters: "/add-driver ""{app}\driver\StreamToSpeaker.inf"" /install"; \
    StatusMsg: "Installing audio driver..."; \
    Flags: runhidden waituntilterminated

; 2) Overwrite the cached endpoint friendly name (the Windows registry
;    keeps the auto-generated "Internal AUX Jack ..." string across
;    reinstalls; this script writes the same slot Sound Settings'
;    Rename button writes to).
Filename: "{sys}\WindowsPowerShell\v1.0\powershell.exe"; \
    Parameters: "-NoProfile -ExecutionPolicy Bypass -File ""{app}\scripts\Rename-Endpoint.ps1"""; \
    StatusMsg: "Naming the audio endpoint..."; \
    Flags: runhidden waituntilterminated

; 3) Offer to launch the app at the end of install.
Filename: "{app}\{#MyAppExeName}"; Description: "Launch {#MyAppName}"; \
    Flags: nowait postinstall skipifsilent

[UninstallRun]
; Kill any running instance before yanking the driver out from under it.
Filename: "{sys}\taskkill.exe"; Parameters: "/F /IM {#MyAppExeName}"; \
    Flags: runhidden; RunOnceId: "KillService"

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
