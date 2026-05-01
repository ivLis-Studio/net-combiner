#define MyAppVersion GetEnv("NET_COMBINER_VERSION")
#if MyAppVersion == ""
#define MyAppVersion "0.1.0"
#endif

[Setup]
AppId={{2E0E49D4-6AFD-4C8C-8D99-A5D5CF93A6A4}
AppName=net-combiner
AppVersion={#MyAppVersion}
AppPublisher=net-combiner
AppPublisherURL=https://github.com/ivLis-Studio/net-combiner
AppSupportURL=https://github.com/ivLis-Studio/net-combiner/issues
AppUpdatesURL=https://github.com/ivLis-Studio/net-combiner/releases
DefaultDirName={autopf}\net-combiner
DefaultGroupName=net-combiner
DisableProgramGroupPage=yes
OutputDir=.
OutputBaseFilename=net-combiner-windows-x86_64-setup
SetupIconFile=..\..\assets\net-combiner-icon.ico
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64
ArchitecturesInstallIn64BitMode=x64
PrivilegesRequired=admin
UninstallDisplayIcon={app}\net-combiner.exe

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "..\..\dist\net-combiner\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{group}\net-combiner"; Filename: "{app}\net-combiner.exe"
Name: "{commondesktop}\net-combiner"; Filename: "{app}\net-combiner.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\net-combiner.exe"; Description: "{cm:LaunchProgram,net-combiner}"; Flags: nowait postinstall skipifsilent
