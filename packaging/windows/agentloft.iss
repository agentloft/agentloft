; Agentloft's Windows installer (Inno Setup 6), built by
; .github/workflows/release.yml:
;   iscc /DAppVersion=<version> /DSourceDir=<staged files> /DOutputDir=<dir> agentloft.iss
; A per-user install: no administrator rights, nothing outside the user's
; profile. The data home (%USERPROFILE%\.agentloft) is left alone on
; uninstall.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef SourceDir
  #define SourceDir "..\..\dist\Agentloft"
#endif
#ifndef OutputDir
  #define OutputDir "..\..\dist"
#endif

[Setup]
AppId={{FF99D825-B65F-469A-92D3-941F8A683BC5}
AppName=Agentloft
AppVersion={#AppVersion}
AppPublisher=Agentloft
AppPublisherURL=https://agentloft.promptanddone.com/
AppSupportURL=https://github.com/agentloft/agentloft/issues
DefaultDirName={localappdata}\Programs\Agentloft
DefaultGroupName=Agentloft
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir={#OutputDir}
OutputBaseFilename=Agentloft-windows-x64-setup
SetupIconFile=..\..\assets\icon\agentloft.ico
UninstallDisplayIcon={app}\agentloft.exe
LicenseFile=..\..\LICENSE
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
CloseApplications=force
RestartApplications=no

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#SourceDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{autoprograms}\Agentloft"; Filename: "{app}\agentloft.exe"
Name: "{autodesktop}\Agentloft"; Filename: "{app}\agentloft.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\agentloft.exe"; Description: "{cm:LaunchProgram,Agentloft}"; Flags: nowait postinstall skipifsilent

[Code]
// The daemon keeps running after the app's windows close, and holds its
// files open: stop it (and the agents it runs) before files are replaced
// or removed.
procedure StopDaemon();
var
  Code: Integer;
begin
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /T /IM agentloftd.exe', '', SW_HIDE, ewWaitUntilTerminated, Code);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  StopDaemon();
  Result := '';
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then
    StopDaemon();
end;
