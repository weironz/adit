; Adit — Inno Setup installer script.
;
; Build (from the repo root, after `cargo build --release -p adit-app`):
;   & "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe" /DAppVersion=0.1.6 installer\adit.iss
;
; Produces target\release\adit-installer-v<version>.exe — a normal install
; wizard (welcome, choose location, optional desktop shortcut, progress, finish)
; with a proper Add/Remove Programs uninstall entry. Being a standard, signed
; Inno Setup stub, it also trips far fewer antivirus false positives than a
; hand-rolled "drop an embedded exe" installer.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#define AppName "Adit"
#define AppExe "Adit.exe"
#define AppPublisher "weironz"
#define AppURL "https://github.com/weironz/adit"

[Setup]
; A stable AppId keeps upgrades and the uninstall entry tracked across versions.
AppId={{7F3B9C1E-2A44-4D8E-B6F1-9E5C7A2D4B10}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#AppPublisher}
AppPublisherURL={#AppURL}
AppSupportURL={#AppURL}
VersionInfoVersion={#AppVersion}
; Per-user install by default (no UAC) → %LOCALAPPDATA%\Programs\Adit, matching
; the previous install location; the user can still choose "all users" and a
; custom folder in the wizard.
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesInstallIn64BitMode=x64compatible
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName={#AppName}
; The app holds a mutex of this name while running; setup/uninstall detect it
; and ask the user to close Adit cleanly instead of force-closing the window.
AppMutex=AditAppInstanceMutex
SetupIconFile=..\crates\adit-app\assets\icon.ico
OutputDir=..\target\release
OutputBaseFilename=adit-installer-v{#AppVersion}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
CloseApplications=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: checkedonce

[Files]
Source: "..\target\release\adit-app.exe"; DestDir: "{app}"; DestName: "{#AppExe}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#AppExe}"; Description: "{cm:LaunchProgram,{#AppName}}"; Flags: nowait postinstall skipifsilent
