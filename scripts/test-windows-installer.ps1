[CmdletBinding()]
param(
    [string] $SetupPath,
    [string] $HelperPath = "$PSScriptRoot/../build/windows/payload/tidemark-maintenance.exe",
    [string] $InnoCompiler = "$PSScriptRoot/../build/windows/tools/inno-7.1.0/ISCC.exe",
    [switch] $CompileOnly
)
$ErrorActionPreference = 'Stop'
$repoRoot = (Resolve-Path "$PSScriptRoot/..").Path
$testBuild = Join-Path $repoRoot 'build/windows/installer-tests'
$appId = 'io.github.zbndev.Tidemark'
$uninstallBase = 'Software\Microsoft\Windows\CurrentVersion\Uninstall'

function Assert-CleanTestUser {
    foreach ($view in @([Microsoft.Win32.RegistryView]::Registry32, [Microsoft.Win32.RegistryView]::Registry64)) {
        $hive = [Microsoft.Win32.RegistryKey]::OpenBaseKey([Microsoft.Win32.RegistryHive]::CurrentUser, $view)
        try {
            foreach ($name in @($appId, "${appId}_is1")) {
                $key = $hive.OpenSubKey("$uninstallBase\$name")
                if ($key) { $key.Dispose(); throw 'Installer tests require a clean Windows user: Tidemark uninstall registration exists. Use -CompileOnly locally.' }
            }
            $run = $hive.OpenSubKey('Software\Microsoft\Windows\CurrentVersion\Run')
            if ($run) {
                try { if ($run.GetValue('Tidemark')) { throw 'Tidemark startup registration exists. Use -CompileOnly locally.' } } finally { $run.Dispose() }
            }
            $aumid = $hive.OpenSubKey("Software\Classes\AppUserModelId\$appId")
            if ($aumid) { $aumid.Dispose(); throw 'Tidemark AppUserModelId registration exists. Use -CompileOnly locally.' }
        } finally { $hive.Dispose() }
    }
    $scheduler = New-Object -ComObject 'Schedule.Service'
    $scheduler.Connect()
    if (@($scheduler.GetFolder('\').GetTasks(1) | Where-Object { $_.Name -eq 'TidemarkDaemon' }).Count) {
        throw 'TidemarkDaemon scheduled task exists. Use -CompileOnly locally.'
    }
    $programs = [Environment]::GetFolderPath('Programs')
    if (Test-Path -LiteralPath (Join-Path $programs 'Tidemark')) { throw 'Tidemark Start menu folder exists. Use -CompileOnly locally.' }
}

function Invoke-CheckedTool([string] $Path, [string[]] $ToolArguments) {
    & $Path @ToolArguments
    if ($LASTEXITCODE -ne 0) { throw "$Path failed with exit code $LASTEXITCODE" }
}

function Write-FixturePayload([string] $Path, [string] $Version, [string] $Fixture) {
    New-Item -ItemType Directory -Path $Path -Force | Out-Null
    foreach ($name in @('tidemark.exe', 'tidemarkd.exe')) { Copy-Item -LiteralPath $Fixture -Destination (Join-Path $Path $name) -Force }
    Copy-Item -LiteralPath $HelperPath -Destination "$Path/tidemark-maintenance.exe" -Force
    Set-Content -LiteralPath "$Path/version.txt" -Value $Version -NoNewline -Encoding utf8NoBOM
    ConvertTo-Json -InputObject @('tidemark.exe', 'tidemarkd.exe', 'tidemark-maintenance.exe', 'version.txt') | Set-Content -LiteralPath "$Path/install-manifest.json" -Encoding utf8NoBOM
}

# Full tests mutate only a clean CI user's disposable installation. The guard runs
# before compilation, any setup process, registry write, or task mutation.
if (-not $CompileOnly) { Assert-CleanTestUser }
if (-not $CompileOnly -and -not $SetupPath) { throw 'Full tests require -SetupPath for the release artifact.' }
if ($SetupPath -and -not (Test-Path -LiteralPath $SetupPath -PathType Leaf)) { throw "Setup artifact missing: $SetupPath" }
if (-not (Test-Path -LiteralPath $HelperPath -PathType Leaf)) { throw "Actual maintenance helper missing: $HelperPath" }
if (-not (Test-Path -LiteralPath $InnoCompiler -PathType Leaf)) { throw "Inno compiler missing: $InnoCompiler" }
. "$PSScriptRoot/windows-env.ps1"
New-Item -ItemType Directory -Path $testBuild -Force | Out-Null
$fixture = Join-Path $testBuild 'fixture.exe'
$legacy = Join-Path $testBuild 'fixture-legacy.exe'
Invoke-CheckedTool rustc.exe @('--edition', '2024', '--target', 'x86_64-pc-windows-msvc', '-C', 'target-feature=+crt-static', '-O', '-o', $fixture, "$PSScriptRoot/windows-installer-fixture.rs")
Invoke-CheckedTool rustc.exe @('--edition', '2024', '--target', 'x86_64-pc-windows-msvc', '-C', 'target-feature=+crt-static', '-O', '--cfg', 'fixture_legacy', '-o', $legacy, "$PSScriptRoot/windows-installer-fixture.rs")
foreach ($version in @('1.0.100', '1.0.101')) {
    $payload = Join-Path $testBuild "payload-$version"
    Write-FixturePayload $payload $version $fixture
    Invoke-CheckedTool $InnoCompiler @('/Q', "/DPayloadDir=$payload", "/DAppVersion=$version", "/DOutputDir=$testBuild", "$repoRoot/data/packaging/windows/installer.iss")
}
if ($CompileOnly) { Write-Host 'Installer fixtures compiled. No setup or fixture processes were launched.'; return }

$testRoot = Join-Path ([IO.Path]::GetTempPath()) "Tidemark.InstallerTests.$([Guid]::NewGuid().ToString('N'))"
$install = Join-Path $testRoot 'app'
$log = Join-Path $testRoot 'fixture.log'
$previousLog = $env:TIDEMARK_FIXTURE_LOG
$previousSpawn = $env:TIDEMARK_FIXTURE_SPAWN_DAEMON
$previousDelay = $env:TIDEMARK_FIXTURE_READY_DELAY_MS
$env:TIDEMARK_FIXTURE_LOG = $log
$env:TIDEMARK_FIXTURE_SPAWN_DAEMON = '1'
New-Item -ItemType Directory -Path $testRoot -Force | Out-Null
$shadowProcess = $null

function Assert-That([bool] $Condition, [string] $Message) { if (-not $Condition) { throw $Message } }
function Wait-Until([scriptblock] $Condition, [string] $Message, [int] $Seconds = 20) {
    $deadline = [DateTime]::UtcNow.AddSeconds($Seconds)
    do { if (& $Condition) { return }; Start-Sleep -Milliseconds 100 } while ([DateTime]::UtcNow -lt $deadline)
    throw "Timed out: $Message"
}
function Read-Log { if (Test-Path -LiteralPath $log) { @(Get-Content -LiteralPath $log) } else { @() } }
function Installed-Processes {
    @(Get-CimInstance Win32_Process -Filter "Name='tidemark.exe' OR Name='tidemarkd.exe'" | Where-Object { $_.ExecutablePath -and [IO.Path]::GetFullPath($_.ExecutablePath).StartsWith($install + '\', [StringComparison]::OrdinalIgnoreCase) })
}
function Run-Setup([string] $Version, [bool] $Success = $true, [string[]] $Extra = @(), [string] $ArtifactPath) {
    $path = if ($ArtifactPath) { $ArtifactPath } else { Join-Path $testBuild "Tidemark-v$Version-setup.exe" }
    $log = Join-Path $testRoot "setup-$Version-$([Guid]::NewGuid().ToString('N')).log"
    $arguments = @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/SP-', "/DIR=`"$install`"", "/LOG=`"$log`"") + $Extra
    $process = Start-Process -FilePath $path -ArgumentList $arguments -WindowStyle Hidden -PassThru
    if (-not $process.WaitForExit(120000)) { $process.Kill(); throw 'Setup exceeded 120 seconds' }
    if (($process.ExitCode -eq 0) -ne $Success) {
        if (Test-Path -LiteralPath $log) { Get-Content -LiteralPath $log -Tail 40 | ForEach-Object { Write-Host "  setup: $_" } }
        throw "Setup $Version exit=$($process.ExitCode), expected success=$Success. Logs: $testRoot"
    }
}
function Uninstall-Fixture {
    $path = Join-Path $install 'unins000.exe'
    $process = Start-Process -FilePath $path -ArgumentList @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', "/LOG=`"$testRoot/uninstall.log`"") -WindowStyle Hidden -PassThru
    if (-not $process.WaitForExit(120000)) { $process.Kill(); throw 'Fixture uninstall exceeded 120 seconds' }
    Assert-That ($process.ExitCode -eq 0) "Uninstall failed: $($process.ExitCode)"
    # The uninstaller relaunches a temporary copy that removes unins000.* only after
    # this process exits; a reinstall scanning the directory must not race it.
    Wait-Until { @(Get-ChildItem -LiteralPath $install -Filter 'unins*' -File -ErrorAction SilentlyContinue).Count -eq 0 } 'uninstaller removed its own files'
    Wait-Until { @(Installed-Processes).Count -eq 0 } 'fixture processes exit after uninstall'
    foreach ($name in @($appId, "${appId}_is1")) {
        $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey("$uninstallBase\$name")
        try { Assert-That (-not $key) 'Uninstall registration survived fixture removal' } finally { if ($key) { $key.Dispose() } }
    }
    $run = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Software\Microsoft\Windows\CurrentVersion\Run')
    if ($run) { try { Assert-That (-not $run.GetValue('Tidemark')) 'Fixture startup Run value survived uninstall' } finally { $run.Dispose() } }
    $scheduler = New-Object -ComObject 'Schedule.Service'
    $scheduler.Connect()
    Assert-That (@($scheduler.GetFolder('\').GetTasks(1) | Where-Object { $_.Name -eq 'TidemarkDaemon' }).Count -eq 0) 'Fixture daemon task survived uninstall'
}
function Start-Fixture([string] $Binary, [string[]] $Arguments = @()) {
    $options = @{ FilePath = (Join-Path $install $Binary); WindowStyle = 'Hidden'; PassThru = $true }
    if ($Arguments.Count) { $options.ArgumentList = $Arguments }
    return Start-Process @options
}
function Assert-NoOwnedProcess {
    Assert-That (@(Installed-Processes).Count -eq 0) 'Setup started a previously stopped fixture'
}
function Assert-HiddenPair {
    Wait-Until { @(Installed-Processes).Count -eq 2 } 'one hidden client and its daemon'
    $current = @(Installed-Processes)
    $ui = @($current | Where-Object { $_.Name -eq 'tidemark.exe' })
    $daemon = @($current | Where-Object { $_.Name -eq 'tidemarkd.exe' })
    Assert-That ($ui.Count -eq 1 -and $daemon.Count -eq 1) 'Daemon/client duplication after restart'
    Assert-That ($daemon[0].ParentProcessId -eq $ui[0].ProcessId) 'Client-owned daemon became an independent daemon'
    Assert-That (($ui[0].CommandLine) -like '*--background*') 'Hidden client was restored in foreground mode'
    return $current
}

function Set-FixtureStartup([bool] $Enabled = $false) {
    $run = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Software\Microsoft\Windows\CurrentVersion\Run')
    try { $run.SetValue('Tidemark', "`"$install\tidemark.exe`" --background --fixture-preserved") } finally { $run.Dispose() }
    $scheduler = New-Object -ComObject 'Schedule.Service'
    $scheduler.Connect()
    $definition = $scheduler.NewTask(0)
    $definition.RegistrationInfo.Description = 'Disposable headless Tidemark installer test'
    $definition.Principal.LogonType = 3 # TASK_LOGON_INTERACTIVE_TOKEN
    $definition.Principal.UserId = "$env:USERDOMAIN\$env:USERNAME"
    $definition.Settings.Enabled = $Enabled
    $action = $definition.Actions.Create(0)
    $action.Path = Join-Path $install 'tidemarkd.exe'
    # No triggers: the fixture never runs automatically, even when enabled.
    $scheduler.GetFolder('\').RegisterTaskDefinition('TidemarkDaemon', $definition, 6, $null, $null, 3, $null) | Out-Null
    $script:fixtureStartupXml = $scheduler.GetFolder('\').GetTask('TidemarkDaemon').Xml
}
function Assert-FixtureStartup([bool] $Enabled = $false) {
    $run = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Software\Microsoft\Windows\CurrentVersion\Run')
    try { Assert-That ($run.GetValue('Tidemark') -ceq "`"$install\tidemark.exe`" --background --fixture-preserved") 'Upgrade changed exact startup arguments' } finally { $run.Dispose() }
    $scheduler = New-Object -ComObject 'Schedule.Service'
    $scheduler.Connect()
    $task = $scheduler.GetFolder('\').GetTask('TidemarkDaemon')
    Assert-That ($task.Enabled -eq $Enabled) 'Upgrade changed the prior scheduled task enabled state'
    Assert-That ($task.Definition.Actions.Item(1).Path -ieq (Join-Path $install 'tidemarkd.exe')) 'Upgrade changed the daemon scheduled task path'
    Assert-That ($task.Xml -ceq $script:fixtureStartupXml) 'Upgrade changed the scheduled task definition'
}

function Assert-ExecutableResources([string] $Path) {
    if (-not ('TidemarkInstallerResourceCheck' -as [type])) {
        Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
public static class TidemarkInstallerResourceCheck {
    [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    private static extern IntPtr LoadLibraryExW(string path, IntPtr file, uint flags);
    [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    private static extern IntPtr FindResourceW(IntPtr module, IntPtr name, IntPtr type);
    [DllImport("kernel32.dll")]
    private static extern bool FreeLibrary(IntPtr module);
    public static bool HasResource(string path, int type) {
        // LOAD_LIBRARY_AS_DATAFILE maps resources without executing entry points
        // or resolving the executable's imports. No application is launched.
        var module = LoadLibraryExW(path, IntPtr.Zero, 2);
        if (module == IntPtr.Zero) throw new Win32Exception(Marshal.GetLastWin32Error());
        try { return FindResourceW(module, new IntPtr(1), new IntPtr(type)) != IntPtr.Zero; }
        finally { FreeLibrary(module); }
    }
}
'@
    }
    Assert-That ([TidemarkInstallerResourceCheck]::HasResource($Path, 14)) "Missing embedded application icon: $Path"
    Assert-That ([TidemarkInstallerResourceCheck]::HasResource($Path, 16)) "Missing embedded version resource: $Path"
}

function Assert-ReleasePayload([string] $Version) {
    $payload = Join-Path $repoRoot 'build/windows/payload'
    $expected = @(Get-Content -LiteralPath "$payload/install-manifest.json" -Raw | ConvertFrom-Json)
    $actual = @(Get-Content -LiteralPath "$install/install-manifest.json" -Raw | ConvertFrom-Json)
    $complete = @($expected) + @('unins000.exe', 'unins000.dat')
    Assert-That (@(Compare-Object ($complete | Sort-Object) ($actual | Sort-Object)).Count -eq 0) 'Installed release manifest differs from the full staged payload and stable uninstall files'
    foreach ($relative in $actual) {
        $path = [IO.Path]::GetFullPath((Join-Path $install $relative))
        Assert-That ($path.StartsWith($install + '\', [StringComparison]::OrdinalIgnoreCase)) 'Release manifest escapes the test installation'
        Assert-That (Test-Path -LiteralPath $path -PathType Leaf) "Release manifest file is absent: $relative"
    }
    foreach ($relative in $expected) {
        Assert-That ((Get-FileHash -LiteralPath (Join-Path $install $relative)).Hash -eq (Get-FileHash -LiteralPath (Join-Path $payload $relative)).Hash) "Release artifact changed or omitted payload content: $relative"
    }
    . "$repoRoot/data/packaging/windows/stage-runtime.ps1" -FunctionsOnly
    foreach ($name in @('tidemark.exe', 'tidemarkd.exe', 'tidemark-maintenance.exe')) {
        $path = Join-Path $install $name
        $metadata = [Diagnostics.FileVersionInfo]::GetVersionInfo($path)
        Assert-That ($metadata.ProductName -ceq 'Tidemark' -and $metadata.ProductVersion -ceq $Version) "Incorrect installed product version resource: $name"
        Assert-ExecutableResources $path
        $imports = & dumpbin.exe /NOLOGO /DEPENDENTS $path 2>&1 | Out-String
        Assert-That ($LASTEXITCODE -eq 0) "Cannot inspect installed release PE imports: $name"
        Assert-SystemImports -Binary $path -Imports (Get-PeImportsFromText $imports)
    }
    $registration = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey("$uninstallBase\${appId}_is1")
    try {
        Assert-That ($null -ne $registration) 'Release setup omitted its uninstall registration'
        Assert-That ($registration.GetValue('DisplayVersion') -ceq $Version) 'Release uninstall registration has the wrong version'
        $registeredPath = [IO.Path]::GetFullPath($registration.GetValue('InstallLocation')).TrimEnd('\')
        Assert-That ($registeredPath -ieq $install.TrimEnd('\')) 'Release setup used a directory outside the isolated test installation'
    } finally { if ($registration) { $registration.Dispose() } }
    return $actual
}

try {
    Run-Setup '1.0.100'
    Assert-That ((Get-Content "$install/version.txt" -Raw) -eq '1.0.100') 'Fresh install payload missing'
    Assert-NoOwnedProcess
    Set-Content -LiteralPath "$install/unowned.txt" -Value 'preserve me' -NoNewline
    $savedManifest = [IO.File]::ReadAllBytes("$install/install-manifest.json")
    $savedExecutableHash = (Get-FileHash "$install/tidemark.exe").Hash
    try {
        [IO.File]::WriteAllText("$install/install-manifest.json", '["../unowned.txt"]')
        Run-Setup '1.0.101' $false
        Assert-That ((Get-FileHash "$install/tidemark.exe").Hash -eq $savedExecutableHash) 'Invalid-manifest failure changed installed binaries'
        Assert-That ((Get-Content "$install/version.txt" -Raw) -eq '1.0.100') 'Invalid-manifest failure changed installed version'
    } finally { [IO.File]::WriteAllBytes("$install/install-manifest.json", $savedManifest) }
    Run-Setup '1.0.101'
    Assert-NoOwnedProcess
    Assert-That ((Get-Content "$install/unowned.txt" -Raw) -eq 'preserve me') 'Upgrade deleted an unowned file'
    Run-Setup '1.0.100' $false
    Assert-That ((Get-Content "$install/version.txt" -Raw) -eq '1.0.101') 'Downgrade modified installed files'
    Uninstall-Fixture
    Assert-That (Test-Path -LiteralPath "$install/unowned.txt") 'Uninstall removed an unowned file'
    Write-Host 'Fresh install, failed preparation, stopped upgrade, downgrade guard and unowned-file preservation passed.'

    Run-Setup '1.0.100'
    Set-FixtureStartup
    # The append-only fixture log already holds earlier phases' ready lines; only
    # this pair's fresh readiness proves both stop listeners exist.
    $readyBefore = @(Read-Log | Where-Object { $_ -like '*|ready|*' }).Count
    $client = Start-Fixture 'tidemark.exe' @('--background')
    $old = @(Assert-HiddenPair)
    Wait-Until { @(Read-Log | Where-Object { $_ -like '*|ready|*' }).Count -ge $readyBefore + 2 } 'cooperative stop event readiness'
    # Same executable basename outside the selected install is never signalled.
    $shadowDirectory = Join-Path $testRoot 'other-app'
    New-Item -ItemType Directory -Path $shadowDirectory | Out-Null
    Copy-Item -LiteralPath $fixture -Destination "$shadowDirectory/tidemarkd.exe"
    $shadowProcess = Start-Process -FilePath "$shadowDirectory/tidemarkd.exe" -WindowStyle Hidden -PassThru
    Run-Setup '1.0.101'
    Assert-FixtureStartup
    Assert-That (-not $shadowProcess.HasExited) 'Upgrade signalled an unrelated fixture with the same basename'
    $new = @(Assert-HiddenPair)
    Assert-That (@($old.ProcessId | Where-Object { $_ -in $new.ProcessId }).Count -eq 0) 'Old processes survived an upgrade'
    $records = @(Read-Log)
    $daemonStop = @($records | Where-Object { $_ -like "*|$($old.Where({ $_.Name -eq 'tidemarkd.exe' })[0].ProcessId)|daemon|stopped|*" })
    $clientStop = @($records | Where-Object { $_ -like "*|$($client.Id)|client|stopped|*" })
    Assert-That ($daemonStop.Count -eq 1 -and $clientStop.Count -eq 1) 'Cooperative stop was not observed for both fixture processes'
    Assert-That ([long]($daemonStop[0].Split('|')[0]) -le [long]($clientStop[0].Split('|')[0])) 'Client stopped before daemon flushed'
    Set-FixtureStartup $true
    Run-Setup '1.0.101'
    Assert-FixtureStartup $true
    $null = Assert-HiddenPair
    Uninstall-Fixture
    Assert-That (-not $shadowProcess.HasExited) 'Uninstall stopped an unrelated fixture with the same basename'
    $shadowProcess.Kill()
    $shadowProcess.WaitForExit()
    $shadowProcess = $null
    Assert-That (@(Read-Log | Where-Object { $_ -like '*|stopped|*' }).Count -ge 4) 'Running uninstall did not stop both processes cooperatively'
    Write-Host 'Running upgrade preserved hidden client ownership; running uninstall stopped daemon before client.'

    Run-Setup '1.0.100'
    $env:TIDEMARK_FIXTURE_READY_DELAY_MS = '8000'
    $delayed = Start-Fixture 'tidemarkd.exe'
    Wait-Until { @(Read-Log | Where-Object { $_ -like "*|$($delayed.Id)|daemon|started|*" }).Count -eq 1 } 'daemon starts before event readiness'
    $independentClient = Start-Fixture 'tidemark.exe' @('--background')
    Wait-Until { @(Read-Log | Where-Object { $_ -like "*|$($independentClient.Id)|client|ready|*" }).Count -eq 1 } 'hidden client starts beside independent daemon'
    Run-Setup '1.0.101'
    Wait-Until { $delayed.HasExited } 'late-ready daemon receives pending stop request'
    Wait-Until { @(Installed-Processes).Count -eq 2 } 'independent daemon and hidden client restored after delayed readiness'
    $independentProcesses = @(Installed-Processes)
    $independentDaemon = @($independentProcesses | Where-Object { $_.Name -eq 'tidemarkd.exe' })
    $restoredClient = @($independentProcesses | Where-Object { $_.Name -eq 'tidemark.exe' })
    Assert-That ($independentDaemon.Count -eq 1 -and $restoredClient.Count -eq 1) 'Independent daemon duplicated when client resumed'
    Assert-That ($independentDaemon[0].ParentProcessId -ne $restoredClient[0].ProcessId) 'Independent daemon became client-owned'
    Assert-That ($restoredClient[0].CommandLine -like '*--background*') 'Independent-daemon upgrade changed hidden client mode'
    $env:TIDEMARK_FIXTURE_READY_DELAY_MS = $null
    Uninstall-Fixture
    Write-Host 'A stop request issued before event readiness survives until the daemon listener starts.'

    # NSIS owned its whole registered program directory. Migration is automatic
    # for older versions, without a DLL inventory or a user-operated uninstaller.
    $userData = Join-Path $testRoot 'user-data'
    New-Item -ItemType Directory -Path $userData -Force | Out-Null
    Set-Content -LiteralPath "$userData/config.toml" -Value 'sentinel = "preserve user preferences"' -NoNewline
    [IO.File]::WriteAllBytes("$userData/history.sqlite", [byte[]](0..63))
    $configHash = (Get-FileHash "$userData/config.toml").Hash
    $historyHash = (Get-FileHash "$userData/history.sqlite").Hash
    foreach ($legacyVersion in @('0.1.0', '0.4.0')) {
        Write-FixturePayload $install $legacyVersion $legacy
        Remove-Item -LiteralPath "$install/install-manifest.json"
        New-Item -ItemType Directory -Path "$install/opaque-vendor-runtime/deep" -Force | Out-Null
        Set-Content -LiteralPath "$install/opaque-vendor-runtime/deep/unknown-runtime.dll" -Value 'old runtime outside every former DLL allowlist' -NoNewline
        Set-Content -LiteralPath "$install/old-nsis-owned.txt" -Value 'NSIS owned this entire directory' -NoNewline
        $key = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey("$uninstallBase\$appId")
        try { $key.SetValue('InstallLocation', $install); $key.SetValue('DisplayVersion', $legacyVersion) } finally { $key.Dispose() }
        $legacyProcess = Start-Fixture 'tidemarkd.exe'
        Wait-Until { @(Read-Log | Where-Object { $_ -like "*|$($legacyProcess.Id)|daemon|ready|*" }).Count -eq 1 } 'legacy fixture readiness'
        $shadowProcess = Start-Process -FilePath "$shadowDirectory/tidemarkd.exe" -WindowStyle Hidden -PassThru
        Wait-Until { @(Read-Log | Where-Object { $_ -like "*|$($shadowProcess.Id)|daemon|ready|*" }).Count -eq 1 } 'unrelated daemon fixture readiness'
        Run-Setup '1.0.101'
        Wait-Until { $legacyProcess.HasExited } 'automatic stop of the matched legacy process'
        Assert-That (-not $shadowProcess.HasExited) 'Automatic NSIS migration force-closed an unrelated process with the same basename'
        Wait-Until { @(Installed-Processes | Where-Object { $_.Name -eq 'tidemarkd.exe' }).Count -eq 1 } 'independent daemon restored after automatic NSIS migration'
        Assert-That ((Get-Content "$install/version.txt" -Raw) -eq '1.0.101') 'NSIS migration did not install the new payload'
        Assert-That (-not (Test-Path -LiteralPath "$install/opaque-vendor-runtime")) 'NSIS migration retained an arbitrary old runtime directory'
        Assert-That (-not (Test-Path -LiteralPath "$install/old-nsis-owned.txt")) 'NSIS migration retained an old program file'
        Assert-That ((Get-FileHash "$userData/config.toml").Hash -eq $configHash) 'NSIS migration changed user preferences outside the program directory'
        Assert-That ((Get-FileHash "$userData/history.sqlite").Hash -eq $historyHash) 'NSIS migration changed user history outside the program directory'
        $legacyKey = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey("$uninstallBase\$appId")
        try { Assert-That (-not $legacyKey) 'Legacy NSIS registration survived successful migration' } finally { if ($legacyKey) { $legacyKey.Dispose() } }
        Uninstall-Fixture
        Assert-That (-not $shadowProcess.HasExited) 'Migrated uninstall stopped an unrelated process'
        $shadowProcess.Kill()
        $shadowProcess.WaitForExit()
        $shadowProcess = $null
        Assert-That ((Get-FileHash "$userData/config.toml").Hash -eq $configHash -and (Get-FileHash "$userData/history.sqlite").Hash -eq $historyHash) 'Uninstall changed user data outside the program directory'
        Write-Host "NSIS $legacyVersion migrated automatically, cleaned its entire old program directory and preserved user data."
    }

    # Exercise the uploaded release executable itself after the fixture scenarios.
    # Silent fresh installation skips [Run]; only Setup, its helper and Uninstall
    # execute. The real client and daemon are checked as files and never launched.
    $install = Join-Path $testRoot 'real-app'
    $manifest = Get-Content -LiteralPath "$repoRoot/Cargo.toml" -Raw
    $releaseVersion = [regex]::Match($manifest, '(?ms)^\[workspace\.package\].*?^version = "([^"]+)"').Groups[1].Value
    Run-Setup $releaseVersion -ArtifactPath ([IO.Path]::GetFullPath($SetupPath))
    Assert-NoOwnedProcess
    $releaseFiles = @(Assert-ReleasePayload $releaseVersion)
    Assert-That ((Get-FileHash "$userData/config.toml").Hash -eq $configHash -and (Get-FileHash "$userData/history.sqlite").Hash -eq $historyHash) 'Release setup changed user-data sentinels outside the program directory'
    Uninstall-Fixture
    Wait-Until { @($releaseFiles + @('install-manifest.json') | Where-Object { Test-Path -LiteralPath (Join-Path $install $_) }).Count -eq 0 } 'release payload and uninstaller self-deletion'
    foreach ($relative in $releaseFiles + @('install-manifest.json')) {
        Assert-That (-not (Test-Path -LiteralPath (Join-Path $install $relative))) "Release uninstall retained an owned payload file: $relative"
    }
    $aumid = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey("Software\Classes\AppUserModelId\$appId")
    try { Assert-That (-not $aumid) 'Release uninstall retained the AppUserModelId registration' } finally { if ($aumid) { $aumid.Dispose() } }
    Assert-That (-not (Test-Path -LiteralPath (Join-Path ([Environment]::GetFolderPath('Programs')) 'Tidemark/Tidemark.lnk'))) 'Release uninstall retained its Start menu shortcut'
    Assert-That ((Get-FileHash "$userData/config.toml").Hash -eq $configHash -and (Get-FileHash "$userData/history.sqlite").Hash -eq $historyHash) 'Release uninstall changed user-data sentinels outside the program directory'
    Write-Host 'The release setup installed its complete payload and resources, launched no app processes, and uninstalled its files and registration.'
    Write-Host 'All native headless installer tests passed.'
} finally {
    # Kill only exact fixture image paths, never by basename or a production PID.
    foreach ($process in @(Installed-Processes)) { Stop-Process -Id $process.ProcessId -Force -ErrorAction SilentlyContinue }
    if ($shadowProcess -and -not $shadowProcess.HasExited) { $shadowProcess.Kill(); $shadowProcess.WaitForExit() }
    $env:TIDEMARK_FIXTURE_LOG = $previousLog
    $env:TIDEMARK_FIXTURE_SPAWN_DAEMON = $previousSpawn
    $env:TIDEMARK_FIXTURE_READY_DELAY_MS = $previousDelay
    Write-Host "Installer test logs retained: $testRoot"
}
