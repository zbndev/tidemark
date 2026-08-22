# Release Version Bump Script Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `scripts/release.sh X.X.X` turns the state of `main` into a pushed release tag by bumping every version-carrying file, committing, tagging and pushing, with all guards running before the first edit.

**Architecture:** One POSIX sh script in the house style of the sibling check scripts, plus one hermetic smoke-test script (`scripts/test-release.sh`) that drives it against a throwaway git fixture whose `origin` is a bare repository in a temp directory. Two workflow files gain one explicit test step each.

**Tech Stack:** POSIX sh (`#!/bin/sh`, `set -eu`), git, cargo, GNU sed, `date`. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-08-22-release-bump-script-design.md`

## Global Constraints

- Scripts are `#!/bin/sh` with `set -eu`, shellcheck-clean (CI runs `shellcheck scripts/*.sh` on both `ci.yml` and `release.yml`).
- GNU sed `-i` is acceptable: the project is Linux-only (systemd user units, makepkg).
- Comments are English, terse, and say why — the house style of `scripts/check-tag-version.sh`.
- The version is canonical: exactly three dot-separated ASCII-digit groups, no leading zeroes (a lone `0` is fine) — the same rules as `scripts/check-tag-version.sh` and `crates/tidemarkd/src/update.rs`.
- The commit message is exactly `chore: bump to vX.X.X`; the tag is annotated `vX.X.X`.
- Nothing but git, cargo, sed, `date` and (best-effort) `appstreamcli` is invoked.
- The smoke test never touches the real repository's files, remotes or tags.

## File Structure

- Create: `scripts/release.sh` — the whole release flow: guards, edits, verification, commit/tag/push.
- Create: `scripts/test-release.sh` — hermetic smoke test: fixture builder, rejection helpers, happy-path assertions.
- Modify: `.github/workflows/ci.yml` — one step running `scripts/test-release.sh`.
- Modify: `.github/workflows/release.yml` — one step running `scripts/test-release.sh`.

---

### Task 1: The version argument gate

**Files:**
- Create: `scripts/release.sh`
- Create: `scripts/test-release.sh`

**Interfaces:**
- Consumes: `scripts/check-tag-version.sh` (copied into the fixture; first used in Task 3).
- Produces: `canonical()` and `die()` in `scripts/release.sh` (used by later tasks);
  `make_fixture`, `reject`, `cleanup` in `scripts/test-release.sh` (used by later tasks).

- [ ] **Step 1: Write the failing smoke test**

Create `scripts/test-release.sh` with exactly this content, then `chmod +x scripts/test-release.sh`:

```sh
#!/bin/sh
set -eu

# Proves scripts/release.sh: every precondition rejection, and a full happy path against a
# throwaway bare repository playing origin — nothing here touches the real repository or
# the real origin.
#
#   scripts/test-release.sh

project_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)

current_version=$(sed -n '/^\[workspace\.package\]/,/^\[/ s/^version = "\(.*\)"$/\1/p' \
    "$project_root/Cargo.toml")
next_version=${current_version%.*}.$(( ${current_version##*.} + 1 ))

fixture=
cleanup() {
    [ -n "$fixture" ] && rm -rf "$fixture" "$fixture.origin.git"
    fixture=
}
trap cleanup EXIT

# A miniature copy of the version-carrying files plus the two scripts under test, with its
# own bare repository as origin, so even the push step of a happy path runs for real.
make_fixture() {
    fixture=$(mktemp -d "${TMPDIR:-/tmp}/tidemark-release-test.XXXXXX")

    mkdir -p "$fixture/scripts" "$fixture/data/metainfo" "$fixture/crates/tidemark-types/src" \
        "$fixture/crates/tidemark-core/src" "$fixture/crates/tidemarkd/src" \
        "$fixture/crates/tidemark/src"
    cp "$project_root/Cargo.toml" "$project_root/Cargo.lock" \
        "$project_root/rust-toolchain.toml" "$project_root/PKGBUILD" "$fixture/"
    cp "$project_root/crates/tidemark-types/Cargo.toml" "$fixture/crates/tidemark-types/"
    cp "$project_root/crates/tidemark-core/Cargo.toml" "$fixture/crates/tidemark-core/"
    cp "$project_root/crates/tidemarkd/Cargo.toml" "$fixture/crates/tidemarkd/"
    cp "$project_root/crates/tidemark/Cargo.toml" "$fixture/crates/tidemark/"
    cp "$project_root/data/metainfo/io.github.zbndev.Tidemark.metainfo.xml" \
        "$fixture/data/metainfo/"
    cp "$project_root/scripts/release.sh" "$project_root/scripts/check-tag-version.sh" \
        "$fixture/scripts/"
    # `cargo update` must be able to load the workspace, and target auto-discovery needs
    # these; empty stubs do — nothing ever compiles in the fixture.
    : >"$fixture/crates/tidemark-types/src/lib.rs"
    : >"$fixture/crates/tidemark-core/src/lib.rs"
    : >"$fixture/crates/tidemarkd/src/main.rs"
    : >"$fixture/crates/tidemark/src/main.rs"

    git -C "$fixture" init -q -b main
    git -C "$fixture" config user.name fixture
    git -C "$fixture" config user.email fixture@invalid
    git -C "$fixture" add -A
    git -C "$fixture" commit -q -m fixture

    git init -q --bare "$fixture.origin.git"
    git -C "$fixture" remote add origin "$fixture.origin.git"
    git -C "$fixture" push -q origin main
}

# Runs the fixture's release.sh expecting failure, and proving it changed nothing: the
# tree may already be dirty (the dirty-tree scenario dirties it on purpose), so the
# invariant is before/after equality, not emptiness.
reject() {
    before=$(git -C "$fixture" status --porcelain)
    if (cd "$fixture" && scripts/release.sh "$@" >/dev/null 2>&1); then
        printf 'expected scripts/release.sh %s to fail\n' "$*" >&2
        exit 1
    fi
    after=$(git -C "$fixture" status --porcelain)
    [ "$before" = "$after" ] || {
        printf 'scripts/release.sh %s failed but changed the tree\n' "$*" >&2
        exit 1
    }
}

printf 'rejecting malformed versions\n'
make_fixture
reject
reject 1.2 1.3
reject 1.2
reject 1.2.3.4
reject "v$next_version"
reject 1.2.3-beta
reject 01.2.3
reject 1.02.3
reject 1.2.03
reject 1..2
reject .1.2
cleanup

printf 'argument validation holds\n'
```

- [ ] **Step 2: Run it to verify it fails**

Run: `scripts/test-release.sh`
Expected: FAIL — `cp: cannot stat .../scripts/release.sh: No such file or directory` (the
fixture builder copies a script that does not exist yet).

- [ ] **Step 3: Write the minimal script**

Create `scripts/release.sh` with exactly this content, then `chmod +x scripts/release.sh`:

```sh
#!/bin/sh
set -eu

# Turns the state of main into a pushed release tag: bumps every version-carrying file,
# commits, tags, pushes. Every guard runs before the first edit; a failure between the
# first edit and the commit leaves the edits in the tree on purpose — inspect, then revert
# with `git checkout -- .`.
#
#   scripts/release.sh X.X.X
#
# No fmt, clippy or tests run here: the tag push triggers the Release workflow, which runs
# the whole suite before building anything.

project_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$project_root"

die() {
    printf 'release.sh: %s\n' "$1" >&2
    exit 1
}

# A canonical version: exactly three dot-separated groups of ASCII digits, no leading
# zeroes (a lone zero is fine) — the same form check-tag-version.sh and tidemarkd's update
# checker accept.
canonical() {
    case $1 in *[!0-9.]*) return 1 ;; esac

    old_ifs=$IFS
    IFS=.
    set -f
    set -- $1
    set +f
    IFS=$old_ifs
    [ $# -eq 3 ] || return 1

    for component in "$@"; do
        case $component in
            0 | [1-9] | [1-9][0-9]*) ;;
            *) return 1 ;;
        esac
    done
}

[ $# -eq 1 ] || die 'usage: scripts/release.sh X.X.X'
new=$1
canonical "$new" || die "$new is not a canonical version (X.X.X, no leading zeroes)"
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `scripts/test-release.sh`
Expected: PASS — prints `rejecting malformed versions` then `argument validation holds`,
exit 0.

- [ ] **Step 5: Commit**

```bash
git add scripts/release.sh scripts/test-release.sh
git commit -m "feat: validate the release script's version argument"
```

---

### Task 2: The precondition guards

**Files:**
- Modify: `scripts/release.sh` (append after the argument gate)
- Modify: `scripts/test-release.sh` (append before the final `printf`)

**Interfaces:**
- Consumes: `canonical()` from Task 1; `make_fixture`, `reject` from Task 1.
- Produces: `greater()` in `scripts/release.sh` (reused by Task 3's numeric-order case).

- [ ] **Step 1: Add the failing rejection scenarios**

In `scripts/test-release.sh`, replace the trailing `printf 'argument validation holds\n'`
line with:

```sh
printf 'rejecting a release cut outside main\n'
make_fixture
git -C "$fixture" switch -q -c side
reject "$next_version"
git -C "$fixture" switch -q main

printf 'rejecting a dirty tree\n'
echo stray >>"$fixture/PKGBUILD"
reject "$next_version"

printf 'rejecting stale main\n'
git -C "$fixture" commit -qam stale
reject "$next_version"
git -C "$fixture" push -q origin main

printf 'rejecting a version that is not newer\n'
reject "$current_version"

printf 'rejecting a tag that exists locally\n'
git -C "$fixture" tag "v$next_version"
reject "$next_version"

printf 'rejecting a tag that exists on origin only\n'
git -C "$fixture" push -q origin "v$next_version"
git -C "$fixture" tag -d "v$next_version" >/dev/null
reject "$next_version"
cleanup

printf 'precondition guards hold\n'
```

- [ ] **Step 2: Run it to verify the new scenarios fail**

Run: `scripts/test-release.sh`
Expected: FAIL — `expected scripts/release.sh <next_version> to fail` on the
outside-main scenario: the script has no branch guard yet, so it succeeds (and, having no
edits either, exits 0).

- [ ] **Step 3: Implement the guards**

Append to `scripts/release.sh`, after the `canonical "$new" || die ...` line:

```sh
# True when canonical $1 is strictly greater than canonical $2. Numeric, not
# lexicographic: 0.1.10 is newer than 0.1.9.
greater() {
    a_head=${1%%.*}
    b_head=${2%%.*}
    [ "$a_head" -lt "$b_head" ] && return 1
    [ "$a_head" -gt "$b_head" ] && return 0
    [ "$1" = "$a_head" ] && return 1
    greater "${1#*.}" "${2#*.}"
}

[ "$(git rev-parse --abbrev-ref HEAD)" = main ] || die 'releases are cut from main'

[ -z "$(git status --porcelain)" ] || die 'the working tree is not clean'

git fetch -q origin main
[ "$(git rev-parse main)" = "$(git rev-parse origin/main)" ] \
    || die 'main is not up to date with origin/main; merge or rebase first'

current=$(sed -n '/^\[workspace\.package\]/,/^\[/ s/^version = "\(.*\)"$/\1/p' Cargo.toml)
[ -n "$current" ] || die 'no version found under [workspace.package] in Cargo.toml'
canonical "$current" || die "the manifest version $current is not canonical"
greater "$new" "$current" || die "$new is not newer than the manifest's $current"

if git rev-parse -q --verify "refs/tags/v$new" >/dev/null; then
    die "tag v$new already exists locally"
fi
if git ls-remote --tags --exit-code origin "refs/tags/v$new" >/dev/null 2>&1; then
    die "tag v$new already exists on origin"
fi
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `scripts/test-release.sh`
Expected: PASS — all rejection scenarios run, ending with `precondition guards hold`.

- [ ] **Step 5: Commit**

```bash
git add scripts/release.sh scripts/test-release.sh
git commit -m "feat: guard the release script's preconditions"
```

---

### Task 3: Edits, verification, commit, tag, push

**Files:**
- Modify: `scripts/release.sh` (append after the tag guards)
- Modify: `scripts/test-release.sh` (replace the trailing `printf` line)

**Interfaces:**
- Consumes: `greater()` from Task 2; `scripts/check-tag-version.sh` via the fixture.
- Produces: the complete `scripts/release.sh`.

- [ ] **Step 1: Add the failing happy-path scenarios**

In `scripts/test-release.sh`, replace the trailing `printf 'precondition guards hold\n'`
line with:

```sh
metainfo=data/metainfo/io.github.zbndev.Tidemark.metainfo.xml

printf 'cutting a full release\n'
make_fixture
(cd "$fixture" && scripts/release.sh "$next_version") >/dev/null 2>&1

[ "$(sed -n '/^\[workspace\.package\]/,/^\[/ s/^version = "\(.*\)"$/\1/p' \
    "$fixture/Cargo.toml")" = "$next_version" ]
grep -q "^tidemark-types = { version = \"$next_version\", path = \"../tidemark-types\" }\$" \
    "$fixture/crates/tidemark-core/Cargo.toml"
grep -q "^tidemark-core = { version = \"$next_version\", path = \"../tidemark-core\" }\$" \
    "$fixture/crates/tidemarkd/Cargo.toml"
for crate in tidemark-types tidemark-core tidemarkd tidemark; do
    grep -A1 "^name = \"$crate\"$" "$fixture/Cargo.lock" \
        | grep -q "^version = \"$next_version\"\$"
done
release_line=$(sed -n '/^  <releases>$/{
n
p
}' "$fixture/$metainfo")
[ "$release_line" = "    <release version=\"$next_version\" date=\"$(date +%F)\" />" ]
grep -q "^pkgver=$next_version\$" "$fixture/PKGBUILD"
grep -q '^pkgrel=1$' "$fixture/PKGBUILD"

[ "$(git -C "$fixture" log -1 --format=%s)" = "chore: bump to v$next_version" ]
[ "$(git -C "$fixture" diff --name-only HEAD~1 HEAD | sort)" = "$(printf '%s\n' \
    Cargo.lock Cargo.toml PKGBUILD \
    crates/tidemark-core/Cargo.toml crates/tidemarkd/Cargo.toml \
    "$metainfo" | sort)" ]
[ "$(git -C "$fixture" for-each-ref "refs/tags/v$next_version" \
    --format='%(objecttype)')" = tag ]
[ "$(git --git-dir="$fixture.origin.git" rev-parse "v$next_version^{commit}")" \
    = "$(git -C "$fixture" rev-parse HEAD)" ]
cleanup

printf '0.1.10 is newer than 0.1.9, numerically\n'
make_fixture
sed -i "/^\[workspace\.package\]/,/^\[/ s/^version = \"\(.*\)\"\$/version = \"0.1.9\"/" \
    "$fixture/Cargo.toml"
git -C "$fixture" commit -qam 'fixture at 0.1.9'
git -C "$fixture" push -q origin main
(cd "$fixture" && scripts/release.sh 0.1.10) >/dev/null 2>&1
[ "$(git -C "$fixture" log -1 --format=%s)" = 'chore: bump to v0.1.10' ]
cleanup

printf 'the happy paths hold\n'
```

- [ ] **Step 2: Run it to verify the new scenarios fail**

Run: `scripts/test-release.sh`
Expected: FAIL — the first `[ ... = "$next_version" ]` manifest assertion fails: the
script has no edits yet, so the fixture's `Cargo.toml` still says the old version.

- [ ] **Step 3: Implement the rest of the script**

Append to `scripts/release.sh`, after the remote-tag guard, and add the trap after the
`cd "$project_root"` line:

After `cd "$project_root"` (Task 1's file), insert:

```sh
edited=
committed=
trap 'if [ -n "$edited" ] && [ -z "$committed" ]; then
        printf "release.sh: edits are still in the working tree; revert with: git checkout -- .\n" >&2
    fi' EXIT
```

After the remote-tag guard (Task 2's addition), append:

```sh
edited=1

# The one number everything else derives from.
sed -i "/^\[workspace\.package\]/,/^\[/ s/^version = \".*\"\$/version = \"$new\"/" Cargo.toml

# ^0.1.0 does not match 0.2.0: the inter-crate requirements move with the workspace or
# the build breaks.
sed -i '/^tidemark-types = { version = /s/version = "[^"]*"/version = "'"$new"'"/' \
    crates/tidemark-core/Cargo.toml
sed -i '/^tidemark-core = { version = /s/version = "[^"]*"/version = "'"$new"'"/' \
    crates/tidemarkd/Cargo.toml

# release.yml builds with --locked, so members' lock entries must move with the manifests.
# --offline: external dependencies are untouched, the registry is never consulted.
cargo update --workspace --offline --quiet

# AppStream wants the newest release first. Notes prose is human work, added by hand.
entry="    <release version=\"$new\" date=\"$(date +%F)\" />"
sed -i "/^  <releases>$/a\\$entry" \
    data/metainfo/io.github.zbndev.Tidemark.metainfo.xml

# The PKGBUILD's header comment says the version comes from the workspace manifest and is
# bumped alongside it.
sed -i "s/^pkgver=.*/pkgver=$new/; s/^pkgrel=.*/pkgrel=1/" PKGBUILD

# A sed that matched nothing becomes a loud failure here, not a broken release.
grep -q "^version = \"$new\"\$" Cargo.toml
grep -q "^tidemark-types = { version = \"$new\"" crates/tidemark-core/Cargo.toml
grep -q "^tidemark-core = { version = \"$new\"" crates/tidemarkd/Cargo.toml
grep -q "<release version=\"$new\" date=" data/metainfo/io.github.zbndev.Tidemark.metainfo.xml
grep -q "^pkgver=$new\$" PKGBUILD

# The exact contract the tag push is about to be judged by in CI.
scripts/check-tag-version.sh "v$new"
if command -v appstreamcli >/dev/null 2>&1; then
    appstreamcli validate --pedantic --no-net \
        data/metainfo/io.github.zbndev.Tidemark.metainfo.xml
fi

git add Cargo.toml Cargo.lock crates/tidemark-core/Cargo.toml crates/tidemarkd/Cargo.toml \
    data/metainfo/io.github.zbndev.Tidemark.metainfo.xml PKGBUILD
git commit -m "chore: bump to v$new"
git tag -a "v$new" -m "Tidemark v$new"
committed=1

if ! git push origin main "v$new"; then
    printf 'release.sh: the push failed, but the commit and tag are local; push them, e.g.: git push origin main v%s\n' \
        "$new" >&2
    exit 1
fi

printf 'release.sh: v%s is pushed; the Release workflow is drafting the release\n' "$new"
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `scripts/test-release.sh`
Expected: PASS — `cutting a full release` runs against the fixture (the push lands on the
throwaway bare remote, which exceeds the spec's "up to but not including the push" while
staying hermetic), then the numeric-order case, ending with `the happy paths hold`.

- [ ] **Step 5: Run shellcheck on both scripts**

Run: `shellcheck scripts/release.sh scripts/test-release.sh`
Expected: no output, exit 0.

- [ ] **Step 6: Commit**

```bash
git add scripts/release.sh scripts/test-release.sh
git commit -m "feat: bump, commit, tag and push from the release script"
```

---

### Task 4: CI wiring

**Files:**
- Modify: `.github/workflows/ci.yml` (after the "The user-daemon restart helper" step)
- Modify: `.github/workflows/release.yml` (after the "The user-daemon restart helper" run)

**Interfaces:**
- Consumes: `scripts/test-release.sh` from Tasks 1–3.
- Produces: nothing consumed later.

- [ ] **Step 1: Add the step to ci.yml**

In `.github/workflows/ci.yml`, directly after the `scripts/test-restart-user-daemon.sh`
step, add:

```yaml
      - name: The release helper
        run: scripts/test-release.sh
```

- [ ] **Step 2: Add the step to release.yml**

In `.github/workflows/release.yml`, directly after the
`- run: scripts/test-restart-user-daemon.sh` line in the `checks` job, add:

```yaml
      - run: scripts/test-release.sh
```

- [ ] **Step 3: Verify the YAML parses**

Run: `python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/ci.yml')); yaml.safe_load(open('.github/workflows/release.yml')); print('ok')"`
Expected: `ok`. (If `python3` or `yaml` is unavailable, `git diff` review of indentation
against the neighbouring steps is an acceptable substitute.)

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/ci.yml .github/workflows/release.yml
git commit -m "ci: run the release script tests"
```

---

### Task 5: Full-suite verification

**Files:** none modified.

- [ ] **Step 1: Run the project's own gates**

Run: `cargo fmt --all --check && shellcheck scripts/*.sh data/restart-user-daemon data/packaging/deb/postinst data/packaging/rpm/post-install.sh && scripts/test-release.sh`
Expected: all pass, ending with `the happy paths hold`.

- [ ] **Step 2: Confirm the working tree is clean**

Run: `git status --porcelain`
Expected: empty (every task committed; nothing stray introduced).

---

## Self-Review Notes

- Corrected during execution (coordinator, 2026-08-22): reject() originally asserted an
  empty porcelain after a rejection, which the dirty-tree scenario (a deliberately dirty
  fixture) can never satisfy; the spec's invariant is "changed nothing", so the helper now
  asserts before/after equality.
- Corrected during execution (coordinator, 2026-08-22): the fixture needs empty target
  stubs (src/lib.rs, src/main.rs) or `cargo update` cannot load the workspace ("no targets
  specified"); happy-path invocations redirect stderr too so git push's summary does not
  pollute test output; the numeric-order scenario syncs its `fixture at 0.1.9` commit to
  the fixture origin before releasing, or the stale-main guard correctly fires.
- Spec coverage: canonical-argument gate (Task 1), main/clean/stale/newer/tag guards
  (Task 2), six-file edit set, `check-tag-version.sh` self-check, best-effort
  `appstreamcli`, explicit `git add`, `chore: bump to vX.X.X`, annotated tag, single push
  with recovery hint, edits-remain-hint trap (Task 3), shellcheck plus fixture smoke test
  (Tasks 1–4). Non-goals need no tasks.
- The happy-path push runs against the fixture's bare remote — beyond the spec's "up to but
  not including the push", hermetically, and asserted on the remote side.
- `v0.1.0` is a lightweight tag; the script creates annotated tags, per the spec.
- If `cargo update --workspace --offline` ever fails in CI's clean environment, dropping
  `--offline` is the fallback; it was verified to work offline on the development machine.
