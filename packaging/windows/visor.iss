; Inno Setup script for Visor's Windows installer. Built by
; .github/workflows/windows.yml:
;   ISCC /DAppVersion=0.1.0 /DSourceDir=<staged files> packaging\windows\visor.iss
; Installs per user by default (no administrator prompt).

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef SourceDir
  #define SourceDir "..\..\dist\windows\Visor"
#endif

[Setup]
AppId={{5C0E7D6A-3B41-4C59-9C1E-6F8B2A9D4E21}
AppName=Visor
AppVersion={#AppVersion}
AppVerName=Visor {#AppVersion}
AppPublisher=Visor contributors
AppPublisherURL=https://visor-cam.pages.dev/
AppSupportURL=https://github.com/rrramirez/visor-cam/issues
DefaultDirName={autopf}\Visor
DefaultGroupName=Visor
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.19041
LicenseFile=..\..\LICENSE
SetupIconFile=visor.ico
UninstallDisplayIcon={app}\visor.exe
UninstallDisplayName=Visor
OutputDir=..\..\dist\windows
OutputBaseFilename=Visor-Setup-{#AppVersion}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
CloseApplications=yes

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#SourceDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs

[Icons]
Name: "{autoprograms}\Visor"; Filename: "{app}\visor.exe"
Name: "{autodesktop}\Visor"; Filename: "{app}\visor.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\visor.exe"; Description: "{cm:LaunchProgram,Visor}"; Flags: nowait postinstall skipifsilent
