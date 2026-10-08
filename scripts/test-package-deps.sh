#!/bin/sh
# Exercises dependency validation using control archives, without building binaries or RPMs.
set -eu
project_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
test_root=$(mktemp -d "${TMPDIR:-/tmp}/tidemark-package-deps.XXXXXX")
trap 'rm -rf -- "$test_root"' EXIT HUP INT TERM
mkdir "$test_root/control"

linked='libfontconfig1, libsqlite3-0'
runtime='dbus-user-session, hicolor-icon-theme, libxkbcommon0, libwayland-client0, libegl1'
recommended='libx11-xcb1, libxcursor1, libxi6, libxrandr2, libxkbcommon-x11-0, xdg-desktop-portal'

check() {
    name=$1 expected=$2 depends=$3 recommends=$4
    printf 'Package: tidemark\nDepends: %s\nRecommends: %s\n' "$depends" "$recommends" \
        > "$test_root/control/control"
    tar -cJf "$test_root/control.tar.xz" -C "$test_root/control" ./control
    (cd "$test_root" && ar rcs fixture.deb control.tar.xz)
    if "$project_root/scripts/check-package-deps.sh" "$test_root/fixture.deb" \
        > "$test_root/result" 2>&1; then
        actual=pass
    else
        actual=fail
    fi
    if [ "$actual" != "$expected" ]; then
        printf '%s: expected %s, got %s\n' "$name" "$expected" "$actual" >&2
        cat "$test_root/result" >&2
        exit 1
    fi
}

check complete pass "$linked, $runtime" "$recommended"
check no-elf-dependencies fail "$runtime" "$recommended"
check no-runtime-dependencies fail "$linked" "$recommended"
for dependency in libxkbcommon0 libwayland-client0 libegl1 dbus-user-session hicolor-icon-theme; do
    reduced=$(printf '%s' "$runtime" | sed "s/$dependency//")
    check "missing-$dependency" fail "$linked, $reduced" "$recommended"
    # A recommendation is not enough for a library every Slint startup needs.
    check "recommended-$dependency" fail "$linked, $reduced" "$recommended, $dependency"
done
for dependency in libx11-xcb1 libxcursor1 libxi6 libxrandr2 libxkbcommon-x11-0 xdg-desktop-portal; do
    reduced=$(printf '%s' "$recommended" | sed "s/$dependency//")
    check "missing-$dependency" fail "$linked, $runtime" "$reduced"
done

printf '%s\n' 'package dependency checks ok'
