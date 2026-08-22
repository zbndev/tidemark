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

    mkdir -p "$fixture/scripts" "$fixture/data/metainfo" "$fixture/crates/tidemark-types" \
        "$fixture/crates/tidemark-core" "$fixture/crates/tidemarkd" "$fixture/crates/tidemark"
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

    git -C "$fixture" init -q -b main
    git -C "$fixture" config user.name fixture
    git -C "$fixture" config user.email fixture@invalid
    git -C "$fixture" add -A
    git -C "$fixture" commit -q -m fixture

    git init -q --bare "$fixture.origin.git"
    git -C "$fixture" remote add origin "$fixture.origin.git"
    git -C "$fixture" push -q origin main
}

# Runs the fixture's release.sh expecting failure, and proving it edited nothing.
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
