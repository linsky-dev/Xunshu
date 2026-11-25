; Inno 仅提供安装向导和卸载入口。版本状态与模型自检仍由 xs-install 管理。
[Setup]
AppId=Xunshu-Alphatest
AppName=寻枢 Xunshu 测试候选版
AppVersion={#AppVersionText}
AppPublisher=Xunshu
DefaultDirName={autopf}\Xunshu Test
DefaultGroupName=寻枢测试候选版
DisableProgramGroupPage=yes
DisableDirPage=no
PrivilegesRequired=admin
MinVersion=10.0
ArchitecturesAllowed=x64os
ArchitecturesInstallIn64BitMode=x64os
Compression=lzma2/fast
SolidCompression=yes
SetupLogging=yes
UninstallLogging=yes
OutputBaseFilename=Xunshu-Windows-x64-Setup-Test
OutputDir={#StageDir}
UninstallDisplayIcon={app}\xs-install.exe
InfoBeforeFile={#StageDir}\README.txt

[Files]
Source: "{#StageDir}\payload\*"; DestDir: "{tmp}\payload"; Flags: recursesubdirs ignoreversion deleteafterinstall
Source: "{#StageDir}\manager\*"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\README.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\VERSION.json"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\licenses\*"; DestDir: "{app}\licenses"; Flags: recursesubdirs ignoreversion

[Icons]
Name: "{group}\寻枢测试候选版"; Filename: "{app}\xs-install.exe"; Parameters: "--root ""{app}"" launch"; WorkingDir: "{app}"
Name: "{group}\卸载寻枢测试候选版"; Filename: "{uninstallexe}"

[Code]
function GetFileAttributesW(const FileName: string): LongWord;
  external 'GetFileAttributesW@kernel32.dll stdcall';

function GetDriveTypeW(const Root: string): LongWord;
  external 'GetDriveTypeW@kernel32.dll stdcall';

function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Path, Root: String;
  Attributes: LongWord;
begin
  Result := '';
  Path := RemoveBackslashUnlessRoot(ExpandConstant('{app}'));
  Root := ExtractFileDrive(Path) + '\';
  if (Length(Root) <> 3) or (Root[2] <> ':') or (Path = Root) then
  begin
    Result := '请选择本地固定磁盘上的安装子目录。';
    Exit;
  end;
  if GetDriveTypeW(Root) <> 3 then
  begin
    Result := '请选择本地固定磁盘上的安装目录。';
    Exit;
  end;
  while CompareText(Path, Root) <> 0 do
  begin
    Attributes := GetFileAttributesW(Path);
    if (Attributes <> $FFFFFFFF) and ((Attributes and $0410) <> $0010) then
    begin
      Result := '安装目录包含链接、接合点或普通文件。请选择实际目录。';
      Exit;
    end;
    Path := ExtractFileDir(Path);
  end;
end;

function NativeAction(const Arguments: String): String;
var
  ExitCode: Integer;
  Root: String;
begin
  Root := ExpandConstant('{app}');
  if not ExecAndLogOutput(Root + '\xs-install.exe', '--root ' + AddQuotes(Root) + ' ' + Arguments,
    Root, SW_SHOWNORMAL, ewWaitUntilTerminated, ExitCode, nil) then
    Result := '无法启动寻枢的原生安装工具。' + SysErrorMessage(ExitCode)
  else if ExitCode <> 0 then
    Result := '寻枢原生安装操作失败（退出码 ' + IntToStr(ExitCode) + '）。请查看安装日志。'
  else
    Result := '';
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  Problem: String;
begin
  if CurStep = ssPostInstall then
  begin
    Problem := NativeAction('install ' + AddQuotes(ExpandConstant('{tmp}\payload')) + ' --sha256 {#PayloadSha256}');
    if Problem <> '' then
      RaiseException(Problem);
  end;
end;

function InitializeUninstall(): Boolean;
var
  Problem: String;
begin
  Problem := NativeAction('uninstall');
  Result := Problem = '';
  if not Result then
    SuppressibleMsgBox(Problem, mbError, MB_OK, IDOK);
end;
