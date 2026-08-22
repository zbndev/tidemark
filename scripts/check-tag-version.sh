#!/bin/sh
set -eu

# Refuses a tag that disagrees with the workspace manifest, so a release can never carry a
# version nobody bumped.
#
#   scripts/check-tag-version.sh v0.1.0

tag=${1:?usage: check-tag-version.sh <tag>}
project_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)

# Only the [workspace.package] version counts. Every member crate says
# `version.workspace = true`, so there is exactly one number to agree with.
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
