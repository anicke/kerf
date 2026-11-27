; Inno Setup script for the Kerf installer. The release workflow compiles it with
;   ISCC /DAppVersion=0.1.0 /DBinDir=<absolute folder with the .exe files> /DOutputDir=<folder>
;        /DOutputName=Kerf-v0.1.0-x86_64-setup packaging\windows\kerf.iss
; It installs for the current user by default (no administrator prompt); the first page
; offers installing for all users instead.

#ifndef AppVersion
  #error Pass /DAppVersion=<version>
#endif
#ifndef BinDir
  #error Pass /DBinDir=<folder with kerf.exe, kerf-cli.exe and kerf-mcp.exe>
#endif
#ifndef OutputDir
  #define OutputDir "."
#endif
#ifndef OutputName
  #define OutputName "Kerf-" + AppVersion + "-setup"
#endif

[Setup]
; Never change AppId: Windows uses it to find an existing installation to upgrade.
AppId={{AC0C7E44-96D9-416A-BE9D-9A61B2A0CE87}
AppName=Kerf
AppVersion={#AppVersion}
AppPublisher=The Kerf contributors
AppPublisherURL=https://github.com/anicke/kerf
AppSupportURL=https://github.com/anicke/kerf/issues
AppUpdatesURL=https://github.com/anicke/kerf/releases
DefaultDirName={autopf}\Kerf
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
ChangesAssociations=yes
SetupIconFile=..\icons\kerf.ico
UninstallDisplayIcon={app}\kerf.exe
UninstallDisplayName=Kerf
WizardStyle=modern
Compression=lzma2/max
SolidCompression=yes
OutputDir={#OutputDir}
OutputBaseFilename={#OutputName}

[Tasks]
Name: desktopicon; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#BinDir}\kerf.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BinDir}\kerf-cli.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BinDir}\kerf-mcp.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\LICENSE-MIT"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\LICENSE-APACHE"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Kerf"; Filename: "{app}\kerf.exe"
Name: "{autodesktop}\Kerf"; Filename: "{app}\kerf.exe"; Tasks: desktopicon

[Registry]
; .kerf projects open in Kerf.
Root: HKA; Subkey: "Software\Classes\.kerf"; ValueType: string; ValueName: ""; ValueData: "Kerf.Project"; Flags: uninsdeletevalue
Root: HKA; Subkey: "Software\Classes\.kerf\OpenWithProgids"; ValueType: string; ValueName: "Kerf.Project"; ValueData: ""; Flags: uninsdeletevalue
Root: HKA; Subkey: "Software\Classes\Kerf.Project"; ValueType: string; ValueName: ""; ValueData: "Kerf project"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\Kerf.Project\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\kerf.exe,0"
Root: HKA; Subkey: "Software\Classes\Kerf.Project\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\kerf.exe"" ""%1"""
; Kerf appears under "Open with" for audio files, without taking them over.
Root: HKA; Subkey: "Software\Classes\Applications\kerf.exe"; ValueType: string; ValueName: "FriendlyAppName"; ValueData: "Kerf"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\Applications\kerf.exe\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\kerf.exe"" ""%1"""
Root: HKA; Subkey: "Software\Classes\Applications\kerf.exe\SupportedTypes"; ValueType: string; ValueName: ".kerf"; ValueData: ""
Root: HKA; Subkey: "Software\Classes\Applications\kerf.exe\SupportedTypes"; ValueType: string; ValueName: ".wav"; ValueData: ""
Root: HKA; Subkey: "Software\Classes\Applications\kerf.exe\SupportedTypes"; ValueType: string; ValueName: ".flac"; ValueData: ""
Root: HKA; Subkey: "Software\Classes\Applications\kerf.exe\SupportedTypes"; ValueType: string; ValueName: ".mp3"; ValueData: ""
Root: HKA; Subkey: "Software\Classes\Applications\kerf.exe\SupportedTypes"; ValueType: string; ValueName: ".ogg"; ValueData: ""
Root: HKA; Subkey: "Software\Classes\Applications\kerf.exe\SupportedTypes"; ValueType: string; ValueName: ".opus"; ValueData: ""
Root: HKA; Subkey: "Software\Classes\Applications\kerf.exe\SupportedTypes"; ValueType: string; ValueName: ".aif"; ValueData: ""
Root: HKA; Subkey: "Software\Classes\Applications\kerf.exe\SupportedTypes"; ValueType: string; ValueName: ".aiff"; ValueData: ""
Root: HKA; Subkey: "Software\Classes\Applications\kerf.exe\SupportedTypes"; ValueType: string; ValueName: ".m4a"; ValueData: ""

[Run]
Filename: "{app}\kerf.exe"; Description: "{cm:LaunchProgram,Kerf}"; Flags: nowait postinstall skipifsilent
