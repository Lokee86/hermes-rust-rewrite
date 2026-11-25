; Per-user installer: no administrator rights. Built by scripts/package-windows.ps1 with Inno Setup 6.
#ifndef SourceDir
  #error Pass /DSourceDir=<folder holding Hermes.exe>
#endif
[Setup]
AppId={{6C0B1F4E-5B1D-4C57-9E0C-6E6F7C1D2A90}
AppName=Hermes (Rust)
AppVersion={#AppVersion}
AppPublisher=Hermes Rust community build
DefaultDirName={localappdata}\Programs\Hermes Rust
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir={#OutputDir}
OutputBaseFilename=Hermes-windows-AMD64-setup
SetupIconFile={#SourceDir}\Hermes.ico
UninstallDisplayIcon={app}\Hermes.ico
LicenseFile={#SourceDir}\Notices\Hermes-Rust-LICENSE
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
CloseApplications=yes
[InstallDelete]
; The bundled runtime of an earlier version. User data is elsewhere.
Type: filesandordirs; Name: "{app}\python"
[Files]
Source: "{#SourceDir}\*"; DestDir: "{app}"; Flags: recursesubdirs ignoreversion
[Tasks]
Name: desktopicon; Description: "Create a desktop shortcut"; Flags: unchecked
[Icons]
Name: "{autoprograms}\Hermes (Rust)"; Filename: "{app}\Hermes.exe"; IconFilename: "{app}\Hermes.ico"
Name: "{autodesktop}\Hermes (Rust)"; Filename: "{app}\Hermes.exe"; IconFilename: "{app}\Hermes.ico"; Tasks: desktopicon
[Run]
Filename: "{app}\Hermes.exe"; Description: "Start Hermes"; Flags: nowait postinstall skipifsilent
