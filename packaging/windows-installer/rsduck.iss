#define AppName "rsduck"
#ifndef AppVersion
#define AppVersion "0.1.0"
#endif
#ifndef SourceDir
#define SourceDir "..\..\service"
#endif
#ifndef OutputDir
#define OutputDir "..\..\dist"
#endif

[Setup]
AppId={{2B24BCB9-6A7A-4D35-8F86-2C0A1865A2F8}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher=dripai
AppPublisherURL=https://github.com/dripai/rsduck
AppSupportURL=https://github.com/dripai/rsduck/issues
AppUpdatesURL=https://github.com/dripai/rsduck/releases
DefaultDirName={autopf}\rsduck
DefaultGroupName=rsduck
DisableProgramGroupPage=yes
OutputDir={#OutputDir}
OutputBaseFilename=rsduck-windows-service-setup-x64
Compression=lzma2
SolidCompression=yes
ArchitecturesAllowed=x64
ArchitecturesInstallIn64BitMode=x64
PrivilegesRequired=admin
WizardStyle=modern
SetupLogging=yes
UninstallDisplayIcon={app}\rsduck.exe

[Dirs]
Name: "{app}\logs"
Name: "{app}\snapshot"

[Files]
Source: "{#SourceDir}\rsduck.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\rsduck-service.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\rsduck-service.xml"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\install-service.ps1"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\uninstall-service.ps1"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\rsduck.toml"; DestDir: "{app}"; Flags: onlyifdoesntexist
Source: "{#SourceDir}\init.sql"; DestDir: "{app}"; Flags: onlyifdoesntexist

[Icons]
Name: "{group}\rsduck Web Console"; Filename: "http://127.0.0.1:8080"
Name: "{group}\Uninstall rsduck"; Filename: "{uninstallexe}"

[Run]
Filename: "powershell.exe"; Parameters: "-NoProfile -ExecutionPolicy Bypass -File ""{app}\install-service.ps1"""; WorkingDir: "{app}"; StatusMsg: "Installing and starting rsduck service..."; Flags: runhidden waituntilterminated
Filename: "http://127.0.0.1:8080"; Description: "Open rsduck Web Console"; Flags: postinstall shellexec skipifsilent unchecked

[UninstallRun]
Filename: "powershell.exe"; Parameters: "-NoProfile -ExecutionPolicy Bypass -File ""{app}\uninstall-service.ps1"""; WorkingDir: "{app}"; Flags: runhidden waituntilterminated
