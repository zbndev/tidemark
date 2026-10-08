#!/usr/bin/env bash
# Enforces Tidemark's crate layering, so that the split survives contact with a hurry.
#
#   tidemark-types  the shared vocabulary. Reaches nothing.
#   tidemark-core   network, disk, secrets. Never the display.
#   tidemarkd       the only process allowed to hold both.
#   tidemark-ipc    the generated D-Bus proxy, shared by every client.
#   tidemark-cli    tidemarkctl. Speaks D-Bus and prints; no runtime, no display.
#   tidemark        the display. Never the network, never the database, never core —
#                   it speaks D-Bus, which is what keeps `tidemarkctl` a third consumer
#                   rather than a rewrite.
#
# Run from anywhere; exits non-zero on the first violation.
set -euo pipefail
cd "$(dirname "$0")/.."

status=0

# Resolve every platform: a Windows-only dependency can enable a Linux toolkit feature
# in Cargo.lock even when that feature is never built on the current host.
resolved=$(cargo tree --quiet --locked --workspace --target all --edges normal,build,dev \
    --prefix none --format '{p}' | awk '{print $1}' | sort -u)
toolkits=$(printf '%s\n' "$resolved" \
    | grep -E '^(gtk[34]?(-sys|-macros)?|gdk[34]?(-sys)?|gdk-pixbuf(-sys)?|glib(-sys|-macros)?|gio(-sys)?|gobject-sys|pango(-sys)?|cairo-rs|cairo-sys-rs|atk(-sys)?|libadwaita(-sys)?|libappindicator(-sys)?)$' || true)
if [ -n "$toolkits" ]; then
    printf 'the workspace must use Slint without the retired display stack:\n%s\n' "$toolkits" >&2
    status=1
fi

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
    reqwest hyper rusqlite libsqlite3-sys tokio slint zbus

# The contract's client half: it may open a connection, and nothing else. tokio is on the
# list because zbus can be built on either reactor, and a CLI whose value is starting fast
# must not acquire a second runtime by accident.
forbid tidemark-ipc 'the contract carries no implementation' \
    tidemark-core reqwest hyper rusqlite libsqlite3-sys slint tokio

forbid tidemark-cli 'the CLI prints what the daemon publishes and nothing else' \
    tidemark-core reqwest hyper rusqlite libsqlite3-sys slint tokio

forbid tidemark-core 'core must build on a machine with no display stack' \
    slint i-slint-core winit wgpu

forbid tidemark 'the client talks to tidemarkd over D-Bus, not to providers' \
    tidemark-core reqwest hyper rusqlite libsqlite3-sys

if [ "$status" -eq 0 ]; then
    echo 'layering ok'
fi
exit "$status"
