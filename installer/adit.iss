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
; /DArch=arm64 builds the Windows-on-ARM installer from natively-built arm64
; binaries. Only that one carries the architecture in its filename; the x64
; installer keeps the name it has always had, because updaters shipped before
; 0.1.61 take the first .exe on the release and must keep landing on the build
; they are already running. See pick_installer_asset in crates/adit-ui.
;
; The separator is an underscore, and that is load-bearing: the GitHub API
; returns a release's assets sorted BY NAME, not by upload order, and '-' (0x2D)
; sorts before '.' (0x2E) — so `...-arm64.exe` came first and every old updater
; on an x64 machine was handed the arm64 build, which refuses to run there.
; '_' (0x5F) sorts after '.', putting the x64 installer first. v0.1.61 shipped
; with the wrong separator; its asset was renamed by hand.
#ifndef Arch
  #define Arch "x64"
#endif
; Where cargo left the binaries, relative to each target dir. A native build
; uses "release"; cross-compiling puts them under the triple, so building the
; arm64 installer on an x64 machine needs
; /DBuildDir=aarch64-pc-windows-msvc\release. CI builds arm64 natively and
; leaves this alone — packaging binaries of the wrong architecture is exactly
; the failure this define exists to make impossible to do by accident.
#ifndef BuildDir
  #define BuildDir "release"
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
; The arm64 installer refuses to run on x64 outright — there is no sense in
; letting it. The x64 one keeps allowing arm64, where Windows emulates it; that
; is what every Windows-on-ARM user has been running until now, and it stays the
; fallback for anyone who downloads the wrong file.
#if Arch == "arm64"
ArchitecturesAllowed=arm64
ArchitecturesInstallIn64BitMode=arm64
#else
ArchitecturesInstallIn64BitMode=x64compatible
#endif
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName={#AppName}
SetupIconFile=..\crates\adit-app\assets\icon.ico
OutputDir=..\target\release
#if Arch == "arm64"
OutputBaseFilename=adit-installer-v{#AppVersion}_arm64
#else
OutputBaseFilename=adit-installer-v{#AppVersion}
#endif
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
Source: "..\target\{#BuildDir}\adit-app.exe"; DestDir: "{app}"; DestName: "{#AppExe}"; Flags: ignoreversion
; The native RDP client runs as a separate helper process (IronRDP can't share
; a Cargo.lock with the main app's russh), built from its own workspace. Adit
; locates it next to adit.exe.
Source: "..\crates\adit-rdp\target\{#BuildDir}\adit-rdp-host.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Run]
; Interactive install: offer a "launch Adit" checkbox on the finish page.
Filename: "{app}\{#AppExe}"; Description: "{cm:LaunchProgram,{#AppName}}"; Flags: nowait postinstall skipifsilent
; Silent (background) update: relaunch Adit automatically, de-elevated so it
; does not keep running as admin.
Filename: "{app}\{#AppExe}"; Flags: nowait runasoriginaluser; Check: WizardSilent
