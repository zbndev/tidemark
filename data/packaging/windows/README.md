# Windows packaging (todo 21)

Per-user NSIS installer: installs `tidemark.exe` + `tidemarkd.exe` plus the
pinned gvsbuild GTK 4 / libadwaita runtime to
`%LOCALAPPDATA%\Programs\tidemark`, creates a Start-menu shortcut carrying the
`System.AppUserModel.ID` property `io.github.zbndev.Tidemark` (the toast
identity todo 16 requires), and registers a per-user uninstaller. The
uninstaller removes the Scheduled Task `TidemarkDaemon` and the HKCU `Run`
value `Tidemark` (the todo-14 lifecycle artifacts), the AUMID key, the
shortcut, and every installed file. Nothing machine-wide, no elevation.

## Files

- `installer.nsi` — the NSIS 3 script (per-user, `RequestExecutionLevel user`).
- `stage-gtk-runtime.sh` — assembles `build/nsis-staging/gtk/` from the pinned
  gvsbuild bundle (`2026.8.0`, same pin as the CI probe jobs): runtime DLLs,
  the gdk-pixbuf SVG loader with a relative-path `loaders.cache`, glib
  schemas, icon themes.
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

The pinned `GVSBUILD_URL` in `stage-gtk-runtime.sh` must match the
`windows-tests`/probe CI jobs; the CI `nsis-package` job performs these same
three steps and uploads the installer as its `nsis-package` artifact.
