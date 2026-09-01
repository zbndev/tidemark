# Nix Flake and NixOS Module Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Tidemark reproducibly buildable and runnable through this repository's Nix flake, with a NixOS module that exposes only its D-Bus-activated user daemon.

**Architecture:** `flake.nix` is a small public catalogue over `nix/package.nix` and `nix/module.nix`. The package builds and wraps the Rust/GTK workspace; the module registers package-owned D-Bus data and emits a non-autostart systemd user unit. Docker builds the actual flake and calls the daemon over a real session bus; CI executes that same script.

**Tech Stack:** Nix flakes, nixpkgs `nixos-unstable`, `rustPlatform.buildRustPackage`, `wrapGAppsHook4`, NixOS `systemd.user.services` / `services.dbus.packages`, Docker, D-Bus.

**Spec:** `docs/superpowers/specs/2026-09-01-nixos-flake-design.md`

## Global Constraints

- Work stays on `feat/nixos-flake`; do not modify `main`, push, or open a PR without a new request.
- Support native Linux `x86_64-linux` and `aarch64-linux` only. The GTK/systemd desktop model does not support macOS.
- Preserve Rust 1.92, GTK 4.22, libadwaita 1.9, workspace layering, and all existing DEB/RPM/Arch packaging.
- Nix package checks are packaging/runtime checks, not the Rust test suite: set `doCheck = false` and retain the native Rust gate.
- `services.tidemark.enable` installs and makes available only the daemon. It must not autostart the GTK process or enable `tidemarkd.service` at login.
- Nix and shell comments are English. New shell is POSIX `sh`, `set -eu`, and shellcheck-clean.
- Use Docker `--network host` for Nix downloads on this host.
- Do not use TDD. Add checks after implementing the package/module interfaces.

---

## File Structure

| File | Responsibility |
| --- | --- |
| `flake.nix` | Inputs, packages, apps, dev shell, formatter, module export, evaluation check. |
| `flake.lock` | Exact nixpkgs revision. |
| `nix/package.nix` | Workspace build, GTK wrapper, installed immutable assets, D-Bus path rewrite. |
| `nix/module.nix` | NixOS options, D-Bus registration, daemon-only user unit. |
| `scripts/test-nix-flake.sh` | Docker acceptance test for flake output and D-Bus runtime. |
| `.github/workflows/ci.yml` | Independent Nix Docker job. |
| `.github/workflows/update-nix-flake.yml` | Weekly, reviewable refresh of the pinned nixpkgs lock input. |
| `README.md`, `CONTEXT.md` | User installation and normative packaging boundary. |

## Task 1: Create the package and flake outputs

**Files:**

- Create: `nix/package.nix`
- Create: `flake.nix`
- Create: `flake.lock` through `nix flake lock`

**Interfaces:**

- Consumes: root `Cargo.lock`, release binaries, icons, desktop metadata, AppStream metadata, and the existing session D-Bus service file.
- Produces: `packages.<system>.{default,tidemark}`, `apps.<system>.{default,tidemark,tidemarkd}`, `devShells.<system>.default`, and `formatter.<system>`.

- [ ] **Step 1: Add `nix/package.nix`**

Implement a `rustPlatform.buildRustPackage` derivation with this mandatory core:

```nix
rustPlatform.buildRustPackage (finalAttrs: {
  pname = "tidemark";
  version = "0.2.1";
  src = ../.;
  cargoLock.lockFile = ../Cargo.lock;
  cargoBuildFlags = [ "--workspace" "--bins" ];
  doCheck = false;

  nativeBuildInputs = [
    pkg-config cmake clang llvmPackages.libclang wrapGAppsHook4 makeWrapper
  ];
  buildInputs = [ gtk4 libadwaita sqlite dbus hicolor-icon-theme ];
  LIBCLANG_PATH = "${llvmPackages.libclang.lib}/lib";
})
```

The argument list includes every identifier above plus `lib`. In `postInstall`, use
`install -Dm755` for both release binaries; install desktop and metainfo below `$out/share`;
copy `data/icons/hicolor` to `$out/share/icons`; install the D-Bus service below
`$out/share/dbus-1/services`; then run:

```sh
substituteInPlace "$out/share/dbus-1/services/io.github.zbndev.Tidemark.Daemon.service" \
  --replace-fail /usr/bin/tidemarkd "$out/bin/tidemarkd"
```

Set MIT metadata, homepage, `lib.platforms.linux`, and `mainProgram = "tidemark"`. Do not
copy the GUI autostart, DEB/RPM upgrade helper, maintainer scripts, or their first-run file.

- [ ] **Step 2: Add the flake catalogue**

Create `flake.nix` with `inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable"` and:

```nix
systems = [ "x86_64-linux" "aarch64-linux" ];
forAllSystems = nixpkgs.lib.genAttrs systems;
packageFor = system:
  let pkgs = import nixpkgs { inherit system; };
  in pkgs.callPackage ./nix/package.nix { };
```

For every system, expose both `default` and `tidemark` package attributes. Expose apps with
programs `${package}/bin/tidemark` and `${package}/bin/tidemarkd`, with the window as default.
Expose a shell containing Cargo, Rust, rustfmt, clippy, pkg-config, CMake, Clang, libclang,
GTK4, libadwaita, SQLite, D-Bus, desktop-file-utils, AppStream, and shellcheck; set its
`LIBCLANG_PATH`. Export `nixfmt-rfc-style` as the formatter. Do not introduce flake-utils.

- [ ] **Step 3: Generate, format, and build**

```sh
nix --extra-experimental-features 'nix-command flakes' flake lock
nix --extra-experimental-features 'nix-command flakes' fmt
nix --extra-experimental-features 'nix-command flakes' flake show
nix --extra-experimental-features 'nix-command flakes' build --no-link .#tidemark
```

Expected: the generated lock file is committed, all Linux outputs appear, and the release
workspace build completes. A missing native tool is fixed by adding its Nix dependency, never
by changing Cargo or vendoring a system library.

- [ ] **Step 4: Commit**

```sh
git add flake.nix flake.lock nix/package.nix
git commit -m "feat(packaging): add a Nix flake package"
```

## Task 2: Add the NixOS daemon module

**Files:**

- Create: `nix/module.nix`
- Modify: `flake.nix`

**Interfaces:**

- Consumes: `self.packages.${pkgs.stdenv.hostPlatform.system}.tidemark` and its D-Bus service file.
- Produces: `nixosModules.default`, `services.tidemark.enable`, `services.tidemark.package`, and `checks.<system>.nixos-module`.

- [ ] **Step 1: Implement `nix/module.nix`**

Use this module contract so the default is this flake's package rather than an imaginary
upstream `pkgs.tidemark`:

```nix
{ self }:
{ config, lib, pkgs, ... }:

let cfg = config.services.tidemark;
in {
  options.services.tidemark = {
    enable = lib.mkEnableOption "the Tidemark user daemon";
    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.tidemark;
      defaultText = lib.literalExpression "inputs.tidemark.packages.\${pkgs.system}.tidemark";
      description = "The Tidemark package to run.";
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];
    services.dbus.packages = [ cfg.package ];
    systemd.user.services.tidemarkd = {
      description = "Tidemark quota daemon";
      after = [ "graphical-session.target" ];
      partOf = [ "graphical-session.target" ];
      wantedBy = [ ];
      serviceConfig = {
        Type = "dbus";
        BusName = "io.github.zbndev.Tidemark.Daemon";
        ExecStart = "${cfg.package}/bin/tidemarkd";
        Restart = "on-failure";
        RestartSec = 5;
        NoNewPrivileges = true;
      };
    };
  };
}
```

Do not add GUI, `WantedBy = [ "default.target" ]`, filesystem sandboxing, user creation, or
an update restart hook. `services.dbus.packages` is required for package-owned session D-Bus
files to enter NixOS's service-directory configuration.

- [ ] **Step 2: Export and evaluate the module**

Add `nixosModules.default = import ./nix/module.nix { inherit self; };` to `flake.nix`. Add a
`checks` output which evaluates `nixpkgs.lib.nixosSystem` with this module and
`{ services.tidemark.enable = true; }`. Before returning
`pkgs.runCommandNoCC "tidemark-nixos-module-evaluation" { } "touch $out"`, assert:

```nix
service.wantedBy == [ ]
service.serviceConfig.Type == "dbus"
service.serviceConfig.BusName == "io.github.zbndev.Tidemark.Daemon"
service.serviceConfig.ExecStart == "${evaluated.config.services.tidemark.package}/bin/tidemarkd"
```

This is an evaluation-only NixOS integration check, not a VM.

- [ ] **Step 3: Format, build the check, and commit**

```sh
nix --extra-experimental-features 'nix-command flakes' fmt
nix --extra-experimental-features 'nix-command flakes' flake check --no-build
nix --extra-experimental-features 'nix-command flakes' build --no-link .#checks.x86_64-linux.nixos-module
git add flake.nix nix/module.nix
git commit -m "feat(nixos): expose the D-Bus activated daemon"
```

Expected: evaluation succeeds and it starts no daemon.

## Task 3: Add Docker acceptance verification

**Files:**

- Create: `scripts/test-nix-flake.sh`

**Interfaces:**

- Consumes: flake outputs from Tasks 1–2 and Docker Engine.
- Produces: the local and CI acceptance command.

- [ ] **Step 1: Implement the script after the package and module**

Create executable POSIX shell using `image=nixos/nix:2.35.2`, a `/src:ro` source mount, and
`docker run --rm --network host --entrypoint sh`. Inside the container run:

```sh
export NIX_CONFIG='experimental-features = nix-command flakes'
nix flake check --no-build
output=$(nix build --no-link --print-out-paths .#tidemark)
```

Assert `$output` contains both binaries, the desktop file, metainfo, the 512px icon and the
D-Bus service. Require exactly `Exec=$output/bin/tidemarkd` and
`SystemdService=tidemarkd.service` in that service file.

Start the daemon with `nix shell --inputs-from . nixpkgs#dbus nixpkgs#systemd -c sh -eu -c ...`
and a nested `dbus-run-session`. The nested shell starts `$output/bin/tidemarkd`, traps exit to
kill/wait for it, retries `busctl --user introspect` for at most 30 seconds, requires
`GetStatus` in the introspection, then calls:

```sh
busctl --user call io.github.zbndev.Tidemark.Daemon \
  /io/github/zbndev/Tidemark io.github.zbndev.Tidemark.Daemon1 GetStatus
```

The source mount remains read-only; the check must not leave host `target/`, `result`, lock,
or root-owned files.

- [ ] **Step 2: Run and lint the completed check**

```sh
shellcheck scripts/test-nix-flake.sh
scripts/test-nix-flake.sh
```

Expected: it builds the flake, verifies staged assets and the store-qualified D-Bus path, then
completes a real D-Bus status call. The initial download/build can take time.

- [ ] **Step 3: Commit**

```sh
git add scripts/test-nix-flake.sh
git commit -m "test(packaging): exercise the Nix flake in Docker"
```

## Task 4: CI and reviewable weekly input refresh

**Files:**

- Modify: `.github/workflows/ci.yml`
- Create: `.github/workflows/update-nix-flake.yml`

**Interfaces:**

- Consumes: the Docker command and root `flake.lock` from prior tasks.
- Produces: continuous flake validation and a weekly review PR for the lock file.

- [ ] **Step 1: Add a dedicated CI job**

Append this sibling of `checks` in `.github/workflows/ci.yml`:

```yaml
  nix:
    name: Nix flake
    runs-on: ubuntu-26.04
    steps:
      - uses: actions/checkout@v5

      - name: Build and exercise the flake in Docker
        run: scripts/test-nix-flake.sh
```

Do not install Nix onto the runner or merge this work into the native Rust/toolkit job.

- [ ] **Step 2: Add the weekly lock-refresh workflow**

Create `.github/workflows/update-nix-flake.yml` with a manual trigger and
`schedule: - cron: "0 0 * * 0"`, plus only `contents: write` and `pull-requests: write`
permissions. Its `ubuntu-26.04` job must:

```yaml
- uses: actions/checkout@v5

- name: Update the pinned nixpkgs input
  run: |
    docker run --rm --network host --entrypoint sh \
      -v "$GITHUB_WORKSPACE":/src \
      -w /src \
      nixos/nix:2.35.2 -eu -c '
        export NIX_CONFIG="experimental-features = nix-command flakes"
        nix flake update
      '
    sudo chown "$(id -u):$(id -g)" flake.lock

- name: Build and exercise the updated flake
  run: scripts/test-nix-flake.sh

- name: Create or update the review PR
  uses: peter-evans/create-pull-request@v7
  with:
    token: ${{ secrets.GITHUB_TOKEN }}
    branch: chore/nix-flake-lock
    delete-branch: true
    commit-message: "chore(nix): update flake lock"
    title: "chore(nix): update flake lock"
    body: |
      Weekly nixpkgs lock refresh, built and exercised by `scripts/test-nix-flake.sh`.
```

The update mount is writable only because `nix flake update` must replace `flake.lock`; the
next Docker invocation is the read-only QA mount from Task 3. The `chown` is necessary because
the Nix image writes as container root. Do not copy Caelestia's direct-to-main behavior: an
update can alter Rust, GTK, libadwaita, systemd, and transitive provider build inputs, so this
workflow must create a reviewable PR rather than merge or push to `main`.

- [ ] **Step 3: Validate and commit the CI workflows**

```sh
shellcheck scripts/*.sh
sed -n '/^  nix:/,$p' .github/workflows/ci.yml
sed -n '1,220p' .github/workflows/update-nix-flake.yml
scripts/test-nix-flake.sh
git add .github/workflows/ci.yml .github/workflows/update-nix-flake.yml
git commit -m "ci: refresh the Nix flake lock weekly"
```

Expected: the refresh job has one scheduled and one manual trigger, runs Docker QA before PR
creation, and does not mutate `main`.

## Task 5: Documentation and final gate

**Files:**

- Modify: `README.md`
- Modify: `CONTEXT.md`

**Interfaces:**

- Consumes: the public outputs, module, Docker command, and weekly lock PR behavior from prior tasks.
- Produces: copy-pasteable standalone Nix/NixOS setup and maintainer expectations for lock refreshes.

- [ ] **Step 1: Add README instructions**

After source-building, add `## Nix` with `nix profile install github:zbndev/tidemark` and
`nix run github:zbndev/tidemark`. Include a NixOS flake-input fragment importing
`tidemark.nixosModules.default` and setting `{ services.tidemark.enable = true; }`. State that
`nix run github:zbndev/tidemark#tidemarkd` is diagnostic-only and the module does **not**
autostart the GUI. Do not claim an external nixpkgs package or a separate Nix repository.

- [ ] **Step 2: Amend the normative packaging record**

Add one concise `CONTEXT.md` packaging paragraph: the repo-local flake exports the Nix package
and `nixosModules.default`; enabling it registers only the D-Bus-activated user daemon. State
that the scheduled workflow proposes `flake.lock` updates through a reviewed PR. DEB, RPM, and
local `PKGBUILD` retain their distinct installation and upgrade behavior.

- [ ] **Step 3: Run the complete relevant gate and commit**

```sh
cargo fmt --check && \
cargo clippy --workspace --all-targets -- -D warnings && \
dbus-run-session -- cargo test --workspace && \
scripts/check-layering.sh && \
scripts/check-desktop-integration.sh && \
scripts/test-restart-user-daemon.sh && \
shellcheck scripts/*.sh data/restart-user-daemon \
  data/packaging/deb/postinst data/packaging/rpm/post-install.sh && \
scripts/test-nix-flake.sh

git add README.md CONTEXT.md
git commit -m "docs: describe Nix and NixOS installation"
git status --short
git log --oneline main..HEAD
```

Expected: native checks remain green and Docker proves the Nix build, staged assets, rewritten
D-Bus executable, and daemon session-bus contract. Report Docker/Nix network or sandbox
failures separately from a code regression; do not weaken the check to hide them. The final
branch is clean and contains only the flake/module, Docker QA, CI hook, and documentation.
