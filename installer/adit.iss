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
; Default to a standard all-users install in C:\Program Files (requires admin /
; a UAC prompt). PrivilegesRequiredOverridesAllowed=dialog shows a "for all
; users / just me" page first, so a user without admin rights can still fall
; back to a per-user install under %LOCALAPPDATA%\Programs. {autopf} resolves to
; Program Files in admin mode and to that per-user folder otherwise.
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
PrivilegesRequired=admin
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesInstallIn64BitMode=x64compatible
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName={#AppName}
SetupIconFile=..\crates\adit-app\assets\icon.ico
OutputDir=..\target\release
OutputBaseFilename=adit-installer-v{#AppVersion}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
; If Adit is running, the "Preparing to Install" page detects it and offers to
; close it automatically (selected by default); the user consents by continuing.
; `force` closes it gracefully first and terminates only if that fails, so a
; running instance never blocks the install with a manual-close error.
CloseApplications=force
RestartApplications=no

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
; Interactive install: offer a "launch Adit" checkbox on the finish page.
Filename: "{app}\{#AppExe}"; Description: "{cm:LaunchProgram,{#AppName}}"; Flags: nowait postinstall skipifsilent
; Silent (background) update: relaunch Adit automatically, de-elevated so it
; does not keep running as admin.
Filename: "{app}\{#AppExe}"; Flags: nowait runasoriginaluser; Check: WizardSilent
