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
reject ".$next_version"
reject "$next_version."
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
