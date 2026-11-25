; Inno 仅提供安装向导和卸载入口。版本状态与模型自检仍由 xs-install 管理。
#define XunshuAppId "Xunshu-Alphatest"
[Setup]
AppId={#XunshuAppId}
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

[UninstallDelete]
; 原生卸载成功后清理安装状态。目录仅在完全为空时移除。
Type: files; Name: "{app}\active.json"
Type: files; Name: "{app}\install.lock"
Type: dirifempty; Name: "{app}\self-test-data"
Type: dirifempty; Name: "{app}\versions"

[Code]
const
  UninstallKey = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\{#XunshuAppId}_is1';
  InvalidAttributes = $FFFFFFFF;

function GetFileAttributesW(const FileName: string): LongWord;
  external 'GetFileAttributesW@kernel32.dll stdcall';

function GetDriveTypeW(const Root: string): LongWord;
  external 'GetDriveTypeW@kernel32.dll stdcall';

function CheckLocalDirectory(const Directory: String): String;
var
  Path, Root: String;
  Attributes: LongWord;
  ErrorCode: LongInt;
begin
  Result := '';
  Path := Directory;
  Root := ExtractFileDrive(Path) + '\';
  if (Length(Root) <> 3) or (Root[2] <> ':') or (CompareText(Path, Root) = 0) then
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
    if Attributes = InvalidAttributes then
    begin
      ErrorCode := DLLGetLastError;
      if (ErrorCode <> 2) and (ErrorCode <> 3) then
      begin
        Result := '无法核验安装目录。' + SysErrorMessage(ErrorCode);
        Exit;
      end;
    end
    else if (Attributes and (FILE_ATTRIBUTE_DIRECTORY or FILE_ATTRIBUTE_REPARSE_POINT)) <> FILE_ATTRIBUTE_DIRECTORY then
    begin
      Result := '安装目录包含链接、接合点或普通文件。请选择实际目录。';
      Exit;
    end;
    Path := ExtractFileDir(Path);
  end;
end;

function CheckDirectoryEntries(const Directory: String; const CheckLicenseTree: Boolean; var Empty: Boolean): Boolean;
var
  Entry: TFindRec;
  ChildEmpty: Boolean;
begin
  Result := False;
  Empty := True;
  if not FindFirst(Directory + '\*', Entry) then
    Exit;
  try
    repeat
      if (Entry.Name <> '.') and (Entry.Name <> '..') then
      begin
        Empty := False;
        if (Entry.Attributes and FILE_ATTRIBUTE_REPARSE_POINT) <> 0 then
          Exit;
        // 只有安装器递归写入的 licenses 树需要深入检查。
        if CheckLicenseTree or (CompareText(Entry.Name, 'licenses') = 0) then
        begin
          if (Entry.Attributes and FILE_ATTRIBUTE_DIRECTORY) <> 0 then
          begin
            if not CheckDirectoryEntries(Directory + '\' + Entry.Name, True, ChildEmpty) then
              Exit;
          end
          else if not CheckLicenseTree then
            Exit;
        end;
      end;
    until not FindNext(Entry);
    Result := True;
  finally
    FindClose(Entry);
  end;
end;

function IsRegularFile(const FileName: String): Boolean;
var
  Attributes: LongWord;
begin
  Attributes := GetFileAttributesW(FileName);
  Result := (Attributes <> InvalidAttributes) and
    ((Attributes and (FILE_ATTRIBUTE_DIRECTORY or FILE_ATTRIBUTE_REPARSE_POINT)) = 0);
end;

function IsOwnedInstall(const Directory, RegisteredDirectory: String): Boolean;
var
  Uninstaller, UninstallerName: String;
  VersionMetadata: AnsiString;
  Size: Int64;
begin
  Result := False;
  // 安装路径由受管理员保护的 Inno 卸载记录确定，再核对同目录的卸载数据及候选版元数据。
  if (CompareText(Directory, RegisteredDirectory) <> 0) or
    (not RegQueryStringValue(HKLM64, UninstallKey, 'UninstallString', Uninstaller)) then
    Exit;
  Uninstaller := RemoveQuotes(Uninstaller);
  UninstallerName := ExtractFileName(Uninstaller);
  if (CompareText(ExtractFileDir(Uninstaller), Directory) <> 0) or
    (Length(UninstallerName) <> 12) or (CompareText(Copy(UninstallerName, 1, 5), 'unins') <> 0) or
    (CompareText(ExtractFileExt(UninstallerName), '.exe') <> 0) or
    (not IsRegularFile(Uninstaller)) or (not IsRegularFile(ChangeFileExt(Uninstaller, '.dat'))) or
    (not IsRegularFile(Directory + '\xs-install.exe')) or (not IsRegularFile(Directory + '\active.json')) or
    (not IsRegularFile(Directory + '\VERSION.json')) then
    Exit;
  if not FileSize64(Directory + '\VERSION.json', Size) then
    Exit;
  if (Size <= 0) or (Size > 65536) then
    Exit;
  if LoadStringFromFile(Directory + '\VERSION.json', VersionMetadata) then
    Result := Pos('"status": "internal_test_candidate"', VersionMetadata) > 0;
end;

function CheckInstallDirectory(const Directory: String; const RequireOwnership: Boolean): String;
var
  RegisteredDirectory: String;
  Empty: Boolean;
begin
  Result := CheckLocalDirectory(Directory);
  if Result <> '' then
    Exit;
  if RegQueryStringValue(HKLM64, UninstallKey, 'InstallLocation', RegisteredDirectory) then
  begin
    RegisteredDirectory := RemoveBackslashUnlessRoot(RegisteredDirectory);
    if CompareText(RegisteredDirectory, Directory) <> 0 then
    begin
      Result := '已在其他目录安装寻枢。请使用原安装目录，或先卸载现有版本。';
      Exit;
    end;
  end;
  if not DirExists(Directory) then
  begin
    if RequireOwnership or (RegisteredDirectory <> '') then
      Result := '已登记的寻枢安装目录缺失，无法安全继续。';
    Exit;
  end;
  if not CheckDirectoryEntries(Directory, False, Empty) then
  begin
    Result := '安装目录包含无法安全处理的项目。请检查链接、接合点或权限。';
    Exit;
  end;
  if (RequireOwnership or (not Empty) or (RegisteredDirectory <> '')) and
    (not IsOwnedInstall(Directory, RegisteredDirectory)) then
    Result := '无法确认此目录属于寻枢。请选择新的空目录，现有文件已保留。';
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  Result := CheckInstallDirectory(RemoveBackslashUnlessRoot(ExpandConstant('{app}')), False);
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
  Problem := CheckInstallDirectory(RemoveBackslashUnlessRoot(ExpandConstant('{app}')), True);
  if Problem = '' then
    Problem := NativeAction('uninstall');
  Result := Problem = '';
  if not Result then
    SuppressibleMsgBox(Problem, mbError, MB_OK, IDOK);
end;
