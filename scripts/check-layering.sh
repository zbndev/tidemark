#!/usr/bin/env bash
# Enforces Tidemark's crate layering, so that the split survives contact with a hurry.
#
#   tidemark-types  the shared vocabulary. Reaches nothing.
#   tidemark-core   network, disk, secrets. Never the display.
#   tidemarkd       the only process allowed to hold both.
#   tidemark        the display. Never the network, never the database, never core —
#                   it speaks D-Bus, which is what keeps a future CLI a third consumer
#                   rather than a rewrite.
#
# Run from anywhere; exits non-zero on the first violation.
set -euo pipefail
cd "$(dirname "$0")/.."

status=0

forbid() {
    local package=$1 reason=$2
    shift 2
    local tree
    tree=$(cargo tree --quiet --package "$package" --edges normal --prefix none --format '{p}' \
           | awk '{print $1}' | sort -u)
    local banned
    for banned in "$@"; do
        if grep -qx -- "$banned" <<<"$tree"; then
            printf '%s must not depend on %s (%s)\n' "$package" "$banned" "$reason" >&2
            cargo tree --quiet --package "$package" --edges normal --invert "$banned" >&2 || true
            status=1
        fi
    done
}

# zvariant is deliberately *not* on this list: the D-Bus wire shapes live in
# tidemark-types and need its derives. zbus is, and stays — encoding a message is the
# contract, opening a connection is an implementation.
forbid tidemark-types 'it is the contract, not an implementation' \
    reqwest hyper rusqlite libsqlite3-sys tokio gtk4 gtk4-sys libadwaita zbus

forbid tidemark-core 'core must build on a machine with no display stack' \
    gtk4 gtk4-sys gdk4-sys libadwaita libadwaita-sys

forbid tidemark 'the client talks to tidemarkd over D-Bus, not to providers' \
    tidemark-core reqwest hyper rusqlite libsqlite3-sys

if [ "$status" -eq 0 ]; then
    echo 'layering ok'
fi
exit "$status"
