# Release version bump script design

## Goal

One command turns the state of `main` into a pushed release tag: bump every version-carrying
file, commit, tag, push. The release workflow already refuses a tag that disagrees with the
workspace manifest (`scripts/check-tag-version.sh`); the script makes that refusal unreachable
by construction and extends the same discipline to two files no CI job gates: the AppStream
release list and the PKGBUILD.

## Interface and shape

`scripts/release.sh X.X.X` — POSIX sh with `set -eu`, styled after the sibling check scripts:
short comments say why, `project_root` is computed from the script's location, and the only
tools used are git, cargo, sed and `date`. `release.yml` already shellchecks every
`scripts/*.sh`, so
the new script is linted in CI for free.

## Preconditions

Every check runs before any file is edited:

- Exactly one argument, a canonical `X.X.X`: three dot-separated groups of ASCII digits with
  no leading zeroes — the same rules `check-tag-version.sh` and `tidemarkd`'s update checker
  enforce, so the script never accepts a version the rest of the system would reject.
- The current branch is `main`.
- The working tree is clean: `git status --porcelain` is empty. `.gitignore` already hides
  makepkg output and local scratch, so leftover artifacts do not block a release.
- After `git fetch origin main`, local `main` equals `origin/main`. A release is never cut
  from stale `main`.
- The requested version is strictly greater, component-wise, than the current
  `[workspace.package]` version — typo and downgrade protection.
- Tag `vX.X.X` exists neither locally nor on the remote.

## Edits

Six files, in this order:

1. Root `Cargo.toml`: `version` under `[workspace.package]`, edited with the same sed section
   range `check-tag-version.sh` uses to read it.
2. `crates/tidemark-core/Cargo.toml`: the `version` requirement of the `tidemark-types` path
   dependency. `^0.1.0` does not match `0.2.0`, so the build breaks unless this moves with the
   workspace.
3. `crates/tidemarkd/Cargo.toml`: the `version` requirement of the `tidemark-core` path
   dependency, for the same reason.
4. `Cargo.lock`: `cargo update --workspace`, which rewrites only workspace members' entries
   and leaves external dependencies alone. Required because `release.yml` builds with
   `--locked`, which fails on a stale lockfile.
5. `data/metainfo/io.github.zbndev.Tidemark.metainfo.xml`: a new
   `<release version="X.X.X" date="<today>" />` inserted as the first — newest — entry of
   `<releases>`. Release notes prose stays human work, added by hand when there is something
   to say.
6. `PKGBUILD`: `pkgver=X.X.X` and `pkgrel=1`. The PKGBUILD's own header comment already says
   the version comes from the workspace manifest and is bumped alongside it.

## Pre-commit verification

- `scripts/check-tag-version.sh "vX.X.X"` must agree — proving, before anything is committed,
  the exact contract the tag push is about to be judged by in CI.
- `appstreamcli validate --pedantic --no-net` on the metainfo when the binary is installed
  locally. Best effort: CI validates it regardless.

## Commit, tag, push

An explicit `git add` of the six files — never `-A`. One commit, `chore: bump to vX.X.X`;
one annotated tag, `vX.X.X`; one push of both refs, `git push origin main vX.X.X`. A failed
push prints the recovery hint instead of failing silently: the commit and tag are already
local, push them manually.

## Failure behaviour

`set -eu` aborts on the first failed step. Before the commit this deliberately leaves the
edits in the working tree — inspectable, and revertible with `git checkout -- .`, a hint the
script prints. No automatic rollback: a half-executed release is something to look at, not
to hide.

## Non-goals

The script runs no fmt, clippy or tests — the tag push triggers `release.yml`, which runs the
full suite before building anything. It generates no changelog and publishes no GitHub
release; `release.yml` drafts one. There is no dry-run mode.

## Testing

shellcheck locally and in CI. A smoke test drives the script against a throwaway git fixture
seeded with copies of the six files: every precondition rejection, the happy path up to but
not including the push, and the content of the resulting edits.
