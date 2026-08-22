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

edited=
committed=
trap 'if [ -n "$edited" ] && [ -z "$committed" ]; then
        printf "release.sh: edits are still in the working tree; revert with: git checkout -- .\n" >&2
    fi' EXIT

die() {
    printf 'release.sh: %s\n' "$1" >&2
    exit 1
}

# A canonical version: exactly three dot-separated groups of ASCII digits, no leading
# zeroes (a lone zero is fine) — the same form check-tag-version.sh and tidemarkd's update
# checker accept.
canonical() {
    case $1 in *[!0-9.]* | .* | *.) return 1 ;; esac

    old_ifs=$IFS
    IFS=.
    set -f
    # shellcheck disable=SC2086  # splitting on '.' is the point; set -f guards globbing
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
