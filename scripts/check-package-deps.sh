#!/bin/sh
set -eu

# Refuses a package missing linked libraries or Slint's runtime-loaded libraries.
#
#   scripts/check-package-deps.sh target/debian/tidemark_0.1.0-1_amd64.deb
#   scripts/check-package-deps.sh target/generate-rpm/tidemark-0.1.0-1.x86_64.rpm
#
# Both package builds derive their dependencies from the built ELF — cargo-deb through
# dpkg-shlibdeps, cargo-generate-rpm through rpm's find-requires. Neither treats a missing
# helper as an error: on a machine without dpkg-dev, `depends = "$auto"` resolves to
# *nothing* and cargo-deb emits a warning that a CI log scrolls straight past. The result
# is a package that installs on a system without them and then fails to start.
#
# Measured on 2026-08-22: built on Arch, where dpkg-shlibdeps does not exist, the .deb came
# out with `Depends: dbus-user-session, hicolor-icon-theme` — only the two entries written
# by hand, with every library silently dropped. Hence this check, which turns that warning
# into a failed build.

package=${1:?usage: check-package-deps.sh <.deb or .rpm>}

case "$package" in
    *.deb)
        # dpkg-deb is not present on every developer machine; ar and tar are.
        control=$(ar p "$package" control.tar.xz | tar -xJO ./control)
        dependencies=$(printf '%s\n' "$control" | sed -n 's/^Depends: //p')
        recommended=$(printf '%s\n' "$control" | sed -n 's/^Recommends: //p')
        runtime='dbus-user-session hicolor-icon-theme libxkbcommon0 libwayland-client0 libegl1'
        optional='libx11-xcb1 libxcursor1 libxi6 libxrandr2 libxkbcommon-x11-0 xdg-desktop-portal'
        ;;
    *.rpm)
        command -v rpm >/dev/null || {
            printf 'rpm is not installed, so this package cannot be read here.\n' >&2
            printf 'Run this on Fedora, or in a fedora container with the file mounted.\n' >&2
            exit 1
        }
        dependencies=$(rpm -qRp "$package" | tr '\n' ' ')
        recommended=$(rpm -qp --qf '[%{RECOMMENDNAME}\n]' "$package" | tr '\n' ' ')
        runtime='dbus-common hicolor-icon-theme libxkbcommon libwayland-client libglvnd-egl util-linux'
        optional='libX11-xcb libXcursor libXi libXrandr libxkbcommon-x11 xdg-desktop-portal'
        ;;
    *)
        printf 'not a package this understands: %s\n' "$package" >&2
        exit 1
        ;;
esac

# An .rpm's requires run to a couple of hundred entries, so report the shape and the
# entries that matter rather than printing the lot into a CI log.
printf '%s entries\n' "$(printf '%s' "$dependencies" | tr ' ,' '\n' | grep -c .)"

# The interface links fontconfig and the daemon SQLite; a list naming neither was not
# derived from the binaries, whatever else it contains.
status=0
for library in fontconfig sqlite; do
    case "$dependencies" in
        *"$library"*)
            printf '  %s: %s\n' "$library" \
                "$(printf '%s' "$dependencies" | tr ' ,' '\n' | grep -i "$library" | tr '\n' ' ')"
            ;;
        *)
            printf 'nothing matching "%s" in the dependencies\n' "$library" >&2
            status=1
            ;;
    esac
done

# dlopen libraries cannot be discovered by ELF dependency scanners. X11 and the portal
# may be recommended, while keyboard, Wayland and EGL support must be hard requirements.
for dependency in $runtime; do
    if ! printf '%s\n' "$dependencies" | tr ' ,' '\n' | grep -Fxq "$dependency"; then
        printf 'missing required runtime dependency: %s\n' "$dependency" >&2
        status=1
    fi
done
for dependency in $optional; do
    if ! printf '%s %s\n' "$dependencies" "$recommended" | tr ' ,' '\n' | grep -Fxq "$dependency"; then
        printf 'missing recommended desktop dependency: %s\n' "$dependency" >&2
        status=1
    fi
done

if [ "$status" -ne 0 ]; then
    printf '%s\n' \
        'The package is missing linked or runtime-loaded libraries.' \
        'For a .deb, install dpkg-dev so that dpkg-shlibdeps can run.' \
        'For an .rpm, build on Fedora so that find-requires can run.' >&2
    exit 1
fi

printf 'the dependency list names linked libraries and the desktop runtime\n'
