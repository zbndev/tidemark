# CI and Release Packaging Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** GitHub Actions checks every change against the real toolkit floor, a `v*` tag builds a `.deb` and an `.rpm` into a draft release, and a package upgrade demonstrably leaves the *new* `tidemarkd` running.

**Architecture:** Checks and the `.deb` build run natively on the `ubuntu-26.04` runner; the `.rpm` builds in a `fedora:44` container because GitHub has no Fedora runner. Packages are produced by `cargo-deb` and `cargo-generate-rpm` from metadata in `crates/tidemark/Cargo.toml`, both deriving dependencies from the built ELF. Both formats' maintainer scripts call the one existing `data/restart-user-daemon`. The upgrade is proven once, by hand, with a script that runs systemd inside Docker; it has no CI trigger.

**Tech Stack:** GitHub Actions, `cargo-deb`, `cargo-generate-rpm`, Docker, systemd user units, D-Bus (`zbus`), GTK 4.22 / libadwaita 1.9.

**Spec:** `docs/superpowers/specs/2026-08-22-ci-release-packaging-design.md`

## Global Constraints

- API floor is GTK **4.22** and libadwaita **1.9** (`v4_22` / `v1_9` binding features). Targets are **Fedora 44+** and **Ubuntu 26.04+**. Nothing older qualifies.
- Rust comes from `rustup` honouring `rust-toolchain.toml` (`channel = "stable"`, floor 1.92), never from the distribution's packaged toolchain.
- All documentation, source code, code comments, tests, logs, and interface copy are written in English.
- `cargo clippy --workspace --all-targets -- -D warnings` must pass. `unsafe_code` is `forbid` workspace-wide.
- `crates/tidemark` must never depend on `tidemark-core`, an HTTP client, or a database driver. `scripts/check-layering.sh` enforces this.
- `PKGBUILD` is not modified by any task in this plan.
- The release workflow creates a **draft**. Nothing is published automatically.
- `scripts/test-package-upgrade.sh` gets **no** GitHub Actions trigger — not `push`, not a tag, not `workflow_dispatch`.
- No Flatpak, no Debian target, no `lintian` / `rpmlint` gate.
- Commit messages end with the repository's `Co-Authored-By` trailer, matching existing history.

---

## File Structure

| File | Responsibility |
|---|---|
| `.github/workflows/ci.yml` | Create. Checks on PR and pushes to `main`. |
| `.github/workflows/release.yml` | Create. Tag-driven build of both packages into a draft release. |
| `crates/tidemark/Cargo.toml` | Modify. `[package.metadata.deb]` and `[package.metadata.generate-rpm]`. |
| `data/packaging/deb/postinst` | Create. Debian `postinst`; calls `restart-user-daemon` on upgrade. |
| `data/packaging/rpm/post-install.sh` | Create. RPM `%post`; calls `restart-user-daemon` on upgrade. |
| `data/packaging/message.txt` | Create. The fresh-install guidance text, shared by both scripts and by `tidemark.install`. |
| `scripts/check-tag-version.sh` | Create. Asserts a `v*` tag agrees with `workspace.package.version`. |
| `scripts/test-package-upgrade.sh` | Create. The one-off systemd-in-Docker upgrade proof. |
| `crates/tidemark/src/bus.rs` | Modify. Read the daemon's `Version`, emit `Update::Version`. |
| `crates/tidemark/src/window.rs` | Modify. An `AdwBanner` top bar driven by `Update::Version`. |
| `CONTEXT.md` | Modify. § Packaging replaced with the real target set. |
| `README.md` | Modify. Installation section. |
| `PLAN.md` | Modify. Step 17 done, log entry. Not tracked in git. |

---

## Task 1: CI workflow

**Files:**
- Create: `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: nothing.
- Produces: a reusable list of check steps that Task 7 copies into `release.yml` job 1.

**Background the implementer needs.** `ubuntu-26.04` is a GitHub-hosted runner label that entered public preview in June 2026. It is the only Ubuntu runner carrying GTK 4.22; `ubuntu-latest` is still 24.04 and this code does not compile against its GTK 4.18. `dbus-run-session` is not optional: the tests in `crates/tidemark-core/src/secrets.rs` *skip themselves* when no session bus is reachable (see the module doc at `secrets.rs:378`), so without a bus the job goes green over tests that never executed.

- [ ] **Step 1: Write the workflow**

Create `.github/workflows/ci.yml`:

```yaml
name: CI

on:
  pull_request:
  push:
    branches: [main]

concurrency:
  group: ci-${{ github.ref }}
  cancel-in-progress: true

env:
  CARGO_TERM_COLOR: always

jobs:
  checks:
    # The only hosted runner carrying GTK 4.22 / libadwaita 1.9, which is the project's
    # API floor. ubuntu-latest is still 24.04 and ships GTK 4.18; this code does not
    # compile against it.
    runs-on: ubuntu-26.04
    steps:
      - uses: actions/checkout@v5

      - name: Install the toolkit and the shell-check tools
        run: |
          sudo apt-get update
          sudo apt-get install --no-install-recommends -y \
            libgtk-4-dev libadwaita-1-dev libsqlite3-dev pkg-config \
            dbus-daemon desktop-file-utils appstream shellcheck

      - name: Confirm the toolkit meets the floor
        run: |
          pkg-config --atleast-version=4.22 gtk4
          pkg-config --atleast-version=1.9 libadwaita-1
          pkg-config --modversion gtk4 libadwaita-1

      # rust-toolchain.toml pins the channel; rustup reads it on first use.
      - name: Show the toolchain
        run: rustup show && cargo --version

      - uses: Swatinem/rust-cache@v2

      - name: Format
        run: cargo fmt --all --check

      - name: Clippy
        run: cargo clippy --workspace --all-targets -- -D warnings

      # A session bus, because tidemark-core's Secret Service tests skip themselves
      # without one and tidemarkd opens a session connection. With a bus but no Secret
      # Service the keyring assertions still skip, which is the designed behaviour.
      - name: Tests
        run: dbus-run-session -- cargo test --workspace

      - name: Layering
        run: scripts/check-layering.sh

      - name: Desktop integration
        run: scripts/check-desktop-integration.sh

      - name: The user-daemon restart helper
        run: scripts/test-restart-user-daemon.sh

      - name: Shell
        run: shellcheck scripts/*.sh data/restart-user-daemon
```

- [ ] **Step 2: Check the workflow parses before pushing it**

Run:

```bash
python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/ci.yml')); print('yaml ok')"
```

Expected: `yaml ok`.

- [ ] **Step 3: Run the same checks locally, so the first CI run is not the first attempt**

Run:

```bash
cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && dbus-run-session -- cargo test --workspace && scripts/check-layering.sh && scripts/test-restart-user-daemon.sh && shellcheck scripts/*.sh data/restart-user-daemon
```

Expected: all pass. If `shellcheck` reports findings in the pre-existing scripts, fix them in this task — the job will not go green otherwise, and they are two-line fixes.

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/ci.yml scripts/
git commit -m "ci: check every change on ubuntu-26.04

The only hosted runner meeting the GTK 4.22 / libadwaita 1.9 floor. Tests
run under dbus-run-session, because tidemark-core's Secret Service tests
skip themselves when no session bus is reachable and would otherwise
report green without running.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

- [ ] **Step 5: Push the branch and confirm the job goes green**

Run: `git push` and watch the run with `gh run watch`.
Expected: the `checks` job succeeds. If `ubuntu-26.04` queues for a long time, that is the accepted preview risk recorded in the spec — wait rather than downgrading the runner.

---

## Task 2: The `.deb`

**Files:**
- Modify: `crates/tidemark/Cargo.toml`
- Create: `data/packaging/message.txt`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: a `.deb` at `target/debian/tidemark_0.1.0-1_amd64.deb`. Task 4 adds its maintainer scripts. Task 7 builds it in CI.

**Background the implementer needs.** The metadata lives in `crates/tidemark/Cargo.toml` because a virtual workspace root has no `[package]` to hang it on. Asset source paths are therefore relative to `crates/tidemark/`, which is why the data files are reached as `../../data/...`. `cargo-deb` special-cases a leading `target/release/`, replacing it with the real target directory, so binary paths are written exactly that way even in a workspace. Both binaries come from one prior `cargo build --release --workspace`, and `--no-build` stops `cargo-deb` from rebuilding only its own crate.

The asset list must match `package()` in `PKGBUILD` file for file. Read that function before writing this; it is the specification of what a Tidemark package contains.

- [ ] **Step 1: Extract the fresh-install message into a shared file**

Create `data/packaging/message.txt` with exactly the text `tidemark.install`'s `post_install()` prints today:

```text
Opening Tidemark starts its daemon through D-Bus activation. The desktop session also
starts Tidemark hidden when a StatusNotifier host is available, keeping its tray ready.

It reports "no-credential" until a provider key is in the Secret Service:

    secret-tool store --label='Tidemark: zai (default)' \
        xdg:schema io.github.zbndev.Tidemark.ProviderKey provider zai account default

Everything it knows is readable without the GUI:

    busctl --user call io.github.zbndev.Tidemark.Daemon /io/github/zbndev/Tidemark \
        io.github.zbndev.Tidemark.Daemon1 GetStatus
```

- [ ] **Step 2: Confirm it matches what `tidemark.install` already prints**

Run:

```bash
sed -n "/cat <<'EOF'/,/^EOF$/p" tidemark.install | sed '1d;$d' | diff -u - data/packaging/message.txt
```

Expected: no output. If they differ, the file is wrong — `tidemark.install` is the original.

- [ ] **Step 3: Install `cargo-deb`**

Run: `cargo install cargo-deb --locked`
Expected: it builds and lands in `~/.cargo/bin`.

- [ ] **Step 4: Add the metadata**

Append to `crates/tidemark/Cargo.toml`:

```toml
# The Debian package. It carries binaries from two crates, so it is built from a prior
# `cargo build --release --workspace` and invoked with `--no-build`. The metadata lives
# here rather than at the workspace root because a virtual manifest has no [package] to
# hang it on, which is also why the data files are reached as `../../data/...`.
#
# The asset list mirrors package() in PKGBUILD file for file. That function is the
# definition of what a Tidemark package contains; keep the two in step.
[package.metadata.deb]
maintainer = "zbndev <https://github.com/zbndev>"
copyright = "2026 zbndev"
license-file = ["../../LICENSE", "0"]
section = "utils"
priority = "optional"
extended-description = """
Tidemark tracks how much of each AI provider's rate-limit window is burned, when it
resets, and whether the current pace reaches it. A polling daemon runs as a systemd
user unit; the GTK4 interface reads it over D-Bus."""
# $auto is dpkg-shlibdeps over the built ELF, so GTK, libadwaita and SQLite are named the
# way Ubuntu names them rather than the way a hand-written list guesses. The two appended
# by hand are the two no ELF can reveal: zbus is pure Rust, so libdbus is never linked,
# and an icon theme leaves no trace in a binary at all. They match the
# [package.metadata.generate-rpm.requires] table below, under Debian's names.
depends = "$auto, dbus-user-session, hicolor-icon-theme"
conf-files = ["/etc/xdg/autostart/io.github.zbndev.Tidemark.desktop"]
maintainer-scripts = "../../data/packaging/deb"
assets = [
    ["target/release/tidemark", "usr/bin/", "755"],
    ["target/release/tidemarkd", "usr/bin/", "755"],
    ["../../data/tidemarkd.service", "usr/lib/systemd/user/", "644"],
    ["../../data/icons/hicolor/16x16/apps/io.github.zbndev.Tidemark.png", "usr/share/icons/hicolor/16x16/apps/", "644"],
    ["../../data/icons/hicolor/22x22/apps/io.github.zbndev.Tidemark.png", "usr/share/icons/hicolor/22x22/apps/", "644"],
    ["../../data/icons/hicolor/24x24/apps/io.github.zbndev.Tidemark.png", "usr/share/icons/hicolor/24x24/apps/", "644"],
    ["../../data/icons/hicolor/32x32/apps/io.github.zbndev.Tidemark.png", "usr/share/icons/hicolor/32x32/apps/", "644"],
    ["../../data/icons/hicolor/48x48/apps/io.github.zbndev.Tidemark.png", "usr/share/icons/hicolor/48x48/apps/", "644"],
    ["../../data/icons/hicolor/64x64/apps/io.github.zbndev.Tidemark.png", "usr/share/icons/hicolor/64x64/apps/", "644"],
    ["../../data/icons/hicolor/128x128/apps/io.github.zbndev.Tidemark.png", "usr/share/icons/hicolor/128x128/apps/", "644"],
    ["../../data/icons/hicolor/256x256/apps/io.github.zbndev.Tidemark.png", "usr/share/icons/hicolor/256x256/apps/", "644"],
    ["../../data/icons/hicolor/512x512/apps/io.github.zbndev.Tidemark.png", "usr/share/icons/hicolor/512x512/apps/", "644"],
    ["../../data/icons/hicolor/512x512@2/apps/io.github.zbndev.Tidemark.png", "usr/share/icons/hicolor/512x512@2/apps/", "644"],
    # These SVGs are their owners' trademarks and are not under this package's licence,
    # which is why TRADEMARKS.md is installed beside LICENSE. A rebuild that drops both
    # lines is a supported configuration: a card with no mark is a state the interface
    # already has.
    ["../../data/icons/hicolor/symbolic/apps/tidemark-*-symbolic.svg", "usr/share/icons/hicolor/symbolic/apps/", "644"],
    ["../../data/applications/io.github.zbndev.Tidemark.desktop", "usr/share/applications/", "644"],
    ["../../data/autostart/io.github.zbndev.Tidemark.desktop", "etc/xdg/autostart/", "644"],
    ["../../data/metainfo/io.github.zbndev.Tidemark.metainfo.xml", "usr/share/metainfo/", "644"],
    ["../../data/dbus-1/services/io.github.zbndev.Tidemark.Daemon.service", "usr/share/dbus-1/services/", "644"],
    ["../../data/restart-user-daemon", "usr/lib/tidemark/restart-user-daemon", "755"],
    ["../../docs/TRADEMARKS.md", "usr/share/doc/tidemark/TRADEMARKS.md", "644"],
    ["../../README.md", "usr/share/doc/tidemark/README.md", "644"],
]
```

- [ ] **Step 5: Build the workspace, then the package**

Run:

```bash
cargo build --release --locked --workspace && cargo deb --no-build -p tidemark
```

Expected: a `.deb` under `target/debian/`.

**This step is also the `!lto` verification the spec left open.** `PKGBUILD` sets `options=(!lto)` because makepkg's `CFLAGS` make the `cc` crate emit GCC LTO bitcode for `aws-lc-sys` that `rust-lld` cannot link. `cargo-deb` injects no `CFLAGS`, so the workspace's own `lto = "thin"` is expected to link cleanly. If instead the release build fails with hundreds of undefined `aws_lc_*` symbols, the expectation was wrong: record that in `PLAN.md`, and set `CFLAGS=-fno-lto` for the package builds in Tasks 7 rather than disabling the project's own thin LTO.

- [ ] **Step 6: Read the generated control file with your eyes**

Run:

```bash
dpkg-deb -I target/debian/*.deb && echo '--- contents ---' && dpkg-deb -c target/debian/*.deb
```

Expected: `Depends:` names real Ubuntu packages — `libgtk-4-1`, `libadwaita-1-0`, `libsqlite3-0`, `libc6` and friends.

**This is the runner-contamination check the spec calls for.** `dpkg-shlibdeps` resolves against *installed* packages, so if this is run on a loaded machine the list can be wider than a default installation needs. Real Ubuntu names are fine even if there are extra ones. If it carries obvious build-machine junk — `libllvm*`, a toolchain package, anything a desktop application plainly cannot need at runtime — then move the `.deb` build into a clean `ubuntu:26.04` container in Task 7 and note why in `PLAN.md`.

Confirm the contents list matches `PKGBUILD`'s `package()` with nothing missing.

- [ ] **Step 7: Commit**

```bash
git add crates/tidemark/Cargo.toml data/packaging/message.txt
git commit -m "packaging: build a .deb with cargo-deb

Dependencies come from dpkg-shlibdeps against the built ELF rather than a
hand-written list, because Debian and Fedora name GTK, libadwaita and
SQLite differently and a hand-written list is the line that rots. The
asset list mirrors package() in PKGBUILD.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

## Task 3: The `.rpm`

**Files:**
- Modify: `crates/tidemark/Cargo.toml`

**Interfaces:**
- Consumes: `data/packaging/message.txt` from Task 2.
- Produces: an `.rpm` under `target/generate-rpm/`. Task 4 adds its `%post`. Task 7 builds it in CI.

**Background the implementer needs.** `cargo-generate-rpm` takes `assets` as an array of tables, not arrays, and `dest` is an absolute path. `auto-req = "find-requires"` runs rpm's own `/usr/lib/rpm/find-requires` over the payload, which needs to happen on Fedora — running it on Arch would produce Arch's idea of the provides. So the authoritative build of this package is the one in the Fedora container (Task 7); a local run is a smoke test of the *metadata*, not of the dependency list.

- [ ] **Step 1: Install `cargo-generate-rpm`**

Run: `cargo install cargo-generate-rpm --locked`
Expected: it builds and lands in `~/.cargo/bin`.

- [ ] **Step 2: Add the metadata**

Append to `crates/tidemark/Cargo.toml`:

```toml
# The RPM. Same shape and the same asset list as [package.metadata.deb] above; the two
# are separate because the dependency names are, and a single hand-written manifest for
# both would only look unified.
#
# auto-req runs rpm's own find-requires over the payload, so this package's dependency
# list is only authoritative when built on Fedora. A local build on another distribution
# is a smoke test of the metadata, not of the requires.
[package.metadata.generate-rpm]
summary = "Track AI provider quota limits"
license = "MIT"
url = "https://github.com/zbndev/tidemark"
auto-req = "find-requires"
post_install_script = "../../data/packaging/rpm/post-install.sh"
post_install_script_prog = ["/bin/sh", "-e"]
assets = [
    { source = "target/release/tidemark", dest = "/usr/bin/tidemark", mode = "755" },
    { source = "target/release/tidemarkd", dest = "/usr/bin/tidemarkd", mode = "755" },
    { source = "../../data/tidemarkd.service", dest = "/usr/lib/systemd/user/tidemarkd.service", mode = "644" },
    { source = "../../data/icons/hicolor/16x16/apps/io.github.zbndev.Tidemark.png", dest = "/usr/share/icons/hicolor/16x16/apps/io.github.zbndev.Tidemark.png", mode = "644" },
    { source = "../../data/icons/hicolor/22x22/apps/io.github.zbndev.Tidemark.png", dest = "/usr/share/icons/hicolor/22x22/apps/io.github.zbndev.Tidemark.png", mode = "644" },
    { source = "../../data/icons/hicolor/24x24/apps/io.github.zbndev.Tidemark.png", dest = "/usr/share/icons/hicolor/24x24/apps/io.github.zbndev.Tidemark.png", mode = "644" },
    { source = "../../data/icons/hicolor/32x32/apps/io.github.zbndev.Tidemark.png", dest = "/usr/share/icons/hicolor/32x32/apps/io.github.zbndev.Tidemark.png", mode = "644" },
    { source = "../../data/icons/hicolor/48x48/apps/io.github.zbndev.Tidemark.png", dest = "/usr/share/icons/hicolor/48x48/apps/io.github.zbndev.Tidemark.png", mode = "644" },
    { source = "../../data/icons/hicolor/64x64/apps/io.github.zbndev.Tidemark.png", dest = "/usr/share/icons/hicolor/64x64/apps/io.github.zbndev.Tidemark.png", mode = "644" },
    { source = "../../data/icons/hicolor/128x128/apps/io.github.zbndev.Tidemark.png", dest = "/usr/share/icons/hicolor/128x128/apps/io.github.zbndev.Tidemark.png", mode = "644" },
    { source = "../../data/icons/hicolor/256x256/apps/io.github.zbndev.Tidemark.png", dest = "/usr/share/icons/hicolor/256x256/apps/io.github.zbndev.Tidemark.png", mode = "644" },
    { source = "../../data/icons/hicolor/512x512/apps/io.github.zbndev.Tidemark.png", dest = "/usr/share/icons/hicolor/512x512/apps/io.github.zbndev.Tidemark.png", mode = "644" },
    { source = "../../data/icons/hicolor/512x512@2/apps/io.github.zbndev.Tidemark.png", dest = "/usr/share/icons/hicolor/512x512@2/apps/io.github.zbndev.Tidemark.png", mode = "644" },
    { source = "../../data/icons/hicolor/symbolic/apps/tidemark-*-symbolic.svg", dest = "/usr/share/icons/hicolor/symbolic/apps/", mode = "644" },
    { source = "../../data/applications/io.github.zbndev.Tidemark.desktop", dest = "/usr/share/applications/io.github.zbndev.Tidemark.desktop", mode = "644" },
    { source = "../../data/autostart/io.github.zbndev.Tidemark.desktop", dest = "/etc/xdg/autostart/io.github.zbndev.Tidemark.desktop", mode = "644", config = "noreplace" },
    { source = "../../data/metainfo/io.github.zbndev.Tidemark.metainfo.xml", dest = "/usr/share/metainfo/io.github.zbndev.Tidemark.metainfo.xml", mode = "644" },
    { source = "../../data/dbus-1/services/io.github.zbndev.Tidemark.Daemon.service", dest = "/usr/share/dbus-1/services/io.github.zbndev.Tidemark.Daemon.service", mode = "644" },
    { source = "../../data/restart-user-daemon", dest = "/usr/lib/tidemark/restart-user-daemon", mode = "755" },
    { source = "../../LICENSE", dest = "/usr/share/licenses/tidemark/LICENSE", mode = "644", doc = true },
    { source = "../../docs/TRADEMARKS.md", dest = "/usr/share/licenses/tidemark/TRADEMARKS.md", mode = "644", doc = true },
    { source = "../../README.md", dest = "/usr/share/doc/tidemark/README.md", mode = "644", doc = true },
]

# Not discoverable from the ELF: zbus is pure Rust, so libdbus is never linked, and an
# icon theme leaves no trace in a binary.
[package.metadata.generate-rpm.requires]
dbus-common = "*"
hicolor-icon-theme = "*"
```

Note the `.deb` in Task 2 has no `LICENSE` asset line because `license-file` already installs it; the RPM has no such field, so it is listed explicitly here.

- [ ] **Step 3: Create a placeholder `post-install.sh` so the metadata resolves**

Task 4 writes the real one. For now:

```bash
mkdir -p data/packaging/rpm
printf '%s\n' '#!/bin/sh' '# Replaced in Task 4.' 'exit 0' > data/packaging/rpm/post-install.sh
chmod +x data/packaging/rpm/post-install.sh
```

- [ ] **Step 4: Build it**

Run:

```bash
cargo build --release --locked --workspace && cargo generate-rpm -p crates/tidemark
```

Expected: an `.rpm` under `target/generate-rpm/`.

- [ ] **Step 5: Read the payload**

Run:

```bash
rpm -qlp target/generate-rpm/*.rpm && echo '--- requires ---' && rpm -qRp target/generate-rpm/*.rpm
```

Expected: the file list matches `PKGBUILD`'s `package()`. The requires list on a non-Fedora machine will be wrong or empty — that is expected and is not a failure of this step; Task 7 produces the authoritative one.

- [ ] **Step 6: Commit**

```bash
git add crates/tidemark/Cargo.toml data/packaging/rpm/post-install.sh
git commit -m "packaging: build an .rpm with cargo-generate-rpm

Same asset list as the .deb. The requires list comes from rpm's own
find-requires, so it is only authoritative when built on Fedora, which is
what the release workflow does.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

## Task 4: Maintainer scripts that restart the daemon

**Files:**
- Create: `data/packaging/deb/postinst`
- Modify: `data/packaging/rpm/post-install.sh`

**Interfaces:**
- Consumes: `data/packaging/message.txt` (Task 2), and `/usr/lib/tidemark/restart-user-daemon`, installed by both packages.
- Produces: the behaviour Task 6 proves.

**Background the implementer needs.** There is one source of truth for the restart — `data/restart-user-daemon`, already written, already tested by `scripts/test-restart-user-daemon.sh`. It exists because a package transaction runs as root, and an unqualified `systemctl --user` would talk to *root's* manager rather than the logged-in desktop users'; it reaches each real user manager through `--machine=<user>@.host`.

Do **not** use `cargo-deb`'s `systemd-units` feature. It generates `dh_installsystemd`-shaped code for the **system** scope, and `tidemarkd.service` is a **user** unit — it would issue `daemon-reload` and `enable` against root's manager, which is precisely the failure the `.host` transport exists to avoid.

Two ordering facts, so they are not rediscovered:
- On an **rpm upgrade** the new package's `%post` runs *before* the old package's `%postun`. The restart therefore lives only in `%post`, and no `%postun` restart logic is ever added.
- On a **dpkg upgrade** `postinst configure` runs after the new files are unpacked, which is the correct point.

`try-restart` stays, rather than `restart`: an inactive daemon is left inactive, and D-Bus activation starts the new binary when Tidemark is next opened.

- [ ] **Step 1: Write the Debian `postinst`**

Create `data/packaging/deb/postinst`:

```sh
#!/bin/sh
set -e

# dpkg runs this after the new files are unpacked, which is the point at which restarting
# the daemon picks up the new binary rather than the old one.
#
# $1 is the action, $2 the previously configured version. A non-empty $2 means this is an
# upgrade; empty means a fresh install, which has no running daemon to replace.
if [ "$1" = configure ]; then
    if [ -n "$2" ]; then
        /usr/lib/tidemark/restart-user-daemon
    else
        cat /usr/share/doc/tidemark/first-run.txt
    fi
fi

#DEBHELPER#
```

- [ ] **Step 2: Install the message where `postinst` reads it**

The `postinst` above reads `/usr/share/doc/tidemark/first-run.txt`, so both packages must ship it. Add to the `assets` array in `[package.metadata.deb]` in `crates/tidemark/Cargo.toml`:

```toml
    ["../../data/packaging/message.txt", "usr/share/doc/tidemark/first-run.txt", "644"],
```

and to `[package.metadata.generate-rpm]`:

```toml
    { source = "../../data/packaging/message.txt", dest = "/usr/share/doc/tidemark/first-run.txt", mode = "644" },
```

- [ ] **Step 3: Write the RPM `%post`**

Replace `data/packaging/rpm/post-install.sh` with:

```sh
#!/bin/sh
set -e

# rpm passes the number of installed instances of this package: 1 on a fresh install, 2 or
# more on an upgrade. On an upgrade the new package's %post runs *before* the old
# package's %postun, so the restart belongs here and nowhere else — a %postun that stopped
# anything would run afterwards and undo it. There is deliberately no %postun.
if [ "$1" -ge 2 ]; then
    /usr/lib/tidemark/restart-user-daemon
else
    cat /usr/share/doc/tidemark/first-run.txt
fi
```

- [ ] **Step 4: Make the deb script executable and shellcheck both**

Run:

```bash
chmod +x data/packaging/deb/postinst data/packaging/rpm/post-install.sh
shellcheck data/packaging/deb/postinst data/packaging/rpm/post-install.sh
```

Expected: no findings. `#DEBHELPER#` is a comment to `sh`, so it does not upset `shellcheck`.

- [ ] **Step 5: Rebuild both packages and confirm the scripts are embedded and branch correctly**

Run:

```bash
cargo build --release --locked --workspace
cargo deb --no-build -p tidemark
cargo generate-rpm -p crates/tidemark
dpkg-deb -I target/debian/*.deb postinst
echo '--- rpm ---'
rpm -qp --scripts target/generate-rpm/*.rpm
```

Expected: `postinst` contains the `configure` / `-n "$2"` branch and calls `/usr/lib/tidemark/restart-user-daemon`; the rpm `postinstall` scriptlet contains the `-ge 2` branch and the same call.

- [ ] **Step 6: Add `shellcheck` over the new scripts to CI**

In `.github/workflows/ci.yml`, change the `Shell` step to:

```yaml
      - name: Shell
        run: |
          shellcheck scripts/*.sh data/restart-user-daemon \
            data/packaging/deb/postinst data/packaging/rpm/post-install.sh
```

- [ ] **Step 7: Commit**

```bash
git add data/packaging crates/tidemark/Cargo.toml .github/workflows/ci.yml
git commit -m "packaging: restart the user daemon on upgrade in both formats

Both call the existing data/restart-user-daemon, which reaches each real
user manager through --machine=<user>@.host rather than talking to root's.
cargo-deb's systemd-units feature is deliberately unused: it generates
system-scope dh_installsystemd code, and tidemarkd.service is a user unit.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

## Task 5: The version-mismatch banner

**Files:**
- Modify: `crates/tidemark/src/bus.rs`
- Modify: `crates/tidemark/src/window.rs`

**Interfaces:**
- Consumes: the `version()` property already declared on `DaemonProxy` in `bus.rs` and already implemented in `crates/tidemarkd/src/service.rs`. No daemon-side change is needed.
- Produces:
  - `pub const CLIENT_VERSION: &str` in `bus.rs`
  - `pub fn version_notice(daemon: Option<&str>) -> Option<String>` in `bus.rs`
  - `Update::Version(Option<String>)` variant in `bus.rs`

**Background the implementer needs.** The package restarts the daemon; nothing can restart the GUI, which lives in the user's tray and belongs to them. So an upgrade can leave a new daemon talking to an old interface. The banner is **advisory**: the client does not refuse to talk to a daemon of another version, because a stale banner is a smaller harm than a program that will not start.

`load()` in `bus.rs` runs on every connect *and* on every `Event::Owner(Some(Some(_)))` — that is, whenever the daemon appears or is replaced. Emitting the notice from there means a restart into a matching version clears the banner by itself, with no separate teardown.

- [ ] **Step 1: Write the failing tests**

Append to `crates/tidemark/src/bus.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_daemon_of_the_same_version_needs_no_notice() {
        assert_eq!(version_notice(Some(CLIENT_VERSION)), None);
    }

    #[test]
    fn a_daemon_of_another_version_names_both_of_them() {
        let notice = version_notice(Some("9.9.9")).expect("a differing version is worth saying");
        assert!(notice.contains("9.9.9"), "the daemon's version: {notice}");
        assert!(notice.contains(CLIENT_VERSION), "the client's version: {notice}");
    }

    // Not knowing is not the same as disagreeing. An unreadable property means an older
    // daemon, or a transient failure, and neither is worth a banner telling the user to
    // restart a program that may be perfectly current.
    #[test]
    fn an_unreadable_version_says_nothing() {
        assert_eq!(version_notice(None), None);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p tidemark --lib bus::tests`
Expected: FAIL — `cannot find function 'version_notice'` and `cannot find value 'CLIENT_VERSION'`.

- [ ] **Step 3: Implement the comparison**

Add near the top of `crates/tidemark/src/bus.rs`, after `RETRY_SECONDS`:

```rust
/// What this interface was built as. Compared against the daemon's own `Version`, because
/// a package upgrade restarts the daemon and cannot restart a program sitting in the
/// user's tray.
pub const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The banner text for a daemon that is not this build, if it is not.
///
/// `None` for a version that matches, and also for one that could not be read: an
/// unreadable property means an old daemon or a transient failure, and neither is grounds
/// for telling the user to restart something that may be perfectly current. The result is
/// advisory — the client keeps talking to whatever answered.
pub fn version_notice(daemon: Option<&str>) -> Option<String> {
    let daemon = daemon?;
    if daemon == CLIENT_VERSION {
        return None;
    }
    Some(format!(
        "Tidemark was updated. The daemon is {daemon} and this window is {CLIENT_VERSION}; \
         restart Tidemark to catch up."
    ))
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p tidemark --lib bus::tests`
Expected: 3 passed.

- [ ] **Step 5: Add the `Update` variant and emit it**

In the `Update` enum in `bus.rs`, add:

```rust
    /// Whether the daemon that answered is a different build from this one. `None` clears
    /// a notice a previous daemon caused, so a restart into a matching version needs no
    /// separate teardown.
    Version(Option<String>),
```

In `load()`, after the proxy has answered, add the read. `load()` currently begins:

```rust
async fn load(proxy: &DaemonProxy<'static>, on: &impl Fn(Update)) {
    let definitions = proxy.list_providers().await;
    let statuses = proxy.get_status().await;
```

Add immediately after those two lines:

```rust
    // Read here rather than once at connect: load() also runs when the daemon is replaced
    // on the bus, which is exactly the moment an upgrade changes the answer.
    let version = proxy.version().await;
    if let Err(error) = &version {
        tracing::debug!(%error, "the daemon did not report a version");
    }
    on(Update::Version(version_notice(version.as_deref().ok())));
```

- [ ] **Step 6: Show it**

In `crates/tidemark/src/window.rs`, add a `banner` field to the `MainWindow` struct alongside `message`:

```rust
    /// Says that the daemon is a different build from this window. Advisory only; the
    /// window keeps working against whatever answered.
    banner: adw::Banner,
```

In `present()`, build it just before the `ToolbarView` is created and add it as a second top bar, below the header:

```rust
        let banner = adw::Banner::new("");

        let view = adw::ToolbarView::builder().content(&stack).build();
        view.add_top_bar(&header);
        view.add_top_bar(&banner);
```

Add `banner` to the `Rc::new(Self { ... })` initialiser, and add the arm to `handle()`:

```rust
            Update::Version(notice) => match notice {
                Some(text) => {
                    self.banner.set_title(&text);
                    self.banner.set_revealed(true);
                }
                None => self.banner.set_revealed(false),
            },
```

- [ ] **Step 7: Verify the whole crate builds and the checks pass**

Run:

```bash
cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && scripts/check-layering.sh
```

Expected: all pass.

- [ ] **Step 8: Look at it**

Per the repository's screenshot loop, run the GUI under Xvfb with `GDK_BACKEND=x11` and confirm the banner is absent against a matching daemon. Then confirm it appears by temporarily returning a fixed wrong string from `version_notice`'s caller — revert that immediately after looking.

- [ ] **Step 9: Commit**

```bash
git add crates/tidemark/src/bus.rs crates/tidemark/src/window.rs
git commit -m "tidemark: say so when the daemon is a different build

A package upgrade restarts the daemon and cannot restart a GUI living in
the user's tray, so the two can diverge. The banner is advisory: the client
keeps talking to whatever answered, because a stale banner is a smaller
harm than a program that will not start.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

## Task 6: Prove the upgrade restarts the daemon

**Files:**
- Create: `scripts/test-package-upgrade.sh`

**Interfaces:**
- Consumes: the packages from Tasks 2 and 3 and the maintainer scripts from Task 4.
- Produces: a recorded result for the `PLAN.md` log in Task 8. No CI trigger, ever.

**Background the implementer needs.** This is the whole point of the step, and it is run **by hand, once**. It gets no `push` trigger, no tag trigger, and no `workflow_dispatch`. It belongs to the same family as `scripts/test-restart-user-daemon.sh` and `scripts/check-desktop-integration.sh`: reproducible checks a person runs.

The assertion is semantic, not a proxy. After the upgrade, the daemon's own `Version` property, read over the user's session bus, must report the **new** version. A changed PID would only show that something restarted; the property shows that the thing now running is the new code. That is why the script needs two packages built at two different versions — only the version string differs, so the second build is a relink, not a rebuild.

Docker and Podman are both installed on the development machine; the script uses `docker`.

- [ ] **Step 1: Build the two package pairs**

Run:

```bash
set -e
mkdir -p /tmp/tidemark-upgrade && rm -f /tmp/tidemark-upgrade/*
cargo build --release --locked --workspace
cargo deb --no-build -p tidemark && cargo generate-rpm -p crates/tidemark
cp target/debian/*.deb target/generate-rpm/*.rpm /tmp/tidemark-upgrade/

# Only the version string changes, so this is a relink rather than a rebuild.
sed -i 's/^version = "0.1.0"$/version = "0.1.1"/' Cargo.toml
cargo build --release --locked --workspace
cargo deb --no-build -p tidemark && cargo generate-rpm -p crates/tidemark
cp target/debian/*.deb target/generate-rpm/*.rpm /tmp/tidemark-upgrade/
git checkout Cargo.toml Cargo.lock
ls -1 /tmp/tidemark-upgrade/
```

Expected: four files — two `.deb` and two `.rpm`, at `0.1.0` and `0.1.1`.

- [ ] **Step 2: Write the script**

Create `scripts/test-package-upgrade.sh`:

```sh
#!/bin/sh
set -eu

# Proves that installing a newer package leaves the *new* tidemarkd running, in both
# package formats, against a real systemd and a real package transaction.
#
# Run by hand. This has no GitHub Actions trigger on purpose: it needs systemd as PID 1 in
# a privileged container, and the thing it guards changes about once a release. See
# docs/superpowers/specs/2026-08-22-ci-release-packaging-design.md.
#
#   scripts/test-package-upgrade.sh /tmp/tidemark-upgrade
#
# The directory must hold exactly two .deb and two .rpm files, an older and a newer.
#
# The assertion is the daemon's own Version property over the user's session bus, not a
# changed PID: a changed PID says something restarted, the property says the thing now
# running is the new code.

packages=${1:?usage: test-package-upgrade.sh <directory of packages>}
packages=$(CDPATH='' cd -- "$packages" && pwd)

old_version=0.1.0
new_version=0.1.1

run_case() {
    image=$1
    install_old=$2
    install_new=$3
    setup=$4

    printf '\n=== %s ===\n' "$image"

    container=$(docker run -d --rm --privileged --cgroupns=host \
        -v /sys/fs/cgroup:/sys/fs/cgroup:rw \
        -v "$packages":/packages:ro \
        "$image" /usr/sbin/init)
    # shellcheck disable=SC2064
    trap "docker rm -f $container >/dev/null 2>&1 || true" EXIT

    # systemd needs a moment to reach a state where it will accept units.
    docker exec "$container" sh -c \
        'for _ in $(seq 60); do systemctl is-system-running --wait >/dev/null 2>&1 && break; sleep 1; done' \
        || true

    docker exec "$container" sh -c "$setup"

    # Lingering gives tester a user manager without a login session, which is what makes
    # `systemctl --user --machine=tester@.host` reachable from the root transaction.
    docker exec "$container" useradd -m tester
    docker exec "$container" loginctl enable-linger tester
    docker exec "$container" sh -c \
        'for _ in $(seq 60); do systemctl is-active user@"$(id -u tester)".service >/dev/null 2>&1 && break; sleep 1; done'

    docker exec "$container" sh -c "$install_old"
    docker exec -u tester "$container" env "XDG_RUNTIME_DIR=/run/user/$(docker exec "$container" id -u tester)" \
        systemctl --user start tidemarkd.service

    reported=$(read_version "$container")
    [ "$reported" = "$old_version" ] || {
        printf 'before the upgrade the daemon reported %s, expected %s\n' "$reported" "$old_version" >&2
        exit 1
    }
    printf 'before: %s\n' "$reported"

    docker exec "$container" sh -c "$install_new"

    # try-restart is asynchronous; give the unit a moment to come back up.
    sleep 3
    reported=$(read_version "$container")
    [ "$reported" = "$new_version" ] || {
        printf 'after the upgrade the daemon reported %s, expected %s\n' "$reported" "$new_version" >&2
        printf 'the package replaced the files but left the old daemon running\n' >&2
        exit 1
    }
    printf 'after:  %s\n' "$reported"

    trap - EXIT
    docker rm -f "$container" >/dev/null
}

read_version() {
    container=$1
    uid=$(docker exec "$container" id -u tester)
    docker exec -u tester "$container" env "XDG_RUNTIME_DIR=/run/user/$uid" \
        busctl --user get-property \
            io.github.zbndev.Tidemark.Daemon \
            /io/github/zbndev/Tidemark \
            io.github.zbndev.Tidemark.Daemon1 \
            Version \
        | sed 's/^s "//; s/"$//'
}

run_case ubuntu:26.04 \
    "apt-get update >/dev/null && apt-get install -y /packages/tidemark_${old_version}-1_amd64.deb" \
    "apt-get install -y --allow-downgrades /packages/tidemark_${new_version}-1_amd64.deb" \
    'apt-get update >/dev/null && apt-get install -y systemd dbus-user-session'

run_case fedora:44 \
    "dnf install -y /packages/tidemark-${old_version}-1.x86_64.rpm" \
    "dnf upgrade -y /packages/tidemark-${new_version}-1.x86_64.rpm" \
    'dnf install -y systemd dbus-daemon'

printf '\nboth formats restart the daemon on upgrade\n'
```

- [ ] **Step 3: Make it executable and shellcheck it**

Run:

```bash
chmod +x scripts/test-package-upgrade.sh && shellcheck scripts/test-package-upgrade.sh
```

Expected: no findings.

- [ ] **Step 4: Run it, for real**

Run: `scripts/test-package-upgrade.sh /tmp/tidemark-upgrade`
Expected: `before: 0.1.0` / `after: 0.1.1` for both images, then `both formats restart the daemon on upgrade`.

This step is where the whole task can turn out to need work. Likely snags, and what they mean:
- The container never reaches `is-system-running`. Check that the host is on cgroup v2 (`stat -fc %T /sys/fs/cgroup` says `cgroup2fs`).
- `busctl --user` cannot reach a bus. `XDG_RUNTIME_DIR` must be `/run/user/<uid>` and the `user@.service` must be active; the waits above cover both, but raise the timeouts before concluding anything.
- The version does not change after the upgrade. **That is the bug this step exists to find.** Do not adjust the assertion. Check that the maintainer script fired at all (`journalctl` in the container, or add `set -x` to the script), then that `restart-user-daemon` found the user (`loginctl list-users`).

- [ ] **Step 5: Add the negative case**

Extend `run_case` to also assert that a *fresh* install leaves the daemon stopped — `try-restart` must not start anything — and that D-Bus activation then starts it on demand. Insert after the old package is installed, before the explicit `systemctl --user start`:

```sh
    docker exec -u tester "$container" env "XDG_RUNTIME_DIR=/run/user/$(docker exec "$container" id -u tester)" \
        sh -c 'systemctl --user is-active --quiet tidemarkd.service' \
        && { printf 'a fresh install left the daemon running; try-restart should start nothing\n' >&2; exit 1; }
```

Run the script again. Expected: still passes.

- [ ] **Step 6: Commit**

```bash
git add scripts/test-package-upgrade.sh
git commit -m "scripts: prove a package upgrade restarts the user daemon

Run by hand, in Docker, with systemd as PID 1 — deliberately no CI
trigger. The assertion is the daemon's own Version property over the
user's session bus rather than a changed PID: a changed PID says
something restarted, the property says the new code is what is running.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

## Task 7: Release workflow

**Files:**
- Create: `scripts/check-tag-version.sh`
- Create: `.github/workflows/release.yml`

**Interfaces:**
- Consumes: the metadata from Tasks 2 and 3, the check steps from Task 1.
- Produces: a draft GitHub Release carrying both packages.

**Background the implementer needs.** "A failed or partial build must not publish a release" is held structurally: the release job `needs:` all three of the others, so any failure means no release object is created at all. The release is a **draft** — a human publishes it.

The `.rpm` job is the only one using a container, because GitHub has no Fedora-hosted runner (`actions/runner-images` issue 2307, open since 2020) and does not plan one. Inside a `fedora:44` container the job runs as root with no `sudo`, and `actions/checkout` needs `git` present before it runs, so the container gets a bootstrap step.

- [ ] **Step 1: Write the tag-version check**

Create `scripts/check-tag-version.sh`:

```sh
#!/bin/sh
set -eu

# Refuses a tag that disagrees with the workspace manifest, so a release can never carry
# a version nobody bumped.
#
#   scripts/check-tag-version.sh v0.1.0

tag=${1:?usage: check-tag-version.sh <tag>}
project_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)

manifest=$(sed -n '/^\[workspace\.package\]/,/^\[/ s/^version = "\(.*\)"$/\1/p' \
    "$project_root/Cargo.toml")

[ -n "$manifest" ] || {
    printf 'no version found under [workspace.package] in Cargo.toml\n' >&2
    exit 1
}

case "$tag" in
    "v$manifest") printf 'tag %s agrees with Cargo.toml\n' "$tag" ;;
    *)
        printf 'tag %s disagrees with Cargo.toml, which says %s\n' "$tag" "$manifest" >&2
        exit 1
        ;;
esac
```

- [ ] **Step 2: Test it both ways**

Run:

```bash
chmod +x scripts/check-tag-version.sh
shellcheck scripts/check-tag-version.sh
scripts/check-tag-version.sh v0.1.0
scripts/check-tag-version.sh v9.9.9 && echo 'BUG: accepted a wrong tag' || echo 'correctly refused'
```

Expected: `tag v0.1.0 agrees with Cargo.toml`, then `correctly refused`.

- [ ] **Step 3: Write the release workflow**

Create `.github/workflows/release.yml`:

```yaml
name: Release

on:
  push:
    tags: ['v*']

permissions:
  contents: write

env:
  CARGO_TERM_COLOR: always

jobs:
  # Nothing is built until the tree passes the same checks a pull request does, and until
  # the tag agrees with the manifest.
  checks:
    runs-on: ubuntu-26.04
    steps:
      - uses: actions/checkout@v5

      - name: Install the toolkit and the shell-check tools
        run: |
          sudo apt-get update
          sudo apt-get install --no-install-recommends -y \
            libgtk-4-dev libadwaita-1-dev libsqlite3-dev pkg-config \
            dbus-daemon desktop-file-utils appstream shellcheck

      - name: The tag must agree with Cargo.toml
        run: scripts/check-tag-version.sh "${GITHUB_REF_NAME}"

      - uses: Swatinem/rust-cache@v2

      - run: cargo fmt --all --check
      - run: cargo clippy --workspace --all-targets -- -D warnings
      - run: dbus-run-session -- cargo test --workspace
      - run: scripts/check-layering.sh
      - run: scripts/check-desktop-integration.sh
      - run: scripts/test-restart-user-daemon.sh
      - run: |
          shellcheck scripts/*.sh data/restart-user-daemon \
            data/packaging/deb/postinst data/packaging/rpm/post-install.sh

  deb:
    needs: checks
    # Ubuntu 26.04 is the oldest release meeting the GTK 4.22 floor, so building on it
    # makes it the oldest supported target. glibc is forward- but not backward-compatible,
    # which is the whole reason each format is built on its own target.
    runs-on: ubuntu-26.04
    steps:
      - uses: actions/checkout@v5
      - name: Install the toolkit
        run: |
          sudo apt-get update
          sudo apt-get install --no-install-recommends -y \
            libgtk-4-dev libadwaita-1-dev libsqlite3-dev pkg-config
      - uses: Swatinem/rust-cache@v2
      - run: cargo install cargo-deb --locked
      - run: cargo build --release --locked --workspace
      - run: cargo deb --no-build -p tidemark
      - name: Show the dependencies the ELF produced
        run: dpkg-deb -I target/debian/*.deb
      - uses: actions/upload-artifact@v4
        with:
          name: deb
          path: target/debian/*.deb
          if-no-files-found: error

  rpm:
    needs: checks
    # GitHub has no Fedora-hosted runner and does not plan one (actions/runner-images
    # issue 2307). A Fedora container on an Ubuntu runner is the standard route; the only
    # alternative is a self-hosted machine. This is the one place Docker appears.
    runs-on: ubuntu-latest
    container: fedora:44
    steps:
      - name: Bootstrap the container
        run: |
          dnf install -y git gcc gtk4-devel libadwaita-devel sqlite-devel \
            pkgconf-pkg-config rpm-build
      - uses: actions/checkout@v5
      - name: Install rustup
        run: |
          curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
            | sh -s -- -y --default-toolchain none
          echo "$HOME/.cargo/bin" >> "$GITHUB_PATH"
      - uses: Swatinem/rust-cache@v2
      - run: cargo install cargo-generate-rpm --locked
      - run: cargo build --release --locked --workspace
      - run: cargo generate-rpm -p crates/tidemark
      - name: Show the requires find-requires produced
        run: rpm -qRp target/generate-rpm/*.rpm
      - uses: actions/upload-artifact@v4
        with:
          name: rpm
          path: target/generate-rpm/*.rpm
          if-no-files-found: error

  # Depends on all three, so a failure anywhere means no release object is created at all.
  # A draft, not a publication: a person decides when it goes out.
  release:
    needs: [checks, deb, rpm]
    runs-on: ubuntu-latest
    steps:
      - uses: actions/download-artifact@v5
        with:
          path: artifacts
          merge-multiple: true
      - run: ls -l artifacts
      - uses: softprops/action-gh-release@v2
        with:
          draft: true
          files: artifacts/*
          fail_on_unmatched_files: true
```

- [ ] **Step 4: Check both workflows parse**

Run:

```bash
python3 -c "import yaml; [yaml.safe_load(open(f)) for f in ('.github/workflows/ci.yml','.github/workflows/release.yml')]; print('yaml ok')"
```

Expected: `yaml ok`.

- [ ] **Step 5: Commit and exercise it on a throwaway tag**

```bash
git add .github/workflows/release.yml scripts/check-tag-version.sh
git commit -m "ci: build both packages into a draft release on a version tag

The release job needs all three others, so a partial build creates no
release object at all. The rpm job is the only container in the repository,
because GitHub has no Fedora-hosted runner.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
git push
git tag v0.1.0 && git push origin v0.1.0
gh run watch
```

Expected: four jobs green, a draft release carrying one `.deb` and one `.rpm`.

- [ ] **Step 6: Read the Fedora dependency list**

Open the `Show the requires find-requires produced` step's log.
Expected: real Fedora sonames — `libgtk-4.so.1()(64bit)`, `libadwaita-1.so.0()(64bit)`, `libsqlite3.so.0()(64bit)` — plus the two hand-added `dbus-common` and `hicolor-icon-theme`. If `find-requires` produced nothing, `auto-req` is misconfigured; fix it here rather than shipping a package that declares no dependencies.

- [ ] **Step 7: Clean up the throwaway tag**

Once the draft looks right, delete the draft release and the tag:

```bash
gh release delete v0.1.0 --yes
git push --delete origin v0.1.0 && git tag -d v0.1.0
```

The real tag is pushed when the owner decides to release.

---

## Task 8: Documentation

**Files:**
- Modify: `CONTEXT.md`
- Modify: `README.md`
- Modify: `PLAN.md` (not tracked in git)

**Interfaces:**
- Consumes: the results recorded during Tasks 2, 6 and 7.
- Produces: nothing other tasks read.

- [ ] **Step 1: Replace `CONTEXT.md` § Packaging**

The section currently reads:

> `deb`, `rpm`, `PKGBUILD`. Distribution artwork policies can refuse third-party trademarks, so a build with no provider marks stays a supported configuration: a card without one is a state the interface already has. Targets need GTK4 ≥ 4.18, which rules out Ubuntu 24.04 as a target — though its glibc, being the oldest, makes it a candidate build host, since glibc is forward- but not backward-compatible.

The last sentence contradicts § API floor in the same document, which sets 4.22. Replace the whole section with:

```markdown
## Packaging

`deb`, `rpm`, `PKGBUILD`. Distribution artwork policies can refuse third-party trademarks,
so a build with no provider marks stays a supported configuration: a card without one is a
state the interface already has.

The GTK 4.22 / libadwaita 1.9 floor above is GNOME 50, which became the default in exactly
two places: **Fedora 44** and **Ubuntu 26.04 LTS**. So the `rpm` targets Fedora 44+ and the
`deb` targets Ubuntu 26.04+; nothing older qualifies, and Debian's trixie at GTK 4.18 does
not.

glibc is forward- but not backward-compatible, so a build host must be no newer than the
oldest target. That is settled by construction rather than by choosing a host: each format
is built on the oldest release of its own target — the `deb` on the `ubuntu-26.04` runner,
the `rpm` in a `fedora:44` container, because GitHub hosts no Fedora runner. There is no
cross-distribution glibc question left to get wrong.
```

- [ ] **Step 2: Add an installation section to `README.md`**

Place it after whatever the README's opening section is, matching the surrounding style:

```markdown
## Installing

Download the `.deb` or `.rpm` from the [latest release](https://github.com/zbndev/tidemark/releases/latest).

    sudo apt install ./tidemark_0.1.0-1_amd64.deb     # Ubuntu 26.04 or newer
    sudo dnf install ./tidemark-0.1.0-1.x86_64.rpm    # Fedora 44 or newer

Tidemark is built against GTK 4.22 and libadwaita 1.9, which is GNOME 50. Older releases
cannot run it, and that is deliberate: the interface follows the toolkit rather than the
other way round. See `CONTEXT.md` § API floor.

Upgrading restarts the daemon for you. The interface does not restart itself — if it is
open in your tray during an upgrade it says so and asks you to restart it.

On Arch, build from the working tree with `makepkg -si`.
```

- [ ] **Step 3: Mark Step 17 done in `PLAN.md` and write the log entry**

Set Step 17's `**State:**` to `done`, and add a dated entry at the top of the `## Log` section following the style of the entries already there — prose, specific, recording what was *measured* rather than what was intended. It must carry:

- the target set and why it is only two distributions;
- whether thin LTO linked without `PKGBUILD`'s `!lto` (Task 2, Step 5);
- what the generated `Depends:` and `Requires:` actually said, and whether runner contamination forced the `.deb` into a container (Task 2 Step 6, Task 7 Step 6);
- the result of the upgrade run, both formats, with the before and after versions (Task 6);
- that `scripts/test-package-upgrade.sh` has no CI trigger and what that leaves uncovered — CI checks `restart-user-daemon` itself but not that the packages call it.

- [ ] **Step 4: Confirm nothing else in the tree still claims the old floor**

Run:

```bash
grep -rn "4\.18\|24\.04" --include="*.md" --include="*.toml" --include="*.yml" . | grep -v target/
```

Expected: no hits claiming 4.18 is the floor or 24.04 is relevant. Fix any that remain.

- [ ] **Step 5: Commit**

```bash
git add CONTEXT.md README.md
git commit -m "docs: record the real packaging target set

CONTEXT.md's packaging section claimed a GTK 4.18 floor and discussed
Ubuntu 24.04 as a build host, contradicting the 4.22 floor in the same
document. The floor is GNOME 50, which is Fedora 44 and Ubuntu 26.04 and
nothing else.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

## Self-Review Notes

Checked against the spec, section by section.

- **Target Set** → Task 8 Step 1.
- **Build Environments** → Tasks 1, 7. Runner contamination check → Task 2 Step 6.
- **Packaging** → Tasks 2, 3. The `!lto` open question → Task 2 Step 5, with the fallback written out.
- **Upgrade Restart** → Task 4, including the `systemd-units` prohibition and both ordering facts.
- **Version Mismatch in the Client** → Task 5.
- **Workflows** → Tasks 1, 7.
- **Test Strategy** → Task 6 (the manual proof, its assertion and its negative case), Task 5 Steps 1–4 (the banner's unit tests), Task 1 (the stub restart test staying in CI).
- **Documentation** → Task 8.

Names used across tasks and checked for consistency: `CLIENT_VERSION`, `version_notice`, `Update::Version`, `data/packaging/message.txt` installed as `/usr/share/doc/tidemark/first-run.txt`, `/usr/lib/tidemark/restart-user-daemon`, `scripts/check-tag-version.sh`, `scripts/test-package-upgrade.sh`.

Ordering dependency worth naming: Task 4 Step 2 edits asset arrays that Tasks 2 and 3 create, so Task 4 must not run before both. Task 6 needs Tasks 2, 3 and 4 complete. Task 5 is independent of the packaging tasks and can be done at any point after Task 1.
