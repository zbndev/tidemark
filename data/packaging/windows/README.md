# Windows packaging (todo 21)

Per-user NSIS installer: installs `tidemark.exe` + `tidemarkd.exe` plus the
pinned MSYS2 UCRT64 runtime they are linked against to
`%LOCALAPPDATA%\Programs\tidemark`, creates a Start-menu shortcut carrying the
`System.AppUserModel.ID` property `io.github.zbndev.Tidemark` (the toast
identity todo 16 requires), and registers a per-user uninstaller. The
uninstaller removes the Scheduled Task `TidemarkDaemon` and the HKCU `Run`
value `Tidemark` (the todo-14 lifecycle artifacts), the AUMID key, the
shortcut, and every installed file. Nothing machine-wide, no elevation.

## Files

- `installer.nsi` — the NSIS 3 script (per-user, `RequestExecutionLevel user`).
- `stage-runtime.sh` — walks the full PE import closure of both release
  executables and assembles `build/nsis-staging/runtime/` with the UCRT64 DLLs
  found there, the provider marks under `share/icons/hicolor` and
  `share/tidemark.ico`. The client draws with Slint and imports only system
  DLLs, with Rubik compiled in; what is staged is the daemon's SQLite and its
  closure. An upgrade from the GTK client removes the GTK runtime it left.
- `msys2-runtime-packages.txt` — exact package versions and package-archive
  SHA-256 hashes for every staged DLL/data owner. The release workflow
  downloads and verifies these archives before it builds, so linked and
  shipped DLL names cannot drift apart.
- `winget/` — winget manifest submission template (manifest only; submitting
  to winget-pkgs is the user's call).

## Local build

```sh
cargo build --release -p tidemark -p tidemarkd
data/packaging/windows/stage-runtime.sh build
cd data/packaging/windows
makensis /DSRC_DIR=<abs path>/target/release /DRUNTIME_DIR=<abs path>/build/nsis-staging/runtime \
         /DOUT_FILE=tidemark-installer.exe installer.nsi
```

Run this from an MSYS2 UCRT64 shell with the versions in
`msys2-runtime-packages.txt` installed.

## Release build

The installer is not built on every push — CI only tests this target. It is
built by the `windows` job in `.github/workflows/release.yml`, which runs on a
`v*` tag: it downloads those exact package archives, checks every SHA-256,
builds against them, runs the same import-closure staging script, and hands
makensis the version from the tag. The asset reaches the draft release as
`Tidemark-v<version>-setup.exe`, beside the `.deb` and the `.rpm`.
