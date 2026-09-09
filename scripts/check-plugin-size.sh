#!/usr/bin/env bash
# The plugin runtime's binary-size budget, from the design: vendored Lua and the plugin
# machinery may add at most 1.5 MiB to the stripped Linux daemon. Over budget, the dependency
# decision goes back to design review rather than being accepted quietly — so this is a gate,
# not a report.
#
# Run by hand, before the plugin work merges, and again whenever a dependency of the daemon
# changes. Deliberately not a CI step: the question it answers is asked once, about one
# decision, and after the answer is in the history a per-commit run would rebuild the release
# profile twice to print a delta of zero.
set -euo pipefail
cd "$(dirname "$0")/.."

baseline_ref=${1:?usage: check-plugin-size.sh <baseline-commit>}
budget=$((1536 * 1024))
binary=target/release/tidemarkd

# Where a baseline build is put, and how it is taken away again. Set before the first
# worktree exists so an interrupted build leaves neither a directory nor a registration
# behind — `git worktree add` refuses a path that is already claimed, so a leaked one would
# make every later run fail rather than just this one.
scratch=
cleanup() {
    if [ -n "$scratch" ]; then
        git worktree remove --force "$scratch/tree" >/dev/null 2>&1 || true
        rm -rf "$scratch"
    fi
}
trap cleanup EXIT

size_at() {
    # Built in a detached worktree so the working tree is never touched, and with the release
    # profile the packaging uses — a debug build's size says nothing about what ships.
    local ref=$1
    scratch=$(mktemp -d)
    git worktree add --detach --quiet "$scratch/tree" "$ref" >&2
    (cd "$scratch/tree" && cargo build --release --locked -p tidemarkd >&2)
    stat -c %s "$scratch/tree/$binary"
    cleanup
    scratch=
}

baseline=$(size_at "$baseline_ref")
cargo build --release --locked -p tidemarkd
current=$(stat -c %s "$binary")
delta=$((current - baseline))

printf 'baseline %s: %s bytes\ncurrent:  %s bytes\ndelta:    %s bytes (budget %s)\n' \
    "$baseline_ref" "$baseline" "$current" "$delta" "$budget"

if [ "$delta" -gt "$budget" ]; then
    printf 'over the plugin size budget: this returns to design review, per the spec\n' >&2
    exit 1
fi
echo 'plugin size ok'
