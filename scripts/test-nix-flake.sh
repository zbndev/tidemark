#!/usr/bin/env bash
set -euo pipefail

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
    # Exclude tracked files removed by an uncommitted cleanup, but include new files.
    git ls-files --cached --others --exclude-standard -z \
        | while IFS= read -r -d '' path; do
            if [ -e "$path" ] || [ -L "$path" ]; then
                printf '%s\0' "$path"
            fi
        done \
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
test -x "$output/bin/tidemarkctl"
test -f "$output/share/applications/io.github.zbndev.Tidemark.desktop"
test -f "$output/share/metainfo/io.github.zbndev.Tidemark.metainfo.xml"
test -f "$output/share/icons/hicolor/512x512/apps/io.github.zbndev.Tidemark.png"

service="$output/share/dbus-1/services/io.github.zbndev.Tidemark.Daemon.service"
test -f "$service"
grep -Fx "Exec=$output/bin/tidemarkd" "$service"
grep -Fx 'SystemdService=tidemarkd.service' "$service"

export TIDEMARK_OUTPUT="$output"
nix shell --inputs-from . nixpkgs#patchelf -c sh -eu -s <<'RUNTIME'
# Slint/winit dlopen these libraries, so DT_NEEDED and the daemon probe cannot validate
# them. Check the GUI's actual search path without creating a window or a display server.
rpath=$(patchelf --print-rpath "$TIDEMARK_OUTPUT/bin/tidemark")
for soname in libxkbcommon.so.0 libxkbcommon-x11.so.0 libwayland-client.so.0 \
    libEGL.so.1 libGL.so.1 libX11.so.6 libX11-xcb.so.1 libXcursor.so.1 libXi.so.6 libXrandr.so.2; do
    found=false
    previous_ifs=$IFS
    IFS=:
    for directory in $rpath; do
        if [ -f "$directory/$soname" ]; then
            found=true
            break
        fi
    done
    IFS=$previous_ifs
    if [ "$found" != true ]; then
        printf 'Slint runtime library missing from the GUI search path: %s\n' "$soname" >&2
        exit 1
    fi
done
printf '%s\n' 'Slint runtime search path ok'
RUNTIME

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
