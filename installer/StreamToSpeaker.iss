; -----------------------------------------------------------------------------
; Stream To Speaker: Inno Setup script.
;
; Stream To Speaker installs in Program Files and nowhere else: it installs a
; sound driver and the uninstaller runs its files as administrator, so they
; must be where only administrators can change them. There is no folder
; page; a /DIR= that names another folder is refused (exit code 1).
;
; What Setup does, in order:
;   1. Refuses, before it changes anything (exit code 7): Windows Server,
;      where the driver doesn't run (/ALLOWSERVER lets the installer
;      lifecycle test run on its Windows Server machines); a test build on a
;      PC whose Windows isn't in test-signing mode, where its driver can't
;      start (/ALLOWTESTBUILD installs it all the same: for a developer's PC
;      that loads test-signed drivers in another way, a kernel debugger or
;      signature enforcement turned off at boot, which Setup can't see).
;   2. If Stream To Speaker runs from its install folder, in this account or
;      another one, asks to close it (only once the person has clicked
;      Install; a silent install doesn't ask) and closes it. A copy started
;      from elsewhere is left alone.
;   3. Copies the app, the driver package (.sys, .inf, .cat, devcon.exe and,
;      in test builds only, the test certificate) and the scripts.
;   4. Runs scripts\Install-Driver.ps1, which installs the driver and its
;      sound output without disturbing a device that already has this
;      driver (see that script), and reads back what happened.
;   5. Adds a Windows Firewall rule for the app (scripts\Set-FirewallRule.ps1).
;   6. Turns "start at sign-in" on or off for the account that ran Setup.
;   7. When an earlier install was in another folder, removes from it what
;      releases put there (scripts\Remove-EarlierInstall.ps1); nothing in
;      that folder is run.
;   8. Says on its last page what happened, starts the app again for the
;      person whose copy it closed, and asks for a restart when the new driver
;      needs one. Setup starts the app only as that person, without
;      administrator rights, which it can when it asked for those rights
;      itself; started with them ("Run as administrator", an elevated
;      prompt), it starts nothing and says to use the Start menu.
; Uninstall closes the app, removes the device, the driver packages,
; Windows' cached sound output entries, the test certificates, the firewall
; rules and the start-at-sign-in entries of signed-in accounts, and says
; what it couldn't remove. Settings and speakers (each account's
; AppData\Roaming\StreamToSpeaker) stay.
;
; Build with installer\build-installer.ps1, or: ISCC.exe installer\StreamToSpeaker.iss
; after staging the files in installer\staging\.
; Output:     installer\out\StreamToSpeakerSetup-<ver>.exe
;
; Exit codes besides Inno Setup's own: 20 = installed, but Setup couldn't
; finish setting up the sound driver (the reason is in Setup's log and in
; <install dir>\logs\install.log); 3010 = a silent install that needs a
; restart (a silent install never restarts the PC).
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
; The app's identity in Windows (Start menu, notifications). The app must use
; the same string when it raises Windows notifications.
#define MyAppUserModelID "StreamToSpeaker.App"
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
; Always Program Files (see the top of this file): no folder page, no
; earlier folder reused (an earlier install elsewhere is moved, see [Code]),
; no network folders.
DefaultDirName={autopf}\{#MyAppName}
DisableDirPage=yes
UsePreviousAppDir=no
AllowUNCPath=no
AllowNetworkDrive=no
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
; output of each step.
UninstallLogging=yes
; pnputil and devcon can set Windows' "restart needed" flag after staging
; or installing a driver, which Inno Setup would turn into a restart
; prompt. Whether a restart is needed is decided from what
; Install-Driver.ps1 reports (NeedRestart in [Code]).
RestartIfNeededByRun=no
AlwaysRestart=no
; Setup closes a running Stream To Speaker itself, in PrepareToInstall
; (see CloseCopies). Restart Manager stays on as the fallback for any other
; program from {app} that holds a file Setup replaces.
CloseApplications=force
CloseApplicationsFilter=*.exe
RestartApplications=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Messages]
WinVersionTooLowError=Stream To Speaker needs Windows 10 version 1809 or later, or Windows 11.
OnlyOnTheseArchitectures=Stream To Speaker can't be installed on this PC. Its sound driver needs 64-bit Windows on an Intel or AMD processor; it doesn't run on ARM processors or on 32-bit Windows.
FinishedRestartMessage=Windows must restart to finish installing Stream To Speaker's sound driver.%n%nRestart now?
ConfirmUninstall=Remove %1 from this PC?%n%nIt is closed in every account on this PC. Each account's settings and speakers are kept.
UninstalledAll=%1's uninstall is finished.%n%nEach account's settings and speakers stay in its AppData\Roaming\StreamToSpeaker folder, and its log in AppData\Local\StreamToSpeaker, so installing it again brings them back. To remove them too, delete those folders.
UninstalledMost=%1's uninstall is finished, but some of its files couldn't be removed: delete the folder Stream To Speaker in Program Files by hand.%n%nEach account's settings and speakers stay in its AppData\Roaming\StreamToSpeaker folder, and its log in AppData\Local\StreamToSpeaker. To remove them too, delete those folders.
UninstalledAndNeedsRestart=%1's uninstall is finished. Its sound driver stays loaded until Windows restarts.%n%nEach account's settings and speakers stay in its AppData\Roaming\StreamToSpeaker folder, and its log in AppData\Local\StreamToSpeaker. To remove them too, delete those folders.%n%nRestart now?

[CustomMessages]
DirRefused=Stream To Speaker installs only in %1: it installs a sound driver and runs parts of itself as administrator, so its files must be in a folder only administrators can change. Run Setup again without /DIR. Nothing was changed.
ServerRefused=Stream To Speaker can't be installed on Windows Server: its sound driver runs only on Windows 10 and Windows 11. Nothing was changed.
TestBuildRefused=This is a test build of Stream To Speaker. Its sound driver runs only on PCs set up for testing drivers (Windows' test-signing mode), and this PC isn't, so the driver couldn't start here.%n%nInstall the release instead, from github.com/Mihonarium/StreamToSpeaker/releases. Nothing was changed.
CloseInstruction=Close Stream To Speaker to update it?
CloseMine=Stream To Speaker is running. Setup closes it to update it and starts it again when it's done; until then, the speakers it plays on are silent.
CloseMineNoStart=Stream To Speaker is running. Setup closes it to update it; start it again from the Start menu when Setup is done. Until then, the speakers it plays on are silent.
CloseOther=Stream To Speaker is running in another account on this PC. Setup closes it there to update it and doesn't start it again in that account; the speakers it plays on there are silent until someone starts it in that account.
CloseBoth=Stream To Speaker is running in your account and in another account on this PC. Setup closes both to update it. It starts yours again when it's done; in the other account it starts again only when someone starts it there.
CloseBothNoStart=Stream To Speaker is running in your account and in another account on this PC. Setup closes both to update it; start yours again from the Start menu when Setup is done. In the other account it starts again only when someone starts it there.
CloseUnknown=Stream To Speaker is running on this PC. Setup closes it to update it; the speakers it plays on are silent until it is started again.
CloseButton=Close Stream To Speaker and install
CloseFailedMine=Setup couldn't close Stream To Speaker, so it installed nothing.%n%nQuit Stream To Speaker (right-click its icon in the notification area, then choose Quit), then run Setup again.
CloseFailedOther=Setup couldn't close Stream To Speaker, which runs in another account on this PC, so it installed nothing.%n%nAsk the person who uses that account to quit it, or restart the PC, then run Setup again.
CloseFailedUnknown=Setup couldn't close Stream To Speaker, so it installed nothing.%n%nQuit Stream To Speaker wherever it runs on this PC (right-click its icon in the notification area, then choose Quit), or restart the PC, then run Setup again.
ClosedYoursStarts=Setup did close the copy that ran in your account, and starts it again when you close Setup.
ClosedYoursStartIt=Setup did close the copy that ran in your account: start it again from the Start menu.
StatusDriver=Installing the Stream To Speaker sound driver (sound on this PC may stop for a moment)...
StatusFirewall=Adding the Windows Firewall rule for Stream To Speaker...
StatusEarlier=Removing the earlier copy of Stream To Speaker...
DoneNew=Stream To Speaker is installed.%n%nNext: make Stream To Speaker the sound output in Windows (select the speaker icon on the taskbar and choose it), then pick a speaker in the app.
NextAfterRestart=After the restart: start Stream To Speaker from the Start menu, make it the sound output in Windows (select the speaker icon on the taskbar and choose it), then pick a speaker in the app.
DoneUpdated=Stream To Speaker is installed.
DoneStillDefault=Stream To Speaker is installed, and Windows still plays to it.
DoneLostDefault=Stream To Speaker is installed, but Windows now plays to another output. To play to Stream To Speaker again, select the speaker icon on the taskbar and choose it.
ExtrasRemoved=Windows showed more than one Stream To Speaker output; Setup removed the extra ones.
ExtrasLeft=Windows still shows more than one Stream To Speaker output, and only one of them plays: Setup couldn't remove the others. To fix it, uninstall Stream To Speaker, restart the PC and install it again.
SoundMoved=Setup closed Stream To Speaker and doesn't start it again, so Windows now plays to the sound output it used before.
EarlierNotEmpty=Stream To Speaker is now in %1. The folder it was in before, %2, still holds other files, so Setup left the folder in place.
EarlierLeftAlone=Stream To Speaker is now in %1. Setup left the folder it was in before, %2, as it was: it couldn't make sure that deleting there would remove only Stream To Speaker's earlier files. Delete that folder yourself when you no longer need it.
StageFailedKept=Stream To Speaker is installed, but Windows didn't accept its new sound driver, so this PC keeps the driver it had.%n%nWhy Windows refused it is in %2. To report the problem, don't post that file (it names your Windows account and this PC): run this in PowerShell and post what it prints.%n%n%1
StageFailed=Stream To Speaker is installed, but Windows didn't accept its sound driver, so it can't play this PC's sound.%n%nWhy Windows refused it is in %2. To report the problem, don't post that file (it names your Windows account and this PC): run this in PowerShell and post what it prints.%n%n%1
TrustFailed=Stream To Speaker is installed, but Windows didn't accept this test build's certificate, so its sound driver isn't installed.%n%nDetails are in %1.
DeviceFailedKept=Stream To Speaker is installed, but Windows couldn't update its sound device; the device still has the driver it had before.%n%nDetails are in %1.
DeviceFailed=Stream To Speaker is installed, but Windows couldn't set up its sound device.%n%nDetails are in %1.
NotStartedTestBuild=Stream To Speaker is installed, but Windows didn't start its sound driver: this is a test build, whose driver starts only in Windows' test-signing mode. Windows now trusts the build's test certificate; uninstalling Stream To Speaker removes it.%n%nInstall the release instead, from github.com/Mihonarium/StreamToSpeaker/releases.
NotStartedSignature=Stream To Speaker is installed, but Windows didn't accept its sound driver's signature, so the driver didn't start.%n%nUpdate Windows (Settings, Windows Update), then restart the PC: Windows tries the driver again when it starts. Details are in %1.
NotStartedPolicy=Stream To Speaker is installed, but a policy on this PC blocks its sound driver. Ask whoever manages this PC to allow it.%n%nDetails are in %1.
NotStartedTurnedOff=Stream To Speaker is installed, but its sound device is turned off in Device Manager. Turn on Stream To Speaker there, under Sound, video and game controllers.
NotStarted=Stream To Speaker is installed, but Windows didn't start its sound driver (Device Manager shows problem code %2).%n%nRestart the PC; if the problem stays, details are in %1.
EndpointMissing=Stream To Speaker is installed and its driver runs, but Windows doesn't show its sound output.%n%nRestart the PC. If it's still missing, run this in an elevated PowerShell, restart again and run Setup:%n%2
EndpointOff=Stream To Speaker is installed, but its sound output is turned off in Windows.%n%nOpen Sound settings, choose Stream To Speaker and allow it, then pick a speaker in the app.
ScriptFailed=Stream To Speaker's files are installed, but its driver step couldn't run to the end (a policy on this PC may block PowerShell scripts), so Setup can't tell whether the sound output works.%n%nDetails are in %1.
TestSigningOff=Stream To Speaker is installed, but its sound driver wasn't: this is a test build, whose driver runs only in Windows' test-signing mode.%n%nInstall the release instead, from github.com/Mihonarium/StreamToSpeaker/releases.
FirewallFailed=Setup couldn't add the Windows Firewall rule for Stream To Speaker. If speakers stay silent, allow it in Windows Security, under Firewall & network protection, Allow an app through firewall.
RestartReplacing=Windows must restart to start Stream To Speaker's new sound driver; until then, the driver it replaces keeps running.
RestartStarting=Windows must restart before Stream To Speaker's sound driver starts.
RestartAppAgain=Stream To Speaker starts again after the restart, or when Setup closes if you restart later.
RestartStartYourself=Start Stream To Speaker again from the Start menu, now or after the restart.
StartYourself=Start Stream To Speaker from the Start menu. Setup was started as administrator, so it doesn't start the app itself: the app would run as administrator too.
StartYourselfUnknown=Start Stream To Speaker from the Start menu. Setup couldn't tell which Windows account started it, so it doesn't start the app itself.
SignInNotSet=Setup didn't set Stream To Speaker to start when you sign in, because it runs in another Windows account than yours. Turn that on in the app instead.
SignInNotSetUnknown=Setup didn't set Stream To Speaker to start when you sign in, because it couldn't tell which Windows account started it. Turn that on in the app instead.
RestartNow=Restart now?
UninstallLeft=The uninstall couldn't remove everything Stream To Speaker put on this PC. Still there:%n%n%1
UninstallRetry=To remove the rest, restart the PC, install Stream To Speaker again and uninstall it.

[Tasks]
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

; --- Scripts Setup also runs before installing ---
; Setup extracts these two to its own temporary folder to find a running
; copy before anything is installed (ExtractTemporaryFile); with solid
; compression, the files listed first are the quickest to extract.
Source: "{#SourcePath}\..\scripts\InstallerFunctions.ps1";    DestDir: "{app}\scripts"; Flags: ignoreversion
Source: "{#SourcePath}\..\scripts\Find-RunningCopies.ps1";    DestDir: "{app}\scripts"; Flags: ignoreversion

; --- Service binary ---
Source: "{#SourcePath}\staging\{#MyAppExeName}"; DestDir: "{app}"; Flags: ignoreversion

; --- Driver package ---
Source: "{#SourcePath}\staging\StreamToSpeaker.sys"; DestDir: "{app}\driver"; Flags: ignoreversion
Source: "{#SourcePath}\staging\StreamToSpeaker.inf"; DestDir: "{app}\driver"; Flags: ignoreversion
; The .cat file only exists when the driver has been signed; an unsigned
; local build still produces an installer, whose driver Windows won't take.
Source: "{#SourcePath}\staging\StreamToSpeaker.cat"; DestDir: "{app}\driver"; Flags: ignoreversion skipifsourcedoesntexist

; --- The other scripts ---
Source: "{#SourcePath}\..\scripts\Install-Driver.ps1";        DestDir: "{app}\scripts"; Flags: ignoreversion
Source: "{#SourcePath}\..\scripts\Rename-Endpoint.ps1";       DestDir: "{app}\scripts"; Flags: ignoreversion
Source: "{#SourcePath}\..\scripts\Reset-Install.ps1";         DestDir: "{app}\scripts"; Flags: ignoreversion
Source: "{#SourcePath}\..\scripts\Diagnose.ps1";              DestDir: "{app}\scripts"; Flags: ignoreversion
Source: "{#SourcePath}\..\scripts\Set-FirewallRule.ps1";      DestDir: "{app}\scripts"; Flags: ignoreversion
Source: "{#SourcePath}\..\scripts\Remove-SignInEntries.ps1";  DestDir: "{app}\scripts"; Flags: ignoreversion
Source: "{#SourcePath}\..\scripts\Remove-EarlierInstall.ps1"; DestDir: "{app}\scripts"; Flags: ignoreversion
Source: "{#SourcePath}\..\scripts\Uninstall-Driver.ps1";      DestDir: "{app}\scripts"; Flags: ignoreversion
Source: "{#SourcePath}\..\scripts\Remove-TestCertificate.ps1"; DestDir: "{app}\scripts"; Flags: ignoreversion

; --- devcon.exe, from the Windows Driver Kit ---
; Creates the root-enumerated Stream To Speaker device and installs the
; driver on it. pnputil /add-driver /install only matches drivers against
; devices that already exist, and a root-enumerated device has to be
; created explicitly (devcon, or the SetupDi API). devcon install adds a
; device every time it runs, so Install-Driver.ps1 calls it only when it
; means to.
Source: "{#SourcePath}\staging\devcon.exe"; DestDir: "{app}\driver"; Flags: ignoreversion

; --- Test-signing certificate (test builds only) ---
; The certificate that signed a test build's driver. Install-Driver.ps1
; records it and then makes Windows trust it (LocalMachine Root and
; TrustedPublisher) before staging the driver; the uninstaller removes it.
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
; The Start-menu entry is always made: people find the app there, and it
; carries the app's AppUserModelID, which Windows needs to show the app's
; notifications under its name.
Name: "{group}\{#MyAppName}";         Filename: "{app}\{#MyAppExeName}"; AppUserModelID: "{#MyAppUserModelID}"
Name: "{commondesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; AppUserModelID: "{#MyAppUserModelID}"; Tasks: desktopicon

[Run]
; The driver, the firewall rule and start-at-sign-in are set up in [Code]
; (CurStepChanged, ssPostInstall), which reads back whether each step
; worked; [Run] entries can't.
Filename: "{app}\{#MyAppExeName}"; Description: "Start {#MyAppName}"; \
    Flags: nowait postinstall skipifsilent

[UninstallRun]
; The real work happens in [Code] (CurUninstallStepChanged, usUninstall),
; which reads back what was left. These entries keep the RunOnceIds of
; earlier versions: an update appends to the uninstall log, and of entries
; with the same RunOnceId only the latest runs, so older ones (such as a
; taskkill of every copy by name) never run. Run after [Code], they find
; nothing left to do; their output goes to the uninstall log.
Filename: "{sys}\WindowsPowerShell\v1.0\powershell.exe"; \
    Parameters: "-NoProfile -NonInteractive -ExecutionPolicy Bypass -File ""{app}\scripts\Find-RunningCopies.ps1"" -Folders ""{app}"" -Close"; \
    Flags: runhidden waituntilterminated logoutput; RunOnceId: "KillService"
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
  RunKey = 'HKCU\Software\Microsoft\Windows\CurrentVersion\Run';
  RunOnceKey = 'HKCU\Software\Microsoft\Windows\CurrentVersion\RunOnce';
  StartupApprovedKey = 'HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run';
  InstallerKey = 'SOFTWARE\StreamToSpeaker\Installer';

var
  // Stream To Speaker was installed before this run, and where.
  Upgrading: Boolean;
  EarlierDir: String;
  Moving: Boolean;
  // The files are in place (Setup can no longer roll back).
  Installed: Boolean;
  // Copies of the app Setup closes: the person's own, and another account's;
  // and how many of each the last try closed.
  CopiesMine, CopiesOther, CopiesMineClosed, CopiesOtherClosed: Integer;
  CopiesClosed: String;
  // PersonKnown: Setup could tell which account ran it. CanStartAsPerson:
  // it can start a program as that person, without administrator rights.
  // CanActAsPerson: what it runs as the original user changes that
  // person's own settings. ClosedMine, ClosedOther: Setup closed a copy of
  // the person's, of another account's (in this try or an earlier one).
  PersonKnown, CanStartAsPerson, CanActAsPerson, ClosedMine, ClosedOther: Boolean;
  // The person's start-at-sign-in entry is there, but turned off in Task
  // Manager or Settings.
  SignInTurnedOff: Boolean;
  ScriptsExtracted: Boolean;
  TasksPageSeen: Boolean;
  // What the scripts printed last.
  DriverResultLine, FirewallResultLine, CopiesLine, EarlierLine: String;
  // What the uninstall couldn't remove: LeftLines, what uninstalling again
  // may remove; StaysLines, what Windows refused to delete.
  LeftLines, StaysLines: String;
  DriverOutcome, DriverProblem: String;
  DriverRestart, DriverReplaced, DriverNewOutput, DriverKeptBefore: Boolean;
  DriverWasDefault, DriverIsDefault, DriverSoundMoved: Boolean;
  DriverExtrasRemoved, DriverExtrasLeft: Integer;
  FirewallOk, SignInSkipped: Boolean;
  EarlierResult: String;
  // Uninstall: the driver was running for a device when uninstall started.
  DriverWasLoaded: Boolean;

function GetCurrentProcessId: Cardinal;
  external 'GetCurrentProcessId@kernel32.dll stdcall';

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

// The value of a command-line parameter /Name=value, without quotes.
function SwitchValue(const Name: String): String;
var
  I: Integer;
  P: String;
begin
  Result := '';
  for I := 1 to ParamCount do begin
    P := ParamStr(I);
    if Pos('/' + Uppercase(Name) + '=', Uppercase(P)) = 1 then begin
      Result := Copy(P, Length(Name) + 3, Length(P));
      if (Length(Result) >= 2) and (Result[1] = '"') and (Result[Length(Result)] = '"') then
        Result := Copy(Result, 2, Length(Result) - 2);
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

// The one folder Stream To Speaker installs in.
function ProgramFilesDir(): String;
begin
  Result := ExpandConstant('{commonpf64}\{#MyAppName}');
end;

function SamePath(const A, B: String): Boolean;
begin
  Result := CompareText(RemoveBackslashUnlessRoot(A), RemoveBackslashUnlessRoot(B)) = 0;
end;

function UninstallKeyName(): String;
begin
  Result := ExpandConstant('Software\Microsoft\Windows\CurrentVersion\Uninstall\{#emit SetupSetting("AppId")}_is1');
end;

// Whether Windows started with test signing on: SystemStartOptions holds
// the options of the current boot, and isn't translated.
function TestSigningOn(): Boolean;
var
  Options: String;
begin
  Result := False;
  if RegQueryStringValue(HKLM64, 'SYSTEM\CurrentControlSet\Control', 'SystemStartOptions', Options) then
    Result := Pos(' TESTSIGNING ', ' ' + Uppercase(Options) + ' ') > 0;
end;

// The value after "Key=" in a result line.
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

// The first word after a line's "name: ".
function ResultWord(const Line, Name: String): String;
begin
  Result := '';
  if Line = '' then
    Exit;
  Result := Copy(Line, Length(Name) + 3, Length(Line));
  if Pos(' ', Result) > 0 then
    Result := Copy(Result, 1, Pos(' ', Result) - 1);
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
    FirewallResultLine := S
  else if Pos('running-copies: ', S) = 1 then
    CopiesLine := S
  else if Pos('earlier-install-result: ', S) = 1 then
    EarlierLine := S
  else if Pos('left: ', S) = 1 then
    LeftLines := LeftLines + '- ' + Copy(S, 7, Length(S)) + #13#10
  else if Pos('stays: ', S) = 1 then
    StaysLines := StaysLines + '- ' + Copy(S, 8, Length(S)) + #13#10;
end;

// Runs a script in 64-bit Windows PowerShell (the one whose registry view
// and System32 are the real ones) and logs what it prints. Returns False
// when it couldn't be started.
function RunPowerShell(const ScriptPath, Params: String; var ResultCode: Integer): Boolean;
begin
  try
    Result := ExecAndLogOutput(ExpandConstant('{sysnative}\WindowsPowerShell\v1.0\powershell.exe'),
      '-NoProfile -NonInteractive -ExecutionPolicy Bypass -File "' + ScriptPath + '"' + Params,
      ExtractFileDir(ScriptPath), SW_HIDE, ewWaitUntilTerminated, ResultCode, @ScriptOutput);
  except
    Log('Running ' + ScriptPath + ' failed: ' + GetExceptionMessage);
    Result := False;
  end;
  if Result then
    Log(ExtractFileName(ScriptPath) + ' exit code ' + IntToStr(ResultCode))
  else
    Log(ExtractFileName(ScriptPath) + ' could not be started');
end;

// One of the installed scripts (in Program Files).
function RunScript(const Script, Params: String; var ResultCode: Integer): Boolean;
begin
  Result := RunPowerShell(ExpandConstant('{app}\scripts\') + Script, Params, ResultCode);
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

// The folders a copy that Setup closes runs from.
function CopyFolders(): String;
begin
  Result := ProgramFilesDir();
  if Moving then
    Result := Result + '|' + EarlierDir;
end;

// Finds the running copies (and with Close, closes them) through
// Find-RunningCopies.ps1, extracted to Setup's own temporary folder (only
// administrators can change files there while Setup runs elevated).
function FindCopies(const Close: Boolean): Boolean;
var
  Code: Integer;
  Params: String;
begin
  if not ScriptsExtracted then begin
    ExtractTemporaryFile('InstallerFunctions.ps1');
    ExtractTemporaryFile('Find-RunningCopies.ps1');
    ScriptsExtracted := True;
  end;
  Params := ' -Folders "' + CopyFolders() + '" -SetupProcessId ' + IntToStr(GetCurrentProcessId());
  if Close then
    Params := Params + ' -Close';
  CopiesLine := '';
  RunPowerShell(ExpandConstant('{tmp}\Find-RunningCopies.ps1'), Params, Code);
  CopiesMine := StrToIntDef(ResultField(CopiesLine, 'mine'), 0);
  CopiesOther := StrToIntDef(ResultField(CopiesLine, 'other'), 0);
  CopiesMineClosed := StrToIntDef(ResultField(CopiesLine, 'closed-mine'), 0);
  CopiesOtherClosed := StrToIntDef(ResultField(CopiesLine, 'closed-other'), 0);
  CopiesClosed := ResultField(CopiesLine, 'closed');
  PersonKnown := ResultField(CopiesLine, 'person') = 'known';
  CanStartAsPerson := ResultField(CopiesLine, 'start') = 'yes';
  CanActAsPerson := ResultField(CopiesLine, 'settings') = 'yes';
  SignInTurnedOff := ResultField(CopiesLine, 'signin') = 'off';
  if (CopiesLine = '') or (CopiesClosed = 'error') then begin
    // The script couldn't run (a policy may block PowerShell): Setup goes
    // on as Inno Setup does by itself, and Windows' Restart Manager closes a
    // copy that holds a file Setup replaces. It starts no app: it can't
    // tell whether the app would run with administrator rights.
    Log('Could not look for running copies of Stream To Speaker; going on without.');
    CopiesMine := 0;
    CopiesOther := 0;
    CopiesMineClosed := 0;
    CopiesOtherClosed := 0;
    CopiesClosed := 'none';
    PersonKnown := False;
    CanStartAsPerson := False;
    CanActAsPerson := True;
    SignInTurnedOff := False;
  end;
  // Also when another copy couldn't be closed, and kept from an earlier
  // try: Install can be clicked again after a close that failed.
  if CopiesMineClosed > 0 then
    ClosedMine := True;
  if CopiesOtherClosed > 0 then
    ClosedOther := True;
  Result := CopiesClosed <> 'failed';
end;

// Records the thumbprint of a test certificate an earlier build left (its
// .cer file), before anything can make the file go away, so that the
// uninstaller can always remove the certificate. A file that isn't one of
// our certificates is recorded too, and ignored at removal (subject check).
procedure RecordCertificate(const CerFile: String);
var
  Thumb, Existing: String;
begin
  if not FileExists(CerFile) then
    Exit;
  try
    Thumb := Uppercase(GetSHA1OfFile(CerFile));
  except
    Log('Could not read ' + CerFile + ': ' + GetExceptionMessage);
    Exit;
  end;
  if not RegQueryMultiStringValue(HKLM64, InstallerKey, 'TestCertificates', Existing) then
    Existing := '';
  if Pos(Thumb, Uppercase(Existing)) > 0 then
    Exit;
  if Existing <> '' then
    Existing := Existing + #0;
  if RegWriteMultiStringValue(HKLM64, InstallerKey, 'TestCertificates', Existing + Thumb) then
    Log('Recorded the earlier test certificate ' + Thumb)
  else
    Log('Could not record the earlier test certificate ' + Thumb);
end;

// Where the earlier build's files are (and its .cer, if a test build).
function EarlierAppDir(): String;
begin
  if Moving then
    Result := EarlierDir
  else
    Result := ExpandConstant('{app}');
end;

function InitializeSetup(): Boolean;
var
  Dir: String;
begin
  Result := True;
  Upgrading := RegKeyExists(HKLM64, UninstallKeyName());
  EarlierDir := '';
  if Upgrading and RegQueryStringValue(HKLM64, UninstallKeyName(), 'Inno Setup: App Path', EarlierDir) then
    EarlierDir := RemoveBackslashUnlessRoot(EarlierDir);
  Moving := (EarlierDir <> '') and not SamePath(EarlierDir, ProgramFilesDir());
  if Moving then
    Log('The earlier install is in ' + EarlierDir + '; Stream To Speaker moves to ' + ProgramFilesDir());
  Dir := SwitchValue('DIR');
  if (Dir <> '') and not SamePath(Dir, ProgramFilesDir()) then begin
    Log('Refusing /DIR=' + Dir + ': Stream To Speaker installs only in ' + ProgramFilesDir());
    SuppressibleMsgBox(FmtMessage(CustomMessage('DirRefused'), [ProgramFilesDir()]), mbCriticalError, MB_OK, IDOK);
    Result := False;
  end;
end;

// Setup starts the app again: it closed the person's own copy, and can
// start a program as that person, without administrator rights (not when
// Setup was started with those rights, by "Run as administrator" or from an
// elevated prompt: the app would run with them; and not as SYSTEM).
function StartsAgain(): Boolean;
begin
  Result := ClosedMine and CanStartAsPerson and not RunningAsSystem();
end;

// The lines that follow the outcome on the last page.
function ExtraLines(): String;
begin
  Result := '';
  if DriverExtrasRemoved > 0 then
    Result := Result + #13#10#13#10 + CustomMessage('ExtrasRemoved');
  if DriverExtrasLeft > 0 then
    Result := Result + #13#10#13#10 + CustomMessage('ExtrasLeft');
  if DriverSoundMoved then
    Result := Result + #13#10#13#10 + CustomMessage('SoundMoved');
  if EarlierResult = 'not-empty' then
    Result := Result + #13#10#13#10 + FmtMessage(CustomMessage('EarlierNotEmpty'), [ExpandConstant('{app}'), EarlierDir])
  else if EarlierResult = 'left-alone' then
    Result := Result + #13#10#13#10 + FmtMessage(CustomMessage('EarlierLeftAlone'), [ExpandConstant('{app}'), EarlierDir]);
  if not FirewallOk then
    Result := Result + #13#10#13#10 + CustomMessage('FirewallFailed');
  if SignInSkipped then begin
    if PersonKnown then
      Result := Result + #13#10#13#10 + CustomMessage('SignInNotSet')
    else
      Result := Result + #13#10#13#10 + CustomMessage('SignInNotSetUnknown');
  end;
end;

function FailureText(): String;
var
  LogPath, Diagnose: String;
begin
  LogPath := InstallLogPath();
  // For a report, what the diagnosis script prints and not the install log
  // itself: the log names the account and the PC, which the script
  // replaces where it finds them.
  Diagnose := 'powershell -NoProfile -ExecutionPolicy Bypass -File "' + ExpandConstant('{app}\scripts\Diagnose.ps1') + '"';
  if DriverOutcome = 'stage-failed' then begin
    if DriverKeptBefore then
      Result := FmtMessage(CustomMessage('StageFailedKept'), [Diagnose, LogPath])
    else
      Result := FmtMessage(CustomMessage('StageFailed'), [Diagnose, LogPath]);
  end else if DriverOutcome = 'trust-failed' then
    Result := FmtMessage(CustomMessage('TrustFailed'), [LogPath])
  else if DriverOutcome = 'test-signing-off' then
    Result := CustomMessage('TestSigningOff')
  else if DriverOutcome = 'device-failed' then begin
    if DriverKeptBefore then
      Result := FmtMessage(CustomMessage('DeviceFailedKept'), [LogPath])
    else
      Result := FmtMessage(CustomMessage('DeviceFailed'), [LogPath]);
  end else if DriverOutcome = 'driver-not-started' then begin
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
    Result := FmtMessage(CustomMessage('EndpointMissing'), [LogPath,
      'powershell -NoProfile -ExecutionPolicy Bypass -File "' + ExpandConstant('{app}\scripts\Reset-Install.ps1') + '"'])
  else if DriverOutcome = 'endpoint-off' then
    Result := CustomMessage('EndpointOff')
  else
    Result := FmtMessage(CustomMessage('ScriptFailed'), [LogPath]);
end;

function OutcomeText(): String;
begin
  if DriverOutcome <> 'ok' then
    Result := FailureText()
  else if not Upgrading then
    Result := CustomMessage('DoneNew')
  else if DriverSoundMoved then
    // The line that says so follows (ExtraLines).
    Result := CustomMessage('DoneUpdated')
  else if DriverWasDefault and DriverIsDefault then
    Result := CustomMessage('DoneStillDefault')
  else if DriverWasDefault then
    Result := CustomMessage('DoneLostDefault')
  else
    Result := CustomMessage('DoneUpdated');
  Result := Result + ExtraLines();
end;

procedure UpdateFinishedPage();
var
  Text: String;
  Grow: Integer;
  OffersStart: Boolean;
begin
  WizardForm.FinishedLabel.ShowAccelChar := False;
  // The box that starts the app is offered only when Setup can start the
  // app as the person, without administrator rights: otherwise the box
  // would start it as the account Setup runs as, with those rights. Inno
  // Setup starts a ticked entry whether or not its list is shown, so the
  // entry is unticked before the list is hidden.
  OffersStart := CanStartAsPerson and not RunningAsSystem();
  if WizardForm.RunList.Items.Count > 0 then begin
    if not OffersStart then begin
      WizardForm.RunList.Checked[0] := False;
      WizardForm.RunList.Visible := False;
    end else if Upgrading and not ClosedMine then
      // On an update, the box is ticked only when Setup closed the
      // person's own copy: it starts no copy for someone who ran none.
      WizardForm.RunList.Checked[0] := False;
  end;
  if DriverRestart then begin
    // What happened comes first, then why Windows must restart, then what
    // to do about the app.
    if DriverOutcome = 'ok' then
      Text := CustomMessage('DoneUpdated') + ExtraLines()
    else
      Text := OutcomeText();
    if DriverReplaced then
      Text := Text + #13#10#13#10 + CustomMessage('RestartReplacing')
    else
      Text := Text + #13#10#13#10 + CustomMessage('RestartStarting');
    if StartsAgain() then
      Text := Text + ' ' + CustomMessage('RestartAppAgain')
    else if ClosedMine then
      Text := Text + ' ' + CustomMessage('RestartStartYourself')
    else if (DriverOutcome = 'ok') and not Upgrading then
      Text := Text + #13#10#13#10 + CustomMessage('NextAfterRestart');
    Text := Text + #13#10#13#10 + CustomMessage('RestartNow');
  end else begin
    Text := OutcomeText();
    if not OffersStart then begin
      if PersonKnown then
        Text := Text + #13#10#13#10 + CustomMessage('StartYourself')
      else
        Text := Text + #13#10#13#10 + CustomMessage('StartYourselfUnknown');
    end;
    Text := Text + #13#10#13#10 + SetupMessage(msgClickFinish);
  end;
  WizardForm.FinishedLabel.Caption := Text;
  Grow := WizardForm.AdjustLabelHeight(WizardForm.FinishedLabel);
  WizardForm.RunList.Top := WizardForm.RunList.Top + Grow;
  WizardForm.YesRadio.Top := WizardForm.YesRadio.Top + Grow;
  WizardForm.NoRadio.Top := WizardForm.NoRadio.Top + Grow;
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
  Body: String;
begin
  Result := True;
  if (CurPageID <> wpReady) or WizardSilent then
    Exit;
  FindCopies(False);
  if CopiesMine + CopiesOther = 0 then
    Exit;
  // "Another account" only when Setup could tell whose copies they are;
  // "starts it again" only when it can.
  if not PersonKnown then
    Body := CustomMessage('CloseUnknown')
  else if (CopiesMine > 0) and (CopiesOther > 0) and CanStartAsPerson then
    Body := CustomMessage('CloseBoth')
  else if (CopiesMine > 0) and (CopiesOther > 0) then
    Body := CustomMessage('CloseBothNoStart')
  else if CopiesOther > 0 then
    Body := CustomMessage('CloseOther')
  else if CanStartAsPerson then
    Body := CustomMessage('CloseMine')
  else
    Body := CustomMessage('CloseMineNoStart');
  Result := SuppressibleTaskDialogMsgBox(CustomMessage('CloseInstruction'), Body, mbConfirmation,
    MB_OKCANCEL, [CustomMessage('CloseButton')], 0, IDOK) = IDOK;
end;

// Runs once the person has clicked Install (or right away when silent),
// before anything on the PC changes. A non-empty result stops Setup with
// that message and exit code 7.
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Version: TWindowsVersion;
begin
  Result := '';
  GetWindowsVersionEx(Version);
  if (Version.ProductType <> VER_NT_WORKSTATION) and not HasSwitch('ALLOWSERVER') then
    Result := CustomMessage('ServerRefused')
  else if IsTestBuild() and not TestSigningOn() and not HasSwitch('ALLOWTESTBUILD') then begin
    Result := CustomMessage('TestBuildRefused');
    // For whoever tests drivers and reads the log; not on the page.
    Log('On a PC that loads test-signed drivers in another way (a kernel debugger, signature enforcement turned off at boot), run Setup with /ALLOWTESTBUILD.');
  end else if not SamePath(ExpandConstant('{app}'), ProgramFilesDir()) then
    Result := FmtMessage(CustomMessage('DirRefused'), [ProgramFilesDir()]);
  if Result <> '' then begin
    Log('Setup stops before changing anything: ' + Result);
    Exit;
  end;
  // Closing the app comes last but one before the files go in, so that
  // nothing can stop Setup between the two.
  if not FindCopies(True) then begin
    // By what still runs: a copy of the person's, or only another account's.
    if CopiesMine > CopiesMineClosed then
      Result := CustomMessage('CloseFailedMine')
    else if PersonKnown and (CopiesOther > CopiesOtherClosed) then
      Result := CustomMessage('CloseFailedOther')
    else
      Result := CustomMessage('CloseFailedUnknown');
    // The person's own copy is closed although another couldn't be: it
    // starts again when Setup closes (DeinitializeSetup), or the person
    // is told to start it.
    if ClosedMine then begin
      if StartsAgain() and not Moving then
        Result := Result + #13#10#13#10 + CustomMessage('ClosedYoursStarts')
      else
        Result := Result + #13#10#13#10 + CustomMessage('ClosedYoursStartIt');
    end;
    Log('Setup stops: ' + Result);
    Exit;
  end;
  // Before anything can lose it, the earlier build's test certificate.
  RecordCertificate(AddBackslash(EarlierAppDir()) + 'driver\StreamToSpeaker.cer');
end;

// Copies the test certificate the build being replaced shipped, so that
// Install-Driver.ps1 can remove it once nothing needs it. The file itself
// is deleted only after the driver step ran.
procedure SavePreviousCertificate();
var
  OldCer: String;
begin
  OldCer := AddBackslash(EarlierAppDir()) + 'driver\StreamToSpeaker.cer';
  if FileExists(OldCer) then
    FileCopy(OldCer, ExpandConstant('{tmp}\previous-test-cert.cer'), False);
end;

// No copy of the app runs after Setup: the one it closed belonged to
// another account (or Setup couldn't tell whose it was), or Setup can't
// start the person's own again without administrator rights, or it runs
// as SYSTEM.
function NoAppAfterwards(): Boolean;
begin
  Result := (ClosedMine or ClosedOther) and not StartsAgain();
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
  if HasSwitch('ALLOWTESTBUILD') then
    Params := Params + ' -AllowTestBuild';
  if NoAppAfterwards() then begin
    Params := Params + ' -NoAppAfterwards';
    if RunningAsSystem() then
      Log('Not starting Stream To Speaker again: Setup runs as SYSTEM, with no person to start it for.')
    else if ClosedMine then
      Log('Not starting Stream To Speaker again: Setup was started with administrator rights, and the app would run with them too.')
    else if not PersonKnown then
      Log('Not starting Stream To Speaker again: Setup could not tell which account the copy it closed ran in.')
    else
      Log('Not starting Stream To Speaker again: the copy Setup closed ran in another account.');
  end;
  DriverResultLine := '';
  RunScript('Install-Driver.ps1', Params, Code);
  DriverOutcome := ResultWord(DriverResultLine, 'driver-step-result');
  if DriverOutcome = '' then
    DriverOutcome := 'script-failed';
  DriverRestart := ResultField(DriverResultLine, 'restart') = '1';
  DriverReplaced := ResultField(DriverResultLine, 'replaced') = '1';
  DriverNewOutput := ResultField(DriverResultLine, 'new-output') = '1';
  DriverKeptBefore := ResultField(DriverResultLine, 'kept-before') = '1';
  DriverProblem := ResultField(DriverResultLine, 'problem');
  DriverExtrasRemoved := StrToIntDef(ResultField(DriverResultLine, 'extras-removed'), 0);
  DriverExtrasLeft := StrToIntDef(ResultField(DriverResultLine, 'extras-left'), 0);
  DriverWasDefault := ResultField(DriverResultLine, 'was-default') = '1';
  DriverIsDefault := ResultField(DriverResultLine, 'is-default') = '1';
  DriverSoundMoved := ResultField(DriverResultLine, 'sound-moved') = '1';
  Log('Driver step: ' + DriverOutcome + ', restart ' + IntToStr(Ord(DriverRestart)) +
      ', problem ' + DriverProblem);
  // The earlier build's .cer goes only once the driver step ran (and so
  // dealt with its certificate): until then it is one more way for the
  // uninstaller to find that certificate.
  if (DriverOutcome <> 'script-failed') and (not IsTestBuild()) and FileExists(ExpandConstant('{app}\driver\StreamToSpeaker.cer')) then
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

// Removes from the earlier folder what releases put there (nothing in it
// is run).
procedure RemoveEarlierInstall();
var
  Code: Integer;
begin
  if not Moving then
    Exit;
  WizardForm.StatusLabel.Caption := CustomMessage('StatusEarlier');
  EarlierLine := '';
  RunScript('Remove-EarlierInstall.ps1', ' -Folder "' + EarlierDir + '"', Code);
  EarlierResult := ResultWord(EarlierLine, 'earlier-install-result');
  if EarlierResult = '' then
    EarlierResult := 'left-alone';
end;

// "Start at sign-in" belongs to the account that ran Setup, so it is
// written as that account (an elevated Setup's own HKCU can be the
// administrator's). An interactive Setup does what the box on the tasks
// page says; a silent one only when the tasks are given on its command
// line (/TASKS or /MERGETASKS), and leaves the setting as it is otherwise.
// When what Setup runs doesn't act on the person's account (started with
// "Run as administrator" from another account, or the person couldn't be
// told), it leaves the setting alone and says so. When the look for running
// copies, which tells this, couldn't run at all, Setup writes the setting
// as it always did: as the original user (ExecAsOriginalUser), who is the
// person when Setup asked for administrator rights itself, and Setup's own
// account when it was started with them.
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
  if not CanActAsPerson then begin
    Log('Start at sign-in: left as it is; Setup runs as another account than the person who ran it, or could not tell who that is.');
    SignInSkipped := Wanted;
    Exit;
  end;
  Exe := ExpandConstant('{app}\{#MyAppExeName}');
  if Wanted then begin
    Log('Start at sign-in: on, reg.exe exit code ' + IntToStr(RegAsOriginalUser(
      'add "' + RunKey + '" /v "{#MyAppName}" /t REG_SZ /d "\"' + Exe + '\" --startup" /f')));
    // Turned off in Task Manager or Settings earlier: the person has now
    // asked for it again.
    RegAsOriginalUser('delete "' + StartupApprovedKey + '" /v "{#MyAppName}" /f');
    SignInTurnedOff := False;
  end else if SignInEntryExists() then
    Log('Start at sign-in: off, reg.exe exit code ' + IntToStr(RegAsOriginalUser(
      'delete "' + RunKey + '" /v "{#MyAppName}" /f')));
end;

// When Windows must restart and the person's copy was closed, the app
// runs again after the restart too, once, for that account; when start at
// sign-in is on, that already does it (an entry the person turned off in
// Task Manager or Settings doesn't).
procedure ScheduleAfterRestart();
begin
  if not DriverRestart or not StartsAgain() then
    Exit;
  if SignInEntryExists() and not SignInTurnedOff then
    Exit;
  Log('Starting Stream To Speaker once after the restart: reg.exe exit code ' + IntToStr(RegAsOriginalUser(
    'add "' + RunOnceKey + '" /v "{#MyAppName}" /t REG_SZ /d "\"' + ExpandConstant('{app}\{#MyAppExeName}') + '\" --startup" /f')));
end;

procedure StartAppAgain();
var
  Code: Integer;
begin
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
    RemoveEarlierInstall();
    ScheduleAfterRestart();
  end else if CurStep = ssDone then begin
    // The person's copy that Setup closed starts again: after a silent
    // install, and after an interactive one that needs a restart (which has
    // no box for it on its last page); an interactive one without a restart
    // offers the box.
    if StartsAgain() then begin
      if WizardSilent then
        StartAppAgain()
      else if DriverRestart and not WizardForm.YesRadio.Checked then
        StartAppAgain();
    end;
  end;
end;

// Setup closed the app and then stopped before the new files were in
// place (cancelled, or failed and rolled back): the earlier version is
// still installed, so start it again rather than leave the PC without it.
// An earlier install in another folder isn't started (Setup runs nothing
// from there).
procedure DeinitializeSetup();
begin
  if not Installed and StartsAgain() and not Moving then
    StartAppAgain();
end;

// Only an interactive install restarts the PC: a silent one returns 3010
// instead (GetCustomSetupExitCode), so that a deployment tool decides when.
function NeedRestart(): Boolean;
begin
  Result := DriverRestart and not WizardSilent;
end;

// Setup's exit code: 20 when Setup couldn't finish setting up the sound
// driver; 3010 for a silent install that needs a restart.
function GetCustomSetupExitCode(): Integer;
begin
  Result := 0;
  if DriverOutcome <> 'ok' then
    Result := 20
  else if WizardSilent and DriverRestart then
    Result := 3010;
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

// The app is closed and the driver, the certificates, the firewall rules
// and the start-at-sign-in entries removed here, before [UninstallRun] and
// before any file is deleted, reading back what is left.
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Code: Integer;
  Text: String;
begin
  if CurUninstallStep = usUninstall then begin
    LeftLines := '';
    StaysLines := '';
    RunScript('Find-RunningCopies.ps1', ' -Folders "' + ExpandConstant('{app}') + '" -Close', Code);
    RunScript('Uninstall-Driver.ps1', '', Code);
    RunScript('Remove-TestCertificate.ps1', ' -CerPath "' + ExpandConstant('{app}\driver\StreamToSpeaker.cer') + '"', Code);
    RunScript('Set-FirewallRule.ps1', ' -Remove', Code);
    RunScript('Remove-SignInEntries.ps1', '', Code);
  end else if (CurUninstallStep = usPostUninstall) and (LeftLines + StaysLines <> '') then begin
    // Said before Inno Setup's own last message, which then no longer
    // reads as if everything went. Trying again is advised only when
    // something is left that another uninstall may remove.
    Log('Left on this PC:' + #13#10 + LeftLines + StaysLines);
    if not UninstallSilent then begin
      Text := FmtMessage(CustomMessage('UninstallLeft'), [Trim(LeftLines + StaysLines)]);
      if LeftLines <> '' then
        Text := Text + #13#10#13#10 + CustomMessage('UninstallRetry');
      MsgBox(Text, mbError, MB_OK);
    end;
  end;
end;

// Only an interactive uninstall asks for the restart: a very silent one
// without /NORESTART would restart the PC without asking, and the loaded
// driver does no harm until the next restart.
function UninstallNeedRestart(): Boolean;
begin
  Result := DriverWasLoaded and not UninstallSilent;
end;
