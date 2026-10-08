; Tidemark per-user setup. Build through scripts/build-windows.ps1.
#if Ver < EncodeVer(7, 1, 0)
  #error Inno Setup 7.1.0 or newer is required
#endif
#ifndef AppVersion
  #error AppVersion must be supplied by the build
#endif
#ifndef PayloadDir
  #error PayloadDir must be supplied by the build
#endif
#ifndef OutputDir
  #error OutputDir must be supplied by the build
#endif
#define AppId "io.github.zbndev.Tidemark"

[Setup]
AppId={#AppId}
AppName=Tidemark
AppVersion={#AppVersion}
AppPublisher=zbndev
AppPublisherURL=https://github.com/zbndev/tidemark
AppSupportURL=https://github.com/zbndev/tidemark/issues
AppUpdatesURL=https://github.com/zbndev/tidemark/releases
DefaultDirName={code:InstallDirectory}
DefaultGroupName=Tidemark
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.17134
OutputDir={#OutputDir}
OutputBaseFilename=Tidemark-v{#AppVersion}-setup
SetupIconFile=..\..\icons\tidemark.ico
UninstallDisplayIcon={app}\tidemark.exe
WizardStyle=modern dynamic windows11
DisableWelcomePage=yes
DisableDirPage=auto
DisableProgramGroupPage=yes
DisableReadyPage=yes
Compression=lzma2
SolidCompression=yes
CloseApplications=no
RestartApplications=no
Uninstallable=yes
UninstallFilesDir={app}
SetupLogging=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Files]
Source: "{#PayloadDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs
Source: "{#PayloadDir}\tidemark-maintenance.exe"; DestName: "tidemark-setup-helper.exe"; Flags: dontcopy
Source: "{#PayloadDir}\install-manifest.json"; DestName: "incoming-manifest.json"; Flags: dontcopy

[Icons]
Name: "{userprograms}\Tidemark\Tidemark"; Filename: "{app}\tidemark.exe"; WorkingDir: "{app}"; AppUserModelID: "{#AppId}"

[Registry]
Root: HKCU; Subkey: "Software\Classes\AppUserModelId\{#AppId}"; ValueType: string; ValueName: "DisplayName"; ValueData: "Tidemark"; Flags: uninsdeletekey

[Run]
Filename: "{app}\tidemark.exe"; Description: "Launch Tidemark"; Flags: nowait postinstall skipifsilent; Check: FreshInstallation

[UninstallDelete]
Type: files; Name: "{app}\install-manifest.json"

[Code]
const
  LegacyKey = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\{#AppId}';
  InnoKey = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\{#AppId}_is1';
  GateName = 'Local\io.github.zbndev.Tidemark.Maintenance';
var
  PreviousDir, PreviousVersion: String;
  Helper, Incoming, StateDir, ErrorFile: String;
  Gate: NativeInt;
  Transaction, Prepared, FilesStarted, Completed, CommitFailed, PreviousInno, PreviousNSIS, IntegrationPrepared, Archived, UninstallFailed: Boolean;

function CreateMutexW(Attributes: NativeInt; InitialOwner: Boolean; Name: String): NativeInt;
  external 'CreateMutexW@kernel32.dll stdcall';
function CloseHandle(Handle: NativeInt): Boolean;
  external 'CloseHandle@kernel32.dll stdcall';
function Win32LastError: LongWord;
  external 'GetLastError@kernel32.dll stdcall';

function Quote(Value: String): String;
begin
  Result := '"' + Value + '"';
end;

function VersionNumber(var Value: String): Integer;
var P: Integer; Part: String;
begin
  P := Pos('.', Value);
  if P = 0 then begin Part := Value; Value := ''; end
  else begin Part := Copy(Value, 1, P - 1); Delete(Value, 1, P); end;
  Result := StrToIntDef(Part, -1);
end;

function CompareVersion(Left, Right: String): Integer;
var I, A, B: Integer;
begin
  Result := 0;
  for I := 1 to 3 do begin
    A := VersionNumber(Left); B := VersionNumber(Right);
    if (A < 0) or (B < 0) then RaiseException('Invalid installed version; installation stopped.');
    if (Result = 0) and (A < B) then Result := -1;
    if (Result = 0) and (A > B) then Result := 1;
  end;
  if (Left <> '') or (Right <> '') then RaiseException('Invalid installed version; installation stopped.');
end;

procedure ReadPrevious;
begin
  PreviousInno := RegKeyExists(HKCU64, InnoKey);
  PreviousNSIS := not PreviousInno and
    (RegKeyExists(HKCU32, LegacyKey) or RegKeyExists(HKCU64, LegacyKey));
  if not RegQueryStringValue(HKCU64, InnoKey, 'InstallLocation', PreviousDir) then
    if not RegQueryStringValue(HKCU32, LegacyKey, 'InstallLocation', PreviousDir) then
      RegQueryStringValue(HKCU64, LegacyKey, 'InstallLocation', PreviousDir);
  if not RegQueryStringValue(HKCU64, InnoKey, 'DisplayVersion', PreviousVersion) then
    if not RegQueryStringValue(HKCU32, LegacyKey, 'DisplayVersion', PreviousVersion) then
      RegQueryStringValue(HKCU64, LegacyKey, 'DisplayVersion', PreviousVersion);
end;

function InitializeSetup: Boolean;
begin
  ReadPrevious;
  Result := True;
  if (PreviousVersion <> '') and (CompareVersion(PreviousVersion, '{#AppVersion}') > 0) then begin
    SuppressibleMsgBox('A newer Tidemark version is already installed. Downgrading is not supported.', mbError, MB_OK, IDOK);
    Result := False;
  end;
end;

function InstallDirectory(Param: String): String;
begin
  if PreviousDir <> '' then Result := PreviousDir
  else Result := ExpandConstant('{localappdata}\Programs\tidemark');
end;

// Inno numbers its uninstallers unins000, unins001, ... The legacy NSIS build
// ships uninstall.exe, which also matches a plain 'unins*.*' mask and is not ours.
function IsInnoUninstallerName(Name: String): Boolean;
var Stem: String; I: Integer;
begin
  Result := False;
  Stem := ChangeFileExt(Name, '');
  if (Length(Stem) < 6) or (CompareText(Copy(Stem, 1, 5), 'unins') <> 0) then Exit;
  for I := 6 to Length(Stem) do
    if (Stem[I] < '0') or (Stem[I] > '9') then Exit;
  Result := True;
end;

function FreshInstallation: Boolean;
begin
  Result := (PreviousDir = '') and Completed;
end;

function AcquireGate: Boolean;
begin
  if Gate <> 0 then begin Result := True; Exit; end;
  Gate := CreateMutexW(0, False, GateName);
  Result := (Gate <> 0) and (Win32LastError <> 183);
  if not Result and (Gate <> 0) then begin CloseHandle(Gate); Gate := 0; end;
end;

procedure ReleaseGate;
begin
  if Gate <> 0 then begin CloseHandle(Gate); Gate := 0; end;
end;

function HelperError: String;
var Text: AnsiString;
begin
  Result := 'The installation helper failed. See the setup log.';
  if LoadStringFromFile(ErrorFile, Text) then Result := UTF8Decode(Text);
end;

function CallHelper(Operation, Extra: String): Integer;
begin
  DeleteFile(ErrorFile);
  Result := 1;
  if not Exec(Helper, Quote(Operation) + ' ' + Quote(ExpandConstant('{app}')) + ' ' +
      Quote(StateDir) + ' ' + Quote(Incoming) + ' ' + Quote(ErrorFile) + ' ' + Extra,
      '', SW_HIDE, ewWaitUntilTerminated, Result) then Result := 1;
  if Result <> 0 then Log(Operation + ': ' + HelperError);
end;

procedure SetPaths;
begin
  StateDir := ExpandConstant('{app}') + '.install-state';
  ErrorFile := ExpandConstant('{tmp}\tidemark-helper-error.txt');
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
var Code: Integer; Extra: String; Found: TFindRec;
begin
  Result := '';
  if Prepared then Exit;
  if (PreviousDir <> '') and (CompareText(AddBackslash(ExpandFileName(ExpandConstant('{app}'))), AddBackslash(ExpandFileName(PreviousDir))) <> 0) then begin
    Result := 'An upgrade must use the existing Tidemark directory.'; Exit;
  end;
  if not AcquireGate then begin Result := 'Another Tidemark setup is running.'; Exit; end;
  // Reserve Inno's stable .000 filenames. Never adopt another application's log.
  if FindFirst(ExpandConstant('{app}\unins*.*'), Found) then begin
    try
      repeat
        if IsInnoUninstallerName(Found.Name) and (not PreviousInno or
            ((CompareText(Found.Name, 'unins000.exe') <> 0) and
             (CompareText(Found.Name, 'unins000.dat') <> 0))) then begin
          Result := 'The selected directory contains an unrelated uninstaller. Choose another directory.';
          Exit;
        end;
      until not FindNext(Found);
    finally FindClose(Found); end;
  end;
  SetPaths;
  ExtractTemporaryFile('tidemark-setup-helper.exe');
  ExtractTemporaryFile('incoming-manifest.json');
  Helper := ExpandConstant('{tmp}\tidemark-setup-helper.exe');
  Incoming := ExpandConstant('{tmp}\incoming-manifest.json');
  if not Transaction then begin
    Extra := '';
    if PreviousNSIS then Extra := '--legacy-directory';
    Code := CallHelper('prepare', Extra);
    if Code <> 0 then begin Result := HelperError; Exit; end;
    Transaction := True;
  end;
  if not IntegrationPrepared then begin
    Code := CallHelper('snapshot-integration', Quote(ExpandConstant('{userprograms}\Tidemark\Tidemark.lnk')));
    if Code <> 0 then begin Result := HelperError; Exit; end;
    IntegrationPrepared := True;
  end;
  Code := CallHelper('stop', '');
  if Code <> 0 then begin Result := HelperError; Exit; end;
  if PreviousNSIS then begin
    // NSIS owns its entire program directory. Archive it as one transaction,
    // install into an empty root, and stop carrying per-version runtime inventories.
    Archived := True;
    Code := CallHelper('archive', '');
    if Code <> 0 then begin Result := HelperError; Exit; end;
  end;
  Prepared := True;
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssInstall then FilesStarted := True;
  if CurStep = ssPostInstall then begin
    if CallHelper('commit', '') <> 0 then begin
      CommitFailed := True;
      SuppressibleMsgBox('Tidemark could not complete the upgrade. The previous files will be restored. ' + HelperError, mbError, MB_OK, IDOK);
      Exit;
    end;
    // Retire NSIS registration only once the new files and integrations succeeded.
    // Never invoke its recursive old uninstaller on the upgraded directory.
    RegDeleteKeyIncludingSubkeys(HKCU32, LegacyKey);
    RegDeleteKeyIncludingSubkeys(HKCU64, LegacyKey);
    Completed := True;
    // [Run] is processed after ssPostInstall. A checked fresh-launch action must
    // see the completed installation, rather than exit against its own setup gate.
    ReleaseGate;
  end;
end;

function GetCustomSetupExitCode: Integer;
begin
  if CommitFailed then Result := 20 else Result := 0;
end;

procedure DeinitializeSetup;
var Ready: Boolean;
begin
  Ready := True;
  if Transaction and not Completed then begin
    if FilesStarted or Archived then Ready := CallHelper('rollback', '') = 0
    else Ready := CallHelper('cancel', '') = 0;
    if not Ready then
      SuppressibleMsgBox('Recovery could not finish. Original program files are retained at ' + StateDir + '. ' + HelperError, mbError, MB_OK, IDOK);
  end;
  ReleaseGate;
  if Transaction and Ready then begin
    if CallHelper('resume', '') = 0 then begin
      if CallHelper('finalize', '') <> 0 then Log('Recovery backup retained: ' + HelperError);
    end else SuppressibleMsgBox('Program files are ready, but Tidemark could not restart. ' + HelperError, mbError, MB_OK, IDOK);
  end;
end;

function InitializeUninstall: Boolean;
var Code: Integer;
begin
  Result := False;
  if not AcquireGate then begin MsgBox('Another Tidemark setup is running.', mbError, MB_OK); Exit; end;
  SetPaths;
  // Copy the helper out of the directory the uninstaller is about to remove.
  Helper := ExpandConstant('{tmp}\tidemark-setup-helper.exe');
  Incoming := ExpandConstant('{tmp}\uninstall-manifest.json');
  if not CopyFile(ExpandConstant('{app}\tidemark-maintenance.exe'), Helper, False) then Exit;
  if not SaveStringToFile(Incoming, '[]', False) then Exit;
  Code := CallHelper('prepare-remove', Quote(ExpandConstant('{userprograms}\Tidemark\Tidemark.lnk')));
  if Code <> 0 then begin MsgBox(HelperError, mbError, MB_OK); Exit; end;
  Transaction := True;
  Result := True;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then begin
    // Inno asks for confirmation after InitializeUninstall. Mutate only after Yes.
    if CallHelper('stop', '') <> 0 then RaiseException(HelperError);
    FilesStarted := True;
  end;
  if CurUninstallStep = usPostUninstall then begin
    if CallHelper('remove-startup', '') <> 0 then begin
      UninstallFailed := True;
      RaiseException(HelperError);
    end;
  end;
  if CurUninstallStep = usDone then begin
    Completed := not UninstallFailed and (CallHelper('verify-remove', '') = 0);
    if not Completed then
      SuppressibleMsgBox('Some Tidemark program files could not be removed. Close processes holding them and rerun the uninstaller. ' + HelperError, mbError, MB_OK, IDOK);
  end;
end;

procedure DeinitializeUninstall;
begin
  if Transaction and not FilesStarted then CallHelper('cancel', '');
  ReleaseGate;
  if Transaction and not FilesStarted then CallHelper('resume', '');
  // Once file deletion starts an uninstaller is not an upgrade transaction.
  // Keep a failed deletion's state; successful uninstall deliberately stays stopped.
  if Transaction and (Completed or not FilesStarted) then CallHelper('finalize-remove', '');
end;
