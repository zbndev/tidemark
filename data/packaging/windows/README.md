# Windows packaging

Tidemark uses a per-user Inno Setup 7 installer and native MSVC binaries. The
default location is `%LOCALAPPDATA%\Programs\tidemark`; installation and removal
need no administrator rights. Slint draws through wgpu. SQLite and the C runtime
are linked statically on Windows; no MSYS2, MinGW, GTK or redistributable DLLs are
shipped. Linux continues to use its system SQLite.

## Build

Install Rust and Visual Studio Build Tools (or Visual Studio) with **Desktop
development with C++**, the x64 MSVC tools, and a Windows SDK. Use PowerShell 7:

```powershell
./scripts/build-windows.ps1
```

The script discovers Visual Studio through `vswhere`, imports its native x64
environment, and removes MSYS2 paths and inherited Cargo/pkg-config overrides.
The explicit `--target` keeps target-only static CRT flags off host build scripts
and procedural macros. For individual checks:

```powershell
. ./scripts/windows-env.ps1
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked --target x86_64-pc-windows-msvc -- -D warnings
cargo test --workspace --locked --target x86_64-pc-windows-msvc
```

Release binaries are under `target/x86_64-pc-windows-msvc/release`; the payload,
PE import report and setup are under `build/windows`. The output is
`build/windows/installer/Tidemark-v<version>-setup.exe`. `-SkipBuild` restages
existing binaries; `-SkipInstaller` builds and checks the payload only.

`install-inno.ps1` downloads the official Inno **7.1.0** compiler and verifies its
pinned SHA-256 before installing it inside `build/windows/tools`. An explicit
`-InnoCompiler` can select an existing compiler. `stage-runtime.ps1` rejects
unknown imports, dynamic CRT dependencies and staging through reparse points.
All three executables carry product/version resources and the multi-size Tidemark
ICO (16–256 px). The wizard uses `modern dynamic windows11` and follows the
Windows light/dark setting. The Start-menu shortcut carries the native AUMID.

## Updating and removal

Updates are downloaded and run manually. There is no in-app updater.

Setup recognizes both the old NSIS registration and the Inno registration, keeps
the existing install directory and rejects downgrades. It pauses the existing
daemon task and creates a durable backup beside the installation, at
`<install directory>.install-state`, before replacing program files. It remembers
the exact Run value, task enabled state, visible/hidden UI and whether the daemon
was running independently of the UI. Per-PID shutdown events request the daemon's
normal flush/exit before closing the client. A session maintenance gate blocks
respawning during replacement. Process control matches the installed image path
and current user/session; another session is an installation blocker.

After success, setup restores the startup preference and resumes only processes
that were running before. It waits for an independently running daemon to become
ready before starting the client. A fresh interactive install offers a launch
checkbox; silent fresh installs remain stopped. No startup preference is enabled
automatically. If copying or commit fails, setup restores program files, its
previous manifest, uninstall registration, AUMID and shortcut before resuming the
old version. A commit failure returns exit code 20 instead of success.

NSIS migration happens once, when the old NSIS registration exists and no Inno
registration exists. Setup stops only the matching installed processes; an old
version without cooperative shutdown support is closed automatically after a
short timeout. It moves the entire NSIS-owned program directory into the transaction
backup and installs a clean payload in the same location. It restores that directory
if installation fails and retires the old registration only after commit. No old
uninstaller is invoked and no catalog of GTK DLLs or per-version payloads is shipped.
Config, history and credentials live outside this program directory and survive.
This also handles direct jumps from older versions without intermediate installs.

After the transition, Inno registration and `install-manifest.json` identify the
installation. Subsequent updates use the normal owned-file transaction; unrelated
files remain in place. The opaque directory transaction contains no knowledge of
old GTK/MSYS2 package versions.

Uninstall confirmation happens before stopping processes. Removal deletes
Tidemark startup integration and its recorded payload, while leaving config,
history, credentials and unrelated program-directory files intact. Cancelling
before file deletion restores task/Run state and running processes. Deletion
failures retain recovery state and are reported; uninstall is not a program-file
rollback transaction after deletion starts.

## Interrupted setup recovery

Backups are discarded only after successful commit/rollback and process resume.
An interrupted transaction blocks another setup instead of guessing whether
its backup is obsolete. Close setup/uninstall and recover from PowerShell using
the helper from the newly built payload (outside the install directory):

```powershell
$install = Join-Path $env:LOCALAPPDATA 'Programs/tidemark'
$state = "$install.install-state"
$errorFile = Join-Path $env:TEMP 'tidemark-recovery-error.txt'
./build/windows/payload/tidemark-maintenance.exe recover $install $state unused $errorFile
if ($LASTEXITCODE -ne 0) { Get-Content -LiteralPath $errorFile; throw 'Recovery failed; keep the backup' }
```

Use the actual install directory if customized. Recovery restores the snapshot,
startup and previous process state, and retains the backup if any step fails.
It does not roll back a partially completed uninstall. Close locking processes
and use the same helper command with `finish-remove` in place of `recover` to
complete removal from the saved file list. Avoid deleting `.install-state` by hand: it may
be the only remaining copy of the previous program files or integration values.

## Checks

CI and release jobs run native format/Clippy/tests, PE staging checks, build the
pinned setup and exercise silent installs/upgrades/removal with headless fixture
processes in a clean CI user profile. The release asset goes to a draft release.
The winget files remain submission templates, using the `inno` type and switches.

```powershell
./data/packaging/windows/test-stage-runtime.ps1
./scripts/test-windows-installer.ps1 -CompileOnly
# Full setup tests: clean disposable Windows user / CI only
./scripts/test-windows-installer.ps1 -SetupPath ./build/windows/installer/Tidemark-v0.5.2-setup.exe
```

The full script refuses to run when Tidemark installation/startup/shell integration
already exists. `-CompileOnly` builds fixtures without launching setup or application
processes. Credential-store tests and live transport probes are unsuitable for
routine checks against a running personal installation.

Manual checks belong to the user: light/dark wizard at 100/150/200% DPI;
Explorer/Start/taskbar icons; fresh-install launch; NSIS migration with visible
and hidden UI; daemon-only monitoring and startup preference; cancellation;
removal while running; preservation of config/history.

Primary references: [Inno event ordering](https://jrsoftware.org/ishelp/topic_scriptevents.htm),
[wizard styles](https://jrsoftware.org/ishelp/topic_setup_wizardstyle.htm),
[setup command line](https://jrsoftware.org/ishelp/topic_setupcmdline.htm),
[Inno source](https://github.com/jrsoftware/issrc), and
[MSVC CRT selection](https://doc.rust-lang.org/reference/linkage.html#static-and-dynamic-c-runtimes).
