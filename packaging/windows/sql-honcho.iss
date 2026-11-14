; Inno Setup script. package.ps1 passes the version and paths.
#ifndef Version
  #define Version "0.0.0"
#endif

[Setup]
AppId={{6F3E9D2B-4C1A-4E8B-9A57-2D5C8F1B0E64}
AppName=sql-honcho
AppVersion={#Version}
AppPublisher=NonGT
AppPublisherURL=https://github.com/NonGT/sql-honcho
DefaultDirName={autopf}\sql-honcho
DefaultGroupName=sql-honcho
DisableProgramGroupPage=yes
; per-user by default, so no admin prompt; the wizard offers all users
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir={#OutDir}
OutputBaseFilename=sql-honcho-{#Version}-windows-x64-setup
SetupIconFile={#Root}\assets\icon\sql-honcho.ico
UninstallDisplayIcon={app}\sql-honcho.exe
LicenseFile={#Root}\LICENSE
Compression=lzma2
SolidCompression=yes
WizardStyle=modern

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#Root}\target\release\sql-honcho.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Root}\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Root}\README.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\sql-honcho"; Filename: "{app}\sql-honcho.exe"
Name: "{autodesktop}\sql-honcho"; Filename: "{app}\sql-honcho.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\sql-honcho.exe"; Description: "{cm:LaunchProgram,sql-honcho}"; Flags: nowait postinstall skipifsilent
