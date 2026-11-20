[Setup]
AppId=RustTalk
AppName=RustTalk
AppVersion={#Version}
DefaultDirName={localappdata}\Programs\RustTalk
DefaultGroupName=RustTalk
DisableProgramGroupPage=yes
OutputBaseFilename=rusttalk-{#Version}-windows-x86_64
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
PrivilegesRequired=lowest
CloseApplications=force
RestartApplications=no
SetupMutex=RustTalkSetup

[Files]
Source: "{#Client}"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\RustTalk"; Filename: "{app}\rusttalk-client.exe"

[Registry]
Root: HKCU; Subkey: "Software\RustTalk\Installer"; ValueType: string; ValueName: "Version"; ValueData: "{#Version}.0"; Flags: uninsdeletekey

[Run]
Filename: "{app}\rusttalk-client.exe"; Flags: nowait; Check: IsUpdate

[Code]
function IsUpdate: Boolean;
begin
  Result := ExpandConstant('{param:RUSTTALKUPDATE|0}') = '1';
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Installed: String;
  OldVersion, NewVersion: Int64;
begin
  Result := '';
  if RegQueryStringValue(HKCU, 'Software\RustTalk\Installer', 'Version', Installed) then
    if StrToVersion(Installed, OldVersion) and StrToVersion('{#Version}.0', NewVersion) then
      if ComparePackedVersion(NewVersion, OldVersion) < 0 then
        Result := 'A newer version of RustTalk is already installed.';
end;
