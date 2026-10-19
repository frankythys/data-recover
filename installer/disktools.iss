; DiskTools installer (Inno Setup 6). Build with installer\build-installer.ps1,
; which passes the version from Cargo.toml.
;
; The installer runs as administrator, asks Dutch or English, installs to
; Program Files and writes the choice to disktools.ini next to the exe; the
; app reads it at start. DiskTools itself always asks for administrator
; rights (manifest in the exe).

#ifndef AppVersion
  #define AppVersion "0.1.0"
#endif

[Setup]
AppId={{BD26C519-2492-425B-901A-EB76AE21EBA8}
AppName=DiskTools
AppVersion={#AppVersion}
AppVerName=DiskTools {#AppVersion}
AppPublisher=frankythys
AppPublisherURL=https://github.com/frankythys/data-recover
DefaultDirName={autopf}\DiskTools
DefaultGroupName=DiskTools
DisableProgramGroupPage=yes
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputDir=..\target\installer
OutputBaseFilename=DiskTools-Setup-{#AppVersion}
SetupIconFile=..\assets\disktools.ico
UninstallDisplayIcon={app}\disktools.exe
UninstallDisplayName=DiskTools
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
DisableWelcomePage=no
ShowLanguageDialog=yes
UsePreviousLanguage=no
CloseApplications=yes

[Languages]
Name: "nl"; MessagesFile: "compiler:Languages\Dutch.isl"
Name: "en"; MessagesFile: "compiler:Default.isl"

[Messages]
nl.WelcomeLabel2=Hiermee wordt [name/ver] op je computer geïnstalleerd.%n%nDiskTools herstelt verwijderde bestanden, ruimt je schijf op en controleert en beheert je schijven. Omdat het rechtstreeks met je schijven werkt, vraagt DiskTools bij elke start om administratorrechten.%n%nDe taal die je net hebt gekozen, wordt ook de taal van DiskTools. Je kunt die later in de app wijzigen.
en.WelcomeLabel2=This will install [name/ver] on your computer.%n%nDiskTools recovers deleted files, cleans up your disk and checks and manages your drives. Because it works directly with your drives, DiskTools asks for administrator rights every time it starts.%n%nThe language you just chose also becomes the language of DiskTools. You can change it later in the app.

[CustomMessages]
nl.LaunchApp=DiskTools nu starten
en.LaunchApp=Start DiskTools now

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"

[Files]
Source: "..\target\release\disktools.exe"; DestDir: "{app}"; Flags: ignoreversion

[INI]
; The app's language: "nl" or "en", the name of the chosen [Languages] entry.
Filename: "{app}\disktools.ini"; Section: "DiskTools"; Key: "taal"; String: "{language}"

[Icons]
Name: "{group}\DiskTools"; Filename: "{app}\disktools.exe"
Name: "{group}\{cm:UninstallProgram,DiskTools}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\DiskTools"; Filename: "{app}\disktools.exe"; Tasks: desktopicon

[Run]
; shellexec: lets Windows show the administrator prompt DiskTools asks for.
Filename: "{app}\disktools.exe"; Description: "{cm:LaunchApp}"; Flags: nowait postinstall skipifsilent shellexec

[UninstallDelete]
Type: files; Name: "{app}\disktools.ini"
