#!/bin/sh
set -eu

project_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
image=nixos/nix:2.35.2

docker run --rm --network host --entrypoint sh \
    -v "$project_root:/src:ro" \
    -w /src \
    "$image" -eu -s <<'CONTAINER'
export NIX_CONFIG='experimental-features = nix-command flakes'

nix flake check --no-build
output=$(nix build --no-link --print-out-paths .#tidemark)

test -x "$output/bin/tidemark"
test -x "$output/bin/tidemarkd"
test -f "$output/share/applications/io.github.zbndev.Tidemark.desktop"
test -f "$output/share/metainfo/io.github.zbndev.Tidemark.metainfo.xml"
test -f "$output/share/icons/hicolor/512x512/apps/io.github.zbndev.Tidemark.png"

service="$output/share/dbus-1/services/io.github.zbndev.Tidemark.Daemon.service"
test -f "$service"
grep -Fx "Exec=$output/bin/tidemarkd" "$service"
grep -Fx 'SystemdService=tidemarkd.service' "$service"

export TIDEMARK_OUTPUT="$output"
nix shell --inputs-from . nixpkgs#dbus nixpkgs#systemd -c sh -eu -s <<'DAEMON'
dbus-run-session -- sh -eu -s <<'BUS'
daemon=

cleanup() {
    if [ -n "$daemon" ]; then
        kill "$daemon" 2>/dev/null || true
        wait "$daemon" 2>/dev/null || true
    fi
}

trap cleanup EXIT HUP INT TERM
"$TIDEMARK_OUTPUT/bin/tidemarkd" &
daemon=$!

deadline=$(( $(date +%s) + 30 ))
while :; do
    if introspection=$(busctl --user introspect io.github.zbndev.Tidemark.Daemon \
        /io/github/zbndev/Tidemark 2>/dev/null) \
        && printf '%s\n' "$introspection" | grep -Fq GetStatus; then
        break
    fi

    if [ "$(date +%s)" -ge "$deadline" ]; then
        printf '%s\n' 'Timed out waiting for Tidemark D-Bus service.' >&2
        exit 1
    fi

    sleep 1
done

busctl --user call io.github.zbndev.Tidemark.Daemon \
    /io/github/zbndev/Tidemark io.github.zbndev.Tidemark.Daemon1 GetStatus
BUS
DAEMON
CONTAINER
