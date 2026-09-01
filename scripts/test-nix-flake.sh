#!/bin/sh
set -eu

project_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
image=nixos/nix:2.35.2
test_root=$(mktemp -d "${TMPDIR:-/tmp}/tidemark-nix-flake.XXXXXX")
source_root=$test_root/source
state_root=$test_root/state

cleanup() {
    rm -rf -- "$test_root"
}

trap cleanup EXIT HUP INT TERM
mkdir -p "$source_root" "$state_root"
(
    cd "$project_root"
    git ls-files --cached --others --exclude-standard -z \
        | tar --null -T - -cf -
) | tar -xf - -C "$source_root"

docker run -i --rm --network host --entrypoint sh \
    -v "$source_root:/src:ro" \
    -v "$state_root:/test-state" \
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
dbus_daemon=$(command -v dbus-daemon)
dbus_prefix=${dbus_daemon%/bin/dbus-daemon}
mkdir -p /etc/dbus-1
# The nixpkgs file delegates to /etc/dbus-1/session.conf, which is also where
# dbus-run-session looks in this minimal image. Copy it without that self-include.
while IFS= read -r line; do
    if [ "$line" != '  <include ignore_missing="yes">/etc/dbus-1/session.conf</include>' ]; then
        printf '%s\n' "$line"
    fi
done < "$dbus_prefix/share/dbus-1/session.conf" > /etc/dbus-1/session.conf

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

touch /test-state/completed
CONTAINER

test -f "$state_root/completed"
