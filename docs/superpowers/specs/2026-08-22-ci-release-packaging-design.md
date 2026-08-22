# CI and Release Packaging Design

- Status: approved
- Date: 2026-08-22
- Implements: implementation step 17

## Purpose

GitHub Actions verifies every change against the toolkit floor the interface actually
uses, and a version tag turns that same tree into a `.deb` and an `.rpm` published as a
draft GitHub Release. A user who upgrades the package must end up running the new daemon,
not the new files beside a daemon still executing the old binary.

All documentation, source code, code comments, tests, logs, and interface copy are
written in English.

## Goals

- Run `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace` and the repository's shell checks on pull requests and on
  pushes to the default branch, in an environment that meets the GTK 4.22 / libadwaita 1.9
  floor rather than one that quietly lowers it.
- On a `v*` tag, build one `.deb` and one `.rpm` from that tag and attach both to a draft
  release, after confirming the tag agrees with the workspace version.
- Make a package upgrade restart the user's `tidemarkd.service`, through the existing
  `data/restart-user-daemon`, in both formats.
- Prove that once, against a real systemd and a real package transaction.
- Let the GUI notice that it is talking to a daemon of a different version, since no
  package script can restart a program living in the user's tray.

## Non-goals

- Flatpak. It remains the answer if reach ever matters more than the API floor does, and
  it is not this step.
- Changes to `PKGBUILD`. It stays the working-tree package from Step 5a: for installing
  the build currently under development, not an artifact of the release workflow.
- `lintian` / `rpmlint` gates. Worth adding later; nothing here depends on them.
- Debian. Trixie ships GTK 4.18 and forky has not released.
- Publishing a release automatically. The workflow creates a draft; a human publishes it.

## Target Set

The API floor is GTK **4.22** and libadwaita **1.9** (`CONTEXT.md` § API floor), which is
GNOME 50. GNOME 50 released in March 2026 and became the default in exactly two places:
**Fedora 44** and **Ubuntu 26.04 LTS**. So the `.rpm` targets Fedora 44+ and the `.deb`
targets Ubuntu 26.04+. Nothing else qualifies today.

`CONTEXT.md` § Packaging currently reads "Targets need GTK4 ≥ 4.18, which rules out Ubuntu
24.04 as a target — though its glibc, being the oldest, makes it a candidate build host".
That is stale against § API floor of the same document and is replaced by the paragraph
above.

The plan's glibc trap — the build host must be no newer than the oldest target, because
glibc is forward- but not backward-compatible — is resolved by construction rather than by
picking a host: each format is built on the oldest release of its own target, so there is
no cross-distribution glibc question to get wrong.

## Build Environments

| Job | Runs on | Why |
|---|---|---|
| Checks | `ubuntu-26.04` runner | Real GTK 4.22 / libadwaita 1.9 from `apt`. No container layer. |
| `.deb` | `ubuntu-26.04` runner | Oldest supported Ubuntu target; builds natively on it. |
| `.rpm` | `ubuntu-latest` runner, `container: fedora:44` | GitHub has no Fedora-hosted runner and does not plan one (`actions/runner-images` issue 2307). A Fedora container on an Ubuntu runner is the standard route; the alternative is a self-hosted machine. |

Docker therefore appears in one job, because there it is the only option, and nowhere
else.

Rust comes from `rustup` honouring `rust-toolchain.toml` in every job, not from the
distribution: the floor is 1.92 (imposed by gtk4-rs 0.11) and neither target's packaged
toolchain is a promise worth depending on.

Two risks are accepted rather than mitigated. `ubuntu-26.04` entered public preview in
June 2026 and may still carry preview instability and queueing; there is no alternative,
because 24.04 carries GTK 4.18 and this code does not compile against it. And the runner
image is not a clean Ubuntu — `dpkg-shlibdeps` resolves against *installed* packages, so a
loaded image can widen `Depends:` beyond a default installation. The names stay real
Ubuntu package names either way, so the package does not break; the generated `Depends:`
line is read by eye after the first build, and only if it carries obvious runner
contamination does the `.deb` move into a clean `ubuntu:26.04` container too.

## Packaging

`cargo-deb` and `cargo-generate-rpm`, configured under `[package.metadata.deb]` and
`[package.metadata.generate-rpm]` in `crates/tidemark/Cargo.toml`.

They are chosen over a single-manifest tool (`nfpm`) for one reason: both derive
dependencies from the built ELF — `cargo-deb` runs `dpkg-shlibdeps`, `cargo-generate-rpm`
uses rpm's `find-requires`. Debian and Fedora name GTK, libadwaita and SQLite differently,
so a single hand-written dependency list is only superficially single, and it is the line
that rots silently. Native `debian/` plus `.spec` would be more idiomatic still and is
more machinery than this stage of the project earns.

Both run after one `cargo build --release --locked --workspace`, with `--no-build`, so a
single package carries binaries from two crates even though the `tidemark` crate owns the
metadata.

The asset list mirrors `package()` in `PKGBUILD` file for file, with the same modes: both
binaries, the user unit, the nine hicolor PNG sizes plus the `512x512@2` variant, the
desktop and autostart entries, the metainfo XML, the D-Bus service file,
`restart-user-daemon`, the provider mark SVGs, `TRADEMARKS.md`, `LICENSE`, `README.md`. A
rebuild that drops the provider marks and `TRADEMARKS.md` stays a supported configuration,
for the reason `PKGBUILD` already records: distribution artwork policies can refuse
third-party trademarks, and a card without a mark is a state the interface already has.

`/etc/xdg/autostart/io.github.zbndev.Tidemark.desktop` is declared a conffile in the
`.deb` and `%config(noreplace)` in the `.rpm`. It lives under `/etc`, and an upgrade must
not discard an edit.

Dependencies not discoverable from the ELF are added by hand, and only those: the D-Bus
daemon (zbus is pure Rust, so `libdbus` is never linked and `find-requires` cannot see it)
and `hicolor-icon-theme`.

`options=(!lto)` in `PKGBUILD` is not carried over. It exists because makepkg exports its
own `CFLAGS`, which makes the `cc` crate compile `aws-lc-sys` to GCC LTO bitcode that
`rust-lld` then cannot link. Neither `cargo-deb` nor `cargo-generate-rpm` injects `CFLAGS`,
so the workspace's own `lto = "thin"` should apply and link. That is an expectation, not a
measurement, and the implementation plan carries it as an explicit thing to verify on the
first build rather than as a settled fact. `[profile.release]` already sets
`strip = "symbols"`, so neither tool strips again and no debug package is produced.

## Upgrade Restart

There is one source of truth, `data/restart-user-daemon`, already installed to
`/usr/lib/tidemark/restart-user-daemon` and already exercised by
`scripts/test-restart-user-daemon.sh`. Both formats call it; neither grows restart logic
of its own.

- `.deb` `postinst`: action `configure` with a non-empty `$2` is an upgrade and calls the
  script. An empty `$2` is a fresh install and prints the guidance text `tidemark.install`
  already carries.
- `.rpm` `%post`: `$1 -ge 2` is an upgrade and calls the script; `$1 -eq 1` prints the
  same text.

`cargo-deb`'s `systemd-units` feature is deliberately unused. It generates
`dh_installsystemd`-shaped code for the **system** scope, and `tidemarkd.service` is a
**user** unit: it would issue `daemon-reload` and `enable` against root's manager, which is
precisely the failure the `--machine=<user>@.host` transport inside `restart-user-daemon`
exists to avoid.

Two ordering facts are recorded so they are not rediscovered. On an rpm upgrade the new
package's `%post` runs *before* the old package's `%postun`, so the restart lives only in
`%post` and no `%postun` restart logic will be added. On a dpkg upgrade `postinst
configure` runs after the new files are unpacked, which is the correct point.

`try-restart` stays, rather than `restart`: an inactive daemon is left inactive, and D-Bus
activation starts the new binary when Tidemark is next opened.

## Version Mismatch in the Client

The package restarts the daemon. Nothing can restart the GUI, which lives in the user's
tray and belongs to them, so an upgrade can leave a new daemon talking to an old front
end. `tidemarkd` already publishes a `Version` property
(`crates/tidemarkd/src/service.rs`), and no client has ever read it.

`crates/tidemark/src/bus.rs` reads it on connect and compares it against the client's own
`env!("CARGO_PKG_VERSION")`. On disagreement the window shows an `AdwBanner` telling the
user Tidemark was updated and should be restarted. The banner is advisory: the client does
not refuse to talk to a daemon of another version, because a stale banner is a smaller
harm than a program that will not start.

## Workflows

`.github/workflows/ci.yml` — pull requests and pushes to the default branch, on
`ubuntu-26.04`:

1. `rustup` per `rust-toolchain.toml`; `apt install libgtk-4-dev libadwaita-1-dev
   libsqlite3-dev desktop-file-utils appstream shellcheck`.
2. `cargo fmt --check`
3. `cargo clippy --workspace --all-targets -- -D warnings`
4. `dbus-run-session -- cargo test --workspace`
5. `scripts/check-layering.sh`, `scripts/check-desktop-integration.sh`,
   `scripts/test-restart-user-daemon.sh`, `shellcheck` over `scripts/` and
   `data/restart-user-daemon`.

`dbus-run-session` is load-bearing, not decoration. The tests in `tidemark-core::secrets`
*skip* rather than fail when no session bus is reachable, and `tidemarkd::service` opens a
session connection; without a bus, CI would report green over tests that never ran. With a
bus but no Secret Service the keyring assertions still skip, which is honest and matches
the design position that an absent or locked keyring is a state rather than a failure.

`.github/workflows/release.yml` — on a `v*` tag, four jobs:

1. **Checks** — the `ci.yml` steps, plus confirming the tag matches
   `workspace.package.version`.
2. **deb** — `ubuntu-26.04`, needs (1).
3. **rpm** — `container: fedora:44`, needs (1). Runs in parallel with (2).
4. **Release** — needs (1), (2) and (3); creates or updates the draft release and uploads
   both artifacts.

"A failed or partial build must not publish a release" holds structurally: the release job
depends on all three, so any failure means no release object is created at all.

Both workflows cache the cargo build per environment.

## Test Strategy

`scripts/test-package-upgrade.sh` is the proof that an upgrade restarts the daemon. It
lives in the repository and has **no GitHub Actions trigger** — not on push, not on a tag,
not `workflow_dispatch`. It is run by a person, in Docker, in the same genre as
`scripts/test-restart-user-daemon.sh` and `scripts/check-desktop-integration.sh`. It is
run once as part of this step and its result recorded in the `PLAN.md` log.

For each of `ubuntu:26.04` and `fedora:44` it starts the image with systemd as PID 1
(`--privileged --cgroupns=host -v /sys/fs/cgroup`), creates a user and enables lingering
so a real user manager exists, then:

1. Installs the package built at version N and starts `tidemarkd.service`.
2. Reads the daemon's `Version` property over the user's session bus with `busctl --user
   get-property`, and asserts it reads N.
3. Installs the package built at version N+1 over it.
4. Asserts the property now reads **N+1**.

Step 4 is the assertion the whole step exists for, stated semantically rather than as a
proxy: the running daemon is the new code. It is worth the one extra relink that building
a second version costs — only the version string changes, so dependencies stay cached.

It also asserts the negative: on a *fresh* install `try-restart` starts nothing, and the
unit remains D-Bus-activatable.

What this leaves uncovered is stated rather than hidden. CI keeps the cheap stub-based
`scripts/test-restart-user-daemon.sh`, which covers `restart-user-daemon` itself but not
the fact that the `.deb` and `.rpm` call it. That link is held by the single manual run and
by nothing else; if the maintainer scripts are later edited, whoever edits them re-runs the
script. This is the accepted cost of not running a systemd-in-Docker transaction on every
release.

The version-mismatch banner is covered by unit tests on the comparison and on the resulting
banner state. No live-bus test.

## Documentation

- `CONTEXT.md` § Packaging replaced with the target set above.
- `README.md` gains an installation section pointing at the release assets, naming Fedora
  44+ and Ubuntu 26.04+ and saying why nothing older qualifies.
- `PLAN.md` Step 17 marked done, with a log entry recording the result of the one upgrade
  run and whatever the first builds turn up about `!lto` and about the generated
  `Depends:`.

## Sequencing

1. `ci.yml` and the checks job. Get the toolkit floor building on `ubuntu-26.04` first;
   everything downstream assumes it.
2. `cargo-deb` metadata and the `.deb`; inspect the generated `Depends:` and confirm thin
   LTO links.
3. `cargo-generate-rpm` metadata and the `.rpm` in the Fedora container.
4. Maintainer scripts in both formats, calling `restart-user-daemon`.
5. `scripts/test-package-upgrade.sh`; run it once, both formats, record the result.
6. The `Version` comparison and banner in `crates/tidemark/src/bus.rs`.
7. `release.yml`, tag-version agreement, draft release.
8. Documentation.
