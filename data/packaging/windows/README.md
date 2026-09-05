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
- `stage-gtk-runtime.sh` — walks the full PE import closure of both release
  executables and all MSYS2 gdk-pixbuf loaders, then assembles
  `build/nsis-staging/gtk/` with those UCRT64 DLLs, a relative-path
  `loaders.cache`, compiled GLib schemas, fontconfig data and icon themes.
- `msys2-runtime-packages.txt` — exact package versions and package-archive
  SHA-256 hashes for every staged DLL/data owner. CI downloads and verifies
  these archives before the release build, so linked and shipped DLL names
  cannot drift apart.
- `winget/` — winget manifest submission template (manifest only; submitting
  to winget-pkgs is the user's call).

## Local build

```sh
cargo build --release -p tidemark -p tidemarkd
data/packaging/windows/stage-gtk-runtime.sh build
cd data/packaging/windows
makensis /DSRC_DIR=<abs path>/target/release /DGTK_DIR=<abs path>/build/nsis-staging/gtk \
         /DOUT_FILE=tidemark-installer.exe installer.nsi
```

Run this from an MSYS2 UCRT64 shell with the versions in
`msys2-runtime-packages.txt` installed. The CI `nsis-package` job downloads
those exact package archives, checks every SHA-256, builds against them, runs
the same import-closure staging script, and uploads the installer as its
`nsis-package` artifact.
