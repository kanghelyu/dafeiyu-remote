; SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
#ifndef SourceDir
  #error SourceDir must be supplied with /DSourceDir=...
#endif
#ifndef OutputDir
  #error OutputDir must be supplied with /DOutputDir=...
#endif
#ifndef AppVersion
  #error AppVersion must be supplied with /DAppVersion=...
#endif

[Setup]
AppId={{9DB56B13-75D0-4D95-A0E7-DF9E6294B984}
AppName=Dafeiyu Remote
AppVersion={#AppVersion}
AppPublisher=Dafeiyu Remote Project
AppPublisherURL=https://github.com/kanghelyu/dafeiyu-remote
AppSupportURL=https://github.com/kanghelyu/dafeiyu-remote/issues
DefaultDirName={autopf}\Dafeiyu Remote
DefaultGroupName=Dafeiyu Remote
DisableProgramGroupPage=yes
LicenseFile={#SourceDir}\docs\LICENSE
InfoBeforeFile={#SourceDir}\docs\relay-deployment.md
OutputDir={#OutputDir}
OutputBaseFilename=dafeiyu-{#AppVersion}-windows-x86_64-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64os
ArchitecturesInstallIn64BitMode=x64os
PrivilegesRequired=lowest
UninstallDisplayName=Dafeiyu Remote {#AppVersion}
UninstallDisplayIcon={app}\dfy.ico
SetupIconFile={#SourceDir}\dfy.ico
CloseApplications=yes
RestartApplications=no

[Files]
Source: "{#SourceDir}\dfy.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\dfy-relay.exe"; DestDir: "{app}"; Flags: ignoreversion
#ifdef HasManager
Source: "{#SourceDir}\dfy-manager.exe"; DestDir: "{app}"; Flags: ignoreversion
#endif
Source: "{#SourceDir}\dfy.ico"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\docs\*"; DestDir: "{app}\docs"; Flags: ignoreversion recursesubdirs

[Icons]
Name: "{group}\Dafeiyu CLI"; Filename: "{app}\dfy.exe"; WorkingDir: "{app}"; IconFilename: "{app}\dfy.ico"
Name: "{group}\Dafeiyu Relay"; Filename: "{app}\dfy-relay.exe"; WorkingDir: "{app}"; IconFilename: "{app}\dfy.ico"
#ifdef HasManager
Name: "{group}\Dafeiyu Manager"; Filename: "{app}\dfy-manager.exe"; WorkingDir: "{app}"; IconFilename: "{app}\dfy.ico"
Name: "{autodesktop}\Dafeiyu Manager"; Filename: "{app}\dfy-manager.exe"; WorkingDir: "{app}"; IconFilename: "{app}\dfy.ico"; Tasks: desktopicon
#endif
Name: "{group}\Relay deployment notes"; Filename: "{app}\docs\relay-deployment.md"
Name: "{autodesktop}\Dafeiyu CLI"; Filename: "{app}\dfy.exe"; WorkingDir: "{app}"; IconFilename: "{app}\dfy.ico"; Tasks: desktopicon

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Additional shortcuts:"

[Messages]
#ifdef HasManager
WelcomeLabel2=This installs Dafeiyu Manager, command-line tools, and relay. No Windows service is installed.
#else
WelcomeLabel2=This installs the Dafeiyu command-line tools and relay. No Windows service is installed.
#endif
