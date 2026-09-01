# Nix Flake and NixOS Module Design

- Status: approved for planning
- Date: 2026-09-01

## Purpose

Tidemark is installable from this repository by Nix users, without a separate package
repository or an upstream nixpkgs submission. The repository exports a reproducible Linux
package, runnable apps, a development shell, and a NixOS module. The module makes the
existing D-Bus-activated **user** daemon available; it does not start the GTK application
at login.

## Goals

- Commit `flake.nix` and `flake.lock`, pinning nixpkgs at a revision that meets Rust 1.92,
  GTK 4.22, and libadwaita 1.9.
- Export native `x86_64-linux` and `aarch64-linux` package outputs, apps for `tidemark` and
  `tidemarkd`, and a development shell with the documented C/C++ and toolkit dependencies.
- Package both binaries, icons, desktop metadata, AppStream metadata, and the session
  D-Bus activation file. Its `Exec` is rewritten to the immutable Nix-store daemon path.
- Export `nixosModules.default`; `services.tidemark.enable = true` installs the package,
  registers the D-Bus service, and defines `tidemarkd.service` in systemd user managers.
- Check the flake in Docker: evaluate the module, build and inspect the package, then start
  the daemon on a temporary session bus and call the public D-Bus interface.

## Non-goals

- A separate Nix repository, FlakeHub publication, or a nixpkgs pull request.
- Altering DEB, RPM, or `PKGBUILD` packaging.
- GUI autostart, a mutable `/etc/xdg/autostart` file, user selection, linger management, or
  root-side restart logic.
- Maintaining or manually configuring a NixOS VM. Docker is the acceptance environment.
- Running the Rust suite inside `buildRustPackage`; native CI already owns that suite.

## Package

`nix/package.nix` uses `rustPlatform.buildRustPackage` with
`cargoLock.lockFile = ../Cargo.lock`; the committed Cargo lock is therefore authoritative and
ordinary dependency bumps do not require a hand-maintained vendor hash. The derivation builds
the whole workspace in release mode, sets `doCheck = false`, uses `wrapGAppsHook4`, and has
the required build inputs: pkg-config, CMake, Clang, libclang, GTK4, libadwaita, SQLite, D-Bus
and the hicolor icon theme.

It installs `tidemark`, `tidemarkd`, `share/applications`, `share/metainfo`,
`share/icons/hicolor`, and `share/dbus-1/services`. It copies the existing D-Bus file then
substitutes `/usr/bin/tidemarkd` with `$out/bin/tidemarkd`. It deliberately does not copy the
DEB/RPM upgrade helper, first-run message, or GUI autostart entry.

`flake.nix` pins `github:NixOS/nixpkgs/nixos-unstable` because older release channels may not
meet the project API floor. It uses a local `genAttrs` helper rather than an extra
flake-utils dependency and exposes `packages.<system>.{default,tidemark}`,
`apps.<system>.{default,tidemark,tidemarkd}`, `devShells.<system>.default`, and a Nix
formatter.

## NixOS module

`nix/module.nix` is parameterized by the flake's `self` and exposes an enable flag and an
overrideable package option. The package option defaults to
`self.packages.${pkgs.stdenv.hostPlatform.system}.tidemark`. When enabled, the module adds the
selected package to `environment.systemPackages` and `services.dbus.packages`, then defines
`systemd.user.services.tidemarkd` with `Type = "dbus"`, the published bus name, and
`ExecStart = "${cfg.package}/bin/tidemarkd"`.

`wantedBy = [ ]` is a contract: the unit exists for the D-Bus service file's
`SystemdService=tidemarkd.service` lookup but is not a login service. The unit keeps the
current `After`/`PartOf=graphical-session.target`, restart policy, `NoNewPrivileges`, and its
intentional absence of a filesystem sandbox; several providers read and atomically write
canonical third-party CLI credential files.

Nix store paths are immutable, so the DEB/RPM root-side upgrade restart hook has no NixOS
equivalent. A system rebuild changes the unit's `ExecStart` path but does not enumerate and
restart every graphical user's daemon. An active user may restart their own service normally.

## Verification and CI

`scripts/test-nix-flake.sh` runs `nixos/nix:2.35.2` with a read-only repository mount and
`--network host`. The upstream image disables Nix's own sandbox by default; Docker supplies
the isolation for this acceptance test. The script enables `nix-command flakes`, runs
`nix flake check --no-build`, builds `.#tidemark`, checks staged paths plus the rewritten
D-Bus `Exec`, and starts the built daemon under `dbus-run-session`. `busctl --user` must
introspect the documented object and successfully call `GetStatus`.

An evaluation-only flake check evaluates `nixosModules.default` with
`services.tidemark.enable = true`, asserting the unit's empty `wantedBy`, type, bus name, and
package-qualified executable. This tests module integration without booting or configuring
NixOS. The Docker script becomes a separate CI job on `ubuntu-26.04` and remains the local
maintainer command.

## Documentation

The README presents `nix profile install github:zbndev/tidemark`, `nix run`, and a NixOS
flake-input example importing `tidemark.nixosModules.default` then enabling the daemon. It
states explicitly that the module does not autostart the GUI. `CONTEXT.md` records Nix as an
additional source-install route, while preserving DEB/RPM release assets and the local Arch
`PKGBUILD` as their separate formats.
