; -----------------------------------------------------------------------------
; Stream To Speaker: Inno Setup script.
;
; What Setup does, in order:
;   1. Refuses Windows Server, where the driver doesn't run, before it changes
;      anything (PrepareToInstall; /ALLOWSERVER lets the installer lifecycle
;      test run on its Windows Server machines).
;   2. If Stream To Speaker is running, in this account or another one, asks
;      to close it (only once the person has clicked Install; a silent install
;      closes it without asking) and closes it.
;   3. Makes the install folder writable by administrators only when it is
;      outside Program Files: the uninstaller later runs files from it as
;      administrator.
;   4. Copies the app, the driver package (.sys, .inf, .cat, devcon.exe and,
;      in test builds only, the test certificate) and the scripts.
;   5. Runs scripts\Install-Driver.ps1, which installs the driver and its
;      sound output without disturbing a device that already has this
;      driver (see that script), and reads back what happened.
;   6. Adds a Windows Firewall rule for the app (scripts\Set-FirewallRule.ps1).
;   7. Turns "start at sign-in" on or off for the account that ran Setup.
;   8. Says on its last page what happened, asks for a restart when the new
;      driver needs one, and returns exit code 20 to silent installs when the
;      sound device isn't working.
; Uninstall closes the app, removes the device, the driver packages,
; Windows' cached sound output entries, the test certificates, the firewall
; rules and the start-at-sign-in entries of signed-in accounts. Settings
; and speakers (each account's AppData\Roaming\StreamToSpeaker) stay.
;
; Build with installer\build-installer.ps1, or: ISCC.exe installer\StreamToSpeaker.iss
; after staging the files in installer\staging\.
; Output:     installer\out\StreamToSpeakerSetup-<ver>.exe
;
; Exit codes besides Inno Setup's own: 20 = installed, but the sound device
; isn't working (the reason is in Setup's log and in
; <install dir>\logs\install.log). A restart that is needed returns 0
; unless /RESTARTEXITCODE= says otherwise (3010 is the usual choice).
;
; Microsoft-signed builds (the release ones) ship no .cer and load on stock
; Windows 10/11 with Secure Boot on. Test-signed builds ship the .cer
; that signed their driver, which loads only while Windows' test-signing
; mode is on. The same script builds both.
; -----------------------------------------------------------------------------

#if Ver < EncodeVer(6, 3, 0)
  #error Inno Setup 6.3 or later is needed (x64os, ExecAndLogOutput, logoutput)
#endif

#define MyAppName        "Stream To Speaker"
#define MyAppShortName   "StreamToSpeaker"
#define MyAppExeName     "stream-to-speaker.exe"
; AppVersion: free-form (e.g. "0.1.0", "0.1.0-rc.1", "0.1.0-abcdef0").
; Override on the command line: ISCC /DAppVersion=0.1.0
#ifndef AppVersion
  #define AppVersion     "0.1.0"
#endif
; VersionInfoVersion: strict X.Y.Z.W numeric, required by the Win32
; resource format. The build script / CI derives this from AppVersion's
; numeric prefix and passes it separately so pre-release suffixes
; ("-rc.1", "-abc1234") don't break the compile.
#ifndef VersionInfoVersion
  #define VersionInfoVersion "0.1.0.0"
#endif
#define MyAppPublisher   "Stream To Speaker"
#define MyAppURL         "https://github.com/Mihonarium/StreamToSpeaker"
; Whether this build ships a test certificate, which makes it a test build.
#define TestSignedBuild  FileExists(AddBackslash(SourcePath) + "staging\StreamToSpeaker.cer")

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
; actually delivers: the signed binaries (all rights reserved; see
; LICENSE-BINARIES.md) plus the MPL-2.0 source-availability notice
; that MPL 3.2 requires for executable-form distribution. Showing
; the raw MPL here would misstate the user's rights over the signed
; binaries; showing the binary terms alone would omit the source
; notice. LICENSE-INSTALLER.txt covers both, plus third-party credits.
LicenseFile={#SourcePath}\LICENSE-INSTALLER.txt
OutputDir={#SourcePath}\out
OutputBaseFilename={#MyAppShortName}Setup-{#AppVersion}
Compression=lzma2
SolidCompression=yes
PrivilegesRequired=admin
; x64os, not x64compatible: the kernel driver is x64-only, and
; x64compatible would also admit ARM64 hosts (where x64 apps run under
; emulation but an x64 .sys cannot load). Requires Inno Setup 6.3+.
ArchitecturesAllowed=x64os
ArchitecturesInstallIn64BitMode=x64os
MinVersion=10.0.17763
WizardStyle=modern
; The setup executable's own icon, and the icon shown in Add/Remove
; Programs. The installed shortcuts pick theirs up from the exe's embedded
; resource (service/build.rs), so all four surfaces show the same mark.
SetupIconFile={#SourcePath}\..\assets\StreamToSpeaker.ico
UninstallDisplayIcon={app}\{#MyAppExeName}
SetupLogging=yes
; An uninstall started from Settings writes a log too (in %TEMP%), with the
; output of each [UninstallRun] step.
UninstallLogging=yes
; pnputil and devcon can set Windows' "restart needed" flag after staging
; or installing a driver, which Inno Setup would turn into a restart
; prompt. Whether a restart is needed is decided from what
; Install-Driver.ps1 reports (NeedRestart in [Code]): only when the driver
; Windows has loaded isn't the one just installed.
RestartIfNeededByRun=no
AlwaysRestart=no
; Setup closes a running Stream To Speaker itself, in PrepareToInstall
; (see CloseRunningCopies). Restart Manager stays on as the fallback for
; any other program from {app} that holds a file Setup replaces.
CloseApplications=force
CloseApplicationsFilter=*.exe
RestartApplications=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Messages]
WinVersionTooLowError=Stream To Speaker needs Windows 10 version 1809 or later, or Windows 11.
OnlyOnTheseArchitectures=Stream To Speaker can't be installed on this PC. Its sound driver needs 64-bit Windows on an Intel or AMD processor; it doesn't run on ARM processors or on 32-bit Windows.
FinishedRestartLabel=Stream To Speaker is installed. Its new sound driver starts once Windows restarts; until then, the driver it replaces keeps running. Restart now?
FinishedRestartMessage=Stream To Speaker is installed. Its new sound driver starts once Windows restarts; until then, the driver it replaces keeps running.%n%nRestart now?
UninstalledAll=%1 was removed from this PC.%n%nEach account's settings and speakers stay in its AppData\Roaming\StreamToSpeaker folder, so installing it again brings them back. To remove them too, delete that folder.
UninstalledAndNeedsRestart=%1 was removed from this PC. Its sound driver stays loaded until Windows restarts.%n%nEach account's settings and speakers stay in its AppData\Roaming\StreamToSpeaker folder. To remove them too, delete that folder.%n%nRestart now?

[CustomMessages]
CloseInstruction=Close Stream To Speaker to update it?
CloseText=Stream To Speaker is running. Setup closes it to update it; streaming stops until it starts again at the end of Setup.
CloseTextOtherAccount=Stream To Speaker is running in another account on this PC. Setup closes it there to update it; in that account it starts again only when someone starts it there.
CloseButton=Close Stream To Speaker and install
CloseFailed=Setup couldn't close Stream To Speaker, so nothing was changed.%n%nQuit Stream To Speaker (right-click its icon in the notification area, then choose Quit), then run Setup again.
CloseFailedOtherAccount=Setup couldn't close Stream To Speaker, which runs in another account on this PC, so nothing was changed.%n%nAsk the person signed in to that account to quit it, or restart the PC, then run Setup again.
ServerRefused=Stream To Speaker can't be installed on Windows Server: its sound driver runs only on Windows 10 and Windows 11. Nothing was changed.
FolderNotSecured=Setup couldn't make the folder %1 safe to install to (only administrators may change the files Setup runs as administrator), so nothing was installed.%n%nChoose a folder in Program Files, or run Setup again.
StatusDriver=Installing the Stream To Speaker sound driver...
StatusFirewall=Adding the Windows Firewall rule for Stream To Speaker...
DoneNew=Stream To Speaker is installed.%n%nNext: make Stream To Speaker the sound output in Windows (select the speaker icon on the taskbar and choose it), then pick a speaker in the app.
DoneUpdated=Stream To Speaker is installed. Its sound output and your sound settings in Windows are as they were.
DoneUpdatedNewOutput=Stream To Speaker is installed. Its sound output in Windows was set up again: if sound now plays from this PC, make Stream To Speaker the sound output again (select the speaker icon on the taskbar and choose it).
StageFailedKept=Stream To Speaker is installed, but Windows didn't accept its sound driver, so this PC keeps the driver it had.%n%nRestart the PC and run Setup again. Details are in %1.
StageFailed=Stream To Speaker is installed, but Windows didn't accept its sound driver, so it can't play this PC's sound yet.%n%nRestart the PC and run Setup again. Details are in %1.
TrustFailed=Stream To Speaker is installed, but Setup couldn't make Windows trust this test build's driver, so the driver isn't installed.%n%nDetails are in %1.
DeviceFailed=Stream To Speaker is installed, but Windows couldn't set up its sound device.%n%nRestart the PC and run Setup again. Details are in %1.
NotStartedTestBuild=Stream To Speaker is installed, but Windows didn't start its sound driver: this is a test build, and its driver starts only while Windows' test-signing mode is on.%n%nInstall the release from github.com/Mihonarium/StreamToSpeaker/releases instead.
NotStartedSignature=Stream To Speaker is installed, but Windows didn't accept its sound driver's signature, so the driver didn't start.%n%nDetails are in %1.
NotStartedPolicy=Stream To Speaker is installed, but a policy on this PC blocks its sound driver. Ask whoever manages this PC to allow it.%n%nDetails are in %1.
NotStartedTurnedOff=Stream To Speaker is installed, but its sound device is turned off in Device Manager. Turn on Stream To Speaker there, under Sound, video and game controllers.
NotStarted=Stream To Speaker is installed, but Windows didn't start its sound driver (Device Manager shows problem code %2).%n%nRestart the PC and run Setup again. Details are in %1.
EndpointMissing=Stream To Speaker is installed and its driver runs, but Windows doesn't show its sound output yet.%n%nRestart the PC. If the output is still missing, run Setup again; details are in %1.
EndpointOff=Stream To Speaker is installed, but its sound output is turned off in Windows.%n%nOpen Sound settings, choose Stream To Speaker and allow it, then pick a speaker in the app.
ScriptFailed=Stream To Speaker's files are installed, but Setup couldn't finish setting up its sound driver, so it can't tell whether the sound output works.%n%nRestart the PC and run Setup again. Details are in %1.
FirewallFailed=Setup couldn't add the Windows Firewall rule for Stream To Speaker. If speakers stay silent, allow it in Windows Security, under Firewall & network protection, Allow an app through firewall.

[Tasks]
Name: "startmenuicon"; Description: "Create a Start Menu shortcut";  GroupDescription: "Shortcuts:"
Name: "desktopicon";   Description: "Create a desktop shortcut";       GroupDescription: "Shortcuts:"; Flags: unchecked
Name: "autostart";     Description: "Start {#MyAppName} when I sign in"; GroupDescription: "Startup:";   Flags: unchecked

[InstallDelete]
; Replaced by Install-Driver.ps1.
Type: files; Name: "{app}\scripts\Pre-Install.ps1"

[Files]
; All artifacts come from installer\staging\, populated by
; installer\build-installer.ps1 (or the CI workflow's staging step)
; before ISCC runs. This decouples the .iss from WDK / Cargo output
; path conventions that vary by toolchain version.

; --- Service binary ---
Source: "{#SourcePath}\staging\{#MyAppExeName}"; DestDir: "{app}"; Flags: ignoreversion

; --- Driver package ---
Source: "{#SourcePath}\staging\StreamToSpeaker.sys"; DestDir: "{app}\driver"; Flags: ignoreversion
Source: "{#SourcePath}\staging\StreamToSpeaker.inf"; DestDir: "{app}\driver"; Flags: ignoreversion
; The .cat file only exists when the driver has been signed; an unsigned
; local build still produces an installer, whose driver Windows won't take.
Source: "{#SourcePath}\staging\StreamToSpeaker.cat"; DestDir: "{app}\driver"; Flags: ignoreversion skipifsourcedoesntexist

; --- Scripts ---
Source: "{#SourcePath}\..\scripts\InstallerFunctions.ps1";    DestDir: "{app}\scripts"; Flags: ignoreversion
Source: "{#SourcePath}\..\scripts\Install-Driver.ps1";        DestDir: "{app}\scripts"; Flags: ignoreversion
Source: "{#SourcePath}\..\scripts\Rename-Endpoint.ps1";       DestDir: "{app}\scripts"; Flags: ignoreversion
Source: "{#SourcePath}\..\scripts\Reset-Install.ps1";         DestDir: "{app}\scripts"; Flags: ignoreversion
Source: "{#SourcePath}\..\scripts\Diagnose.ps1";              DestDir: "{app}\scripts"; Flags: ignoreversion
Source: "{#SourcePath}\..\scripts\Set-FirewallRule.ps1";      DestDir: "{app}\scripts"; Flags: ignoreversion
Source: "{#SourcePath}\..\scripts\Remove-SignInEntries.ps1";  DestDir: "{app}\scripts"; Flags: ignoreversion
Source: "{#SourcePath}\..\scripts\Uninstall-Driver.ps1";      DestDir: "{app}\scripts"; Flags: ignoreversion
Source: "{#SourcePath}\..\scripts\Remove-TestCertificate.ps1"; DestDir: "{app}\scripts"; Flags: ignoreversion

; --- devcon.exe, from the Windows Driver Kit ---
; Creates the root-enumerated Stream To Speaker device and installs the
; driver on it. pnputil /add-driver /install only matches drivers against
; devices that already exist, and a root-enumerated device has to be
; created explicitly (devcon, or the SetupDi API). devcon install adds a
; device every time it runs, so Install-Driver.ps1 calls it only when
; there is none.
Source: "{#SourcePath}\staging\devcon.exe"; DestDir: "{app}\driver"; Flags: ignoreversion

; --- Test-signing certificate (test builds only) ---
; The certificate that signed a test build's driver. Install-Driver.ps1
; makes Windows trust it (LocalMachine Root and TrustedPublisher) before
; staging the driver, and records it so the uninstaller removes it.
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

[Run]
; The driver, the firewall rule and start-at-sign-in are set up in [Code]
; (CurStepChanged, ssPostInstall), which reads back whether each step
; worked; [Run] entries can't.
Filename: "{app}\{#MyAppExeName}"; Description: "Start {#MyAppName}"; \
    Flags: nowait postinstall skipifsilent

[UninstallRun]
; The RunOnceIds stay the same across versions: an upgrade appends to the
; uninstall log, and of entries with the same RunOnceId only the latest
; runs. Each entry's output goes to the uninstall log.
Filename: "{sys}\taskkill.exe"; Parameters: "/F /IM {#MyAppExeName}"; \
    Flags: runhidden logoutput; RunOnceId: "KillService"
Filename: "{app}\driver\devcon.exe"; Parameters: "remove Root\StreamToSpeaker"; \
    Flags: runhidden waituntilterminated logoutput; RunOnceId: "RemoveDevice"
Filename: "{sys}\WindowsPowerShell\v1.0\powershell.exe"; \
    Parameters: "-NoProfile -NonInteractive -ExecutionPolicy Bypass -File ""{app}\scripts\Uninstall-Driver.ps1"""; \
    Flags: runhidden waituntilterminated logoutput; RunOnceId: "RemoveDriver"
Filename: "{sys}\WindowsPowerShell\v1.0\powershell.exe"; \
    Parameters: "-NoProfile -NonInteractive -ExecutionPolicy Bypass -File ""{app}\scripts\Remove-TestCertificate.ps1"" -CerPath ""{app}\driver\StreamToSpeaker.cer"""; \
    Flags: runhidden waituntilterminated logoutput; RunOnceId: "RemoveTestCert"
Filename: "{sys}\WindowsPowerShell\v1.0\powershell.exe"; \
    Parameters: "-NoProfile -NonInteractive -ExecutionPolicy Bypass -File ""{app}\scripts\Set-FirewallRule.ps1"" -Remove"; \
    Flags: runhidden waituntilterminated logoutput; RunOnceId: "RemoveFirewallRules"
Filename: "{sys}\WindowsPowerShell\v1.0\powershell.exe"; \
    Parameters: "-NoProfile -NonInteractive -ExecutionPolicy Bypass -File ""{app}\scripts\Remove-SignInEntries.ps1"""; \
    Flags: runhidden waituntilterminated logoutput; RunOnceId: "RemoveSignInEntries"

[UninstallDelete]
; The scripts keep their transcript in {app}\logs\install.log. They create
; it, not [Files], so uninstall only knows to remove it from here (a link
; put in the folder's place is removed, not followed).
Type: filesandordirs; Name: "{app}\logs"

[Code]

const
  MutexName = 'Global\StreamToSpeaker.Singleton';
  SYNCHRONIZE = $00100000;
  ERROR_ACCESS_DENIED = 5;
  RunKey = 'HKCU\Software\Microsoft\Windows\CurrentVersion\Run';
  StartupApprovedKey = 'HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run';

var
  // A copy of Stream To Speaker was running when Setup started installing.
  AppWasRunning: Boolean;
  // Stream To Speaker was installed before this run.
  Upgrading: Boolean;
  // The files are in place (Setup can no longer roll back).
  Installed: Boolean;
  TasksPageSeen: Boolean;
  // What Install-Driver.ps1 and Set-FirewallRule.ps1 printed last.
  DriverResultLine: String;
  FirewallResultLine: String;
  DriverOutcome: String;
  DriverRestart: Boolean;
  DriverNewOutput: Boolean;
  DriverKeptBefore: Boolean;
  DriverProblem: String;
  FirewallOk: Boolean;
  // Uninstall: the driver was running for a device when uninstall started.
  DriverWasLoaded: Boolean;

function OpenMutexW(dwDesiredAccess: Cardinal; bInheritHandle: Longint; lpName: String): Longint;
  external 'OpenMutexW@kernel32.dll stdcall';
function CloseHandle(hObject: Longint): Longint;
  external 'CloseHandle@kernel32.dll stdcall';

function IsTestBuild(): Boolean;
begin
  Result := {#TestSignedBuild} <> 0;
end;

// A command-line switch such as /ALLOWSERVER, with or without "=value".
function HasSwitch(const Name: String): Boolean;
var
  I: Integer;
  P: String;
begin
  Result := False;
  for I := 1 to ParamCount do begin
    P := Uppercase(ParamStr(I));
    if (P = '/' + Uppercase(Name)) or (Pos('/' + Uppercase(Name) + '=', P) = 1) then begin
      Result := True;
      Exit;
    end;
  end;
end;

// Setup runs as SYSTEM (a deployment tool): there is no person whose
// sign-in or session Setup could act for.
function RunningAsSystem(): Boolean;
begin
  Result := Pos('\config\systemprofile', Lowercase(GetEnv('USERPROFILE'))) > 0;
end;

function InstallLogPath(): String;
begin
  Result := ExpandConstant('{app}\logs\install.log');
end;

// -1 nothing runs; 0 a copy runs that this account can reach; 1 a copy
// runs in another account (the instance lock exists, but opening it is
// denied, which Inno Setup's own CheckForMutexes reads as "absent").
function RunningCopy(): Integer;
var
  H: Longint;
begin
  Result := -1;
  H := OpenMutexW(SYNCHRONIZE, 0, MutexName);
  if H <> 0 then begin
    CloseHandle(H);
    Result := 0;
  end else if DLLGetLastError = ERROR_ACCESS_DENIED then
    Result := 1;
end;

// The value after "Key=" in the line Install-Driver.ps1 prints last.
function ResultField(const Line, Key: String): String;
var
  S: String;
  P: Integer;
begin
  Result := '';
  P := Pos(' ' + Key + '=', Line);
  if P = 0 then
    Exit;
  S := Copy(Line, P + Length(Key) + 2, Length(Line));
  P := Pos(' ', S);
  if P > 0 then
    S := Copy(S, 1, P - 1);
  Result := S;
end;

procedure ScriptOutput(const S: String; const Error, FirstLine: Boolean);
begin
  if Error then begin
    Log('Reading the script''s output failed: ' + S);
    Exit;
  end;
  Log(S);
  if Pos('driver-step-result: ', S) = 1 then
    DriverResultLine := S
  else if Pos('firewall-rule-result: ', S) = 1 then
    FirewallResultLine := S;
end;

// Runs one of the installed scripts in 64-bit Windows PowerShell (the one
// whose registry view and System32 are the real ones) and logs what it
// prints. Returns False when it couldn't be started.
function RunScript(const Script, Params: String; var ResultCode: Integer): Boolean;
var
  PowerShell, Args: String;
begin
  Args := '-NoProfile -NonInteractive -ExecutionPolicy Bypass -File "' +
          ExpandConstant('{app}\scripts\') + Script + '"' + Params;
  try
#if Ver >= EncodeVer(7, 0, 0)
    PowerShell := ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe');
    Result := ExecAndLogOutputWithNativeSysDir(PowerShell, Args, ExpandConstant('{app}'),
      SW_HIDE, ewWaitUntilTerminated, ResultCode, @ScriptOutput);
#else
    PowerShell := ExpandConstant('{sysnative}\WindowsPowerShell\v1.0\powershell.exe');
    Result := ExecAndLogOutput(PowerShell, Args, ExpandConstant('{app}'),
      SW_HIDE, ewWaitUntilTerminated, ResultCode, @ScriptOutput);
#endif
  except
    Log('Running ' + Script + ' failed: ' + GetExceptionMessage);
    Result := False;
  end;
  if Result then
    Log(Script + ' exit code ' + IntToStr(ResultCode))
  else
    Log(Script + ' could not be started');
end;

// reg.exe as the account that ran Setup (the person, not the
// administrator who may have approved the elevation). Returns its exit
// code, -1 when it couldn't be started.
function RegAsOriginalUser(const Params: String): Integer;
var
  Code: Integer;
begin
  Result := -1;
  try
    if ExecAsOriginalUser(ExpandConstant('{sys}\reg.exe'), Params + ' /reg:64', '', SW_HIDE,
                          ewWaitUntilTerminated, Code) then
      Result := Code;
  except
    Log('reg.exe as the original user failed: ' + GetExceptionMessage);
  end;
end;

function SignInEntryExists(): Boolean;
begin
  Result := RegAsOriginalUser('query "' + RunKey + '" /v "{#MyAppName}"') = 0;
end;

function InitializeSetup(): Boolean;
begin
  Upgrading := RegKeyExists(HKLM64, ExpandConstant(
    'Software\Microsoft\Windows\CurrentVersion\Uninstall\{#emit SetupSetting("AppId")}_is1'));
  Result := True;
end;

function DriverWorks(): Boolean;
begin
  Result := (DriverOutcome = 'ok');
end;

function FinishedText(): String;
var
  LogPath: String;
begin
  LogPath := InstallLogPath();
  if DriverOutcome = 'ok' then begin
    if not Upgrading then
      Result := CustomMessage('DoneNew')
    else if DriverNewOutput then
      Result := CustomMessage('DoneUpdatedNewOutput')
    else
      Result := CustomMessage('DoneUpdated');
  end else if DriverOutcome = 'stage-failed' then begin
    if DriverKeptBefore then
      Result := FmtMessage(CustomMessage('StageFailedKept'), [LogPath])
    else
      Result := FmtMessage(CustomMessage('StageFailed'), [LogPath]);
  end else if DriverOutcome = 'trust-failed' then
    Result := FmtMessage(CustomMessage('TrustFailed'), [LogPath])
  else if DriverOutcome = 'device-failed' then
    Result := FmtMessage(CustomMessage('DeviceFailed'), [LogPath])
  else if DriverOutcome = 'driver-not-started' then begin
    if (DriverProblem = '52') and IsTestBuild() then
      Result := CustomMessage('NotStartedTestBuild')
    else if DriverProblem = '52' then
      Result := FmtMessage(CustomMessage('NotStartedSignature'), [LogPath])
    else if DriverProblem = '48' then
      Result := FmtMessage(CustomMessage('NotStartedPolicy'), [LogPath])
    else if DriverProblem = '22' then
      Result := CustomMessage('NotStartedTurnedOff')
    else
      Result := FmtMessage(CustomMessage('NotStarted'), [LogPath, DriverProblem]);
  end else if DriverOutcome = 'endpoint-missing' then
    Result := FmtMessage(CustomMessage('EndpointMissing'), [LogPath])
  else if DriverOutcome = 'endpoint-off' then
    Result := CustomMessage('EndpointOff')
  else
    Result := FmtMessage(CustomMessage('ScriptFailed'), [LogPath]);
  if not FirewallOk then
    Result := Result + #13#10#13#10 + CustomMessage('FirewallFailed');
end;

procedure UpdateFinishedPage();
var
  Grow: Integer;
begin
  // A restart that is due has its own text and buttons on this page.
  if DriverRestart then
    Exit;
  WizardForm.FinishedLabel.ShowAccelChar := False;
  WizardForm.FinishedLabel.Caption := FinishedText() + #13#10#13#10 + SetupMessage(msgClickFinish);
  Grow := WizardForm.AdjustLabelHeight(WizardForm.FinishedLabel);
  WizardForm.RunList.Top := WizardForm.RunList.Top + Grow;
end;

// On an update the "start at sign-in" box shows what is set now for the
// account that ran Setup (the person may have changed it in the app since),
// and Setup then does what the box says.
procedure CurPageChanged(CurPageID: Integer);
begin
  if (CurPageID = wpSelectTasks) and not TasksPageSeen then begin
    TasksPageSeen := True;
    if Upgrading and not WizardSilent and not RunningAsSystem() and
       not (HasSwitch('TASKS') or HasSwitch('MERGETASKS')) then begin
      if SignInEntryExists() then
        WizardSelectTasks('autostart')
      else
        WizardSelectTasks('!autostart');
    end;
  end else if CurPageID = wpFinished then
    UpdateFinishedPage();
end;

// The question comes once the person has clicked Install, never before
// they have chosen to install; Cancel keeps them on this page with
// Stream To Speaker running. Silent installs don't ask.
function NextButtonClick(CurPageID: Integer): Boolean;
var
  Running: Integer;
  Body: String;
begin
  Result := True;
  if (CurPageID <> wpReady) or WizardSilent then
    Exit;
  Running := RunningCopy();
  if Running < 0 then
    Exit;
  if Running = 1 then
    Body := CustomMessage('CloseTextOtherAccount')
  else
    Body := CustomMessage('CloseText');
  Result := SuppressibleTaskDialogMsgBox(CustomMessage('CloseInstruction'), Body, mbConfirmation,
    MB_OKCANCEL, [CustomMessage('CloseButton')], 0, IDOK) = IDOK;
end;

// Closes every running copy, in any session. An elevated taskkill can end
// another account's copy, which Restart Manager can't.
function CloseRunningCopies(): String;
var
  Running, Code, Attempts: Integer;
begin
  Result := '';
  Running := RunningCopy();
  if Running < 0 then
    Exit;
  AppWasRunning := True;
  Log('Closing the running Stream To Speaker (in another account: ' + IntToStr(Running) + ')');
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM {#MyAppExeName}', '', SW_HIDE,
       ewWaitUntilTerminated, Code);
  Log('taskkill exit code ' + IntToStr(Code));
  // The lock goes when the process has ended; wait up to 10 s.
  Attempts := 0;
  while (RunningCopy() >= 0) and (Attempts < 20) do begin
    Sleep(500);
    Attempts := Attempts + 1;
  end;
  if RunningCopy() >= 0 then begin
    if Running = 1 then
      Result := CustomMessage('CloseFailedOtherAccount')
    else
      Result := CustomMessage('CloseFailed');
  end;
end;

function InsideFolder(const Path, Folder: String): Boolean;
begin
  Result := (Folder <> '') and (Pos(Lowercase(AddBackslash(Folder)), Lowercase(AddBackslash(Path))) = 1);
end;

// The uninstaller runs devcon.exe and the scripts in {app} as
// administrator, so nobody else may be able to change them. Program Files
// is like that already; any other folder gets the same rights: owned by
// Administrators, full control for Administrators and SYSTEM, read and run
// for everyone else. The ACL is set by SID, so this works on Windows in
// any language. An existing folder is fixed on every run.
function SecureInstallFolder(): String;
var
  App, IcAcls: String;
  Code: Integer;
begin
  Result := '';
  App := RemoveBackslashUnlessRoot(ExpandConstant('{app}'));
  if InsideFolder(App, ExpandConstant('{commonpf64}')) or InsideFolder(App, ExpandConstant('{commonpf32}')) then
    Exit;
  Log('Install folder outside Program Files; limiting changes to administrators: ' + App);
  if not ForceDirectories(App) then begin
    Result := FmtMessage(CustomMessage('FolderNotSecured'), [App]);
    Exit;
  end;
  IcAcls := ExpandConstant('{sys}\icacls.exe');
  Exec(IcAcls, '"' + App + '" /setowner *S-1-5-32-544 /T /C /L /Q', '', SW_HIDE, ewWaitUntilTerminated, Code);
  Log('icacls /setowner exit code ' + IntToStr(Code));
  Exec(IcAcls, '"' + App + '" /reset /T /C /L /Q', '', SW_HIDE, ewWaitUntilTerminated, Code);
  Log('icacls /reset exit code ' + IntToStr(Code));
  if not Exec(IcAcls, '"' + App + '" /inheritance:r /grant:r *S-1-5-32-544:(OI)(CI)F *S-1-5-18:(OI)(CI)F *S-1-5-32-545:(OI)(CI)RX /L /Q',
              '', SW_HIDE, ewWaitUntilTerminated, Code) or (Code <> 0) then begin
    Log('icacls /grant exit code ' + IntToStr(Code));
    Result := FmtMessage(CustomMessage('FolderNotSecured'), [App]);
  end;
end;

// Runs once the person has clicked Install (or right away when silent),
// before anything on the PC changes. A non-empty result stops Setup with
// that message and exit code 7.
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Version: TWindowsVersion;
begin
  GetWindowsVersionEx(Version);
  if (Version.ProductType <> VER_NT_WORKSTATION) and not HasSwitch('ALLOWSERVER') then begin
    Result := CustomMessage('ServerRefused');
    Exit;
  end;
  // The folder first: closing the app is the last thing before the files
  // go in, so nothing can stop Setup between the two.
  Result := SecureInstallFolder();
  if Result <> '' then
    Exit;
  Result := CloseRunningCopies();
end;

// Copies the test certificate the build being replaced shipped, so that
// Install-Driver.ps1 can remove it once nothing needs it. The file itself
// is deleted only after the driver step, when Setup can no longer roll
// back to the earlier install that uses it.
procedure SavePreviousCertificate();
var
  OldCer, SavedCer: String;
begin
  OldCer := ExpandConstant('{app}\driver\StreamToSpeaker.cer');
  SavedCer := ExpandConstant('{tmp}\previous-test-cert.cer');
  if FileExists(OldCer) then
    FileCopy(OldCer, SavedCer, False);
end;

procedure InstallDriver();
var
  Code: Integer;
  Params, SavedCer: String;
begin
  WizardForm.StatusLabel.Caption := CustomMessage('StatusDriver');
  WizardForm.FilenameLabel.Caption := '';
  Params := '';
  SavedCer := ExpandConstant('{tmp}\previous-test-cert.cer');
  if FileExists(SavedCer) then
    Params := ' -PreviousCertificate "' + SavedCer + '"';
  DriverResultLine := '';
  RunScript('Install-Driver.ps1', Params, Code);
  if DriverResultLine = '' then
    DriverOutcome := 'script-failed'
  else begin
    DriverOutcome := Copy(DriverResultLine, Length('driver-step-result: ') + 1, Length(DriverResultLine));
    if Pos(' ', DriverOutcome) > 0 then
      DriverOutcome := Copy(DriverOutcome, 1, Pos(' ', DriverOutcome) - 1);
  end;
  DriverRestart := ResultField(DriverResultLine, 'restart') = '1';
  DriverNewOutput := ResultField(DriverResultLine, 'new-output') = '1';
  DriverKeptBefore := ResultField(DriverResultLine, 'kept-before') = '1';
  DriverProblem := ResultField(DriverResultLine, 'problem');
  Log('Driver step: ' + DriverOutcome + ', restart ' + IntToStr(Ord(DriverRestart)) +
      ', new output ' + IntToStr(Ord(DriverNewOutput)) + ', problem ' + DriverProblem);
  if (not IsTestBuild()) and FileExists(ExpandConstant('{app}\driver\StreamToSpeaker.cer')) then
    DeleteFile(ExpandConstant('{app}\driver\StreamToSpeaker.cer'));
end;

procedure AddFirewallRule();
var
  Code: Integer;
begin
  WizardForm.StatusLabel.Caption := CustomMessage('StatusFirewall');
  FirewallResultLine := '';
  RunScript('Set-FirewallRule.ps1', '', Code);
  FirewallOk := FirewallResultLine = 'firewall-rule-result: ok';
end;

// "Start at sign-in" belongs to the account that ran Setup, so it is
// written as that account (an elevated Setup's own HKCU can be the
// administrator's). An interactive Setup does what the box on the tasks
// page says; a silent one only when the tasks are given on its command
// line (/TASKS or /MERGETASKS), and leaves the setting as it is otherwise.
procedure ApplySignIn();
var
  Wanted: Boolean;
  Exe: String;
begin
  if RunningAsSystem() then
    Exit;
  if WizardSilent and not (HasSwitch('TASKS') or HasSwitch('MERGETASKS')) then
    Exit;
  Wanted := WizardIsTaskSelected('autostart');
  Exe := ExpandConstant('{app}\{#MyAppExeName}');
  if Wanted then begin
    Log('Start at sign-in: on, reg.exe exit code ' + IntToStr(RegAsOriginalUser(
      'add "' + RunKey + '" /v "{#MyAppName}" /t REG_SZ /d "\"' + Exe + '\" --startup" /f')));
    // Turned off in Task Manager or Settings earlier: the person has now
    // asked for it again.
    RegAsOriginalUser('delete "' + StartupApprovedKey + '" /v "{#MyAppName}" /f');
  end else if SignInEntryExists() then
    Log('Start at sign-in: off, reg.exe exit code ' + IntToStr(RegAsOriginalUser(
      'delete "' + RunKey + '" /v "{#MyAppName}" /f')));
end;

procedure StartAppAgain();
var
  Code: Integer;
begin
  if not AppWasRunning or RunningAsSystem() then
    Exit;
  try
    if ExecAsOriginalUser(ExpandConstant('{app}\{#MyAppExeName}'), '--startup', ExpandConstant('{app}'),
                          SW_SHOWNORMAL, ewNoWait, Code) then
      Log('Started Stream To Speaker again.')
    else
      Log('Starting Stream To Speaker again failed: ' + SysErrorMessage(Code));
  except
    Log('Starting Stream To Speaker again failed: ' + GetExceptionMessage);
  end;
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssInstall then
    SavePreviousCertificate()
  else if CurStep = ssPostInstall then begin
    Installed := True;
    InstallDriver();
    AddFirewallRule();
    ApplySignIn();
  end else if (CurStep = ssDone) and WizardSilent then
    // A silent update closed the app: start it again, in the tray, for the
    // account that ran Setup. (An interactive Setup offers it on its last
    // page.)
    StartAppAgain();
end;

// Setup closed the app and then stopped before the new files were in
// place (cancelled, or failed and rolled back): the earlier version is
// still installed, so start it again rather than leave the PC without it.
procedure DeinitializeSetup();
begin
  if not Installed then
    StartAppAgain();
end;

function NeedRestart(): Boolean;
begin
  Result := DriverRestart;
end;

// Setup's exit code for a silent install: 20 when the app is installed but
// its sound device isn't working.
function GetCustomSetupExitCode(): Integer;
begin
  Result := 0;
  if not DriverWorks() and not DriverRestart then
    Result := 20;
end;

function InitializeUninstall(): Boolean;
var
  Count: Cardinal;
begin
  // While the driver runs for a device, Windows keeps it loaded after the
  // device is gone, until a restart.
  DriverWasLoaded := RegQueryDWordValue(HKLM64, 'SYSTEM\CurrentControlSet\Services\StreamToSpeaker\Enum',
                                        'Count', Count) and (Count > 0);
  Result := True;
end;

// Only an interactive uninstall asks for the restart: a very silent one
// without /NORESTART would restart the PC without asking, and the loaded
// driver does no harm until the next restart.
function UninstallNeedRestart(): Boolean;
begin
  Result := DriverWasLoaded and not UninstallSilent;
end;
