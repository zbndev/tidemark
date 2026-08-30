#!/bin/sh
set -eu

# Proves that installing a newer package leaves the *new* tidemarkd running, in both
# package formats, against a real systemd and a real package transaction.
#
#   scripts/test-package-upgrade.sh [work directory]
#
# Run by hand. This has no GitHub Actions trigger on purpose: it needs systemd as PID 1 in
# a privileged container, and the thing it guards changes about once a release. See
# docs/superpowers/specs/2026-08-22-ci-release-packaging-design.md.
#
# # Why the packages are built inside the target containers
#
# An earlier draft built them on the development machine and installed them into the
# containers. That proves nothing. The binaries would be linked against the host
# distribution's libraries, and cargo-generate-rpm's auto-req scans the payload
# *transitively*, so an rpm built on Arch asks for libgstreamer, libcups and
# libxml2.so.16 — libraries neither binary links and which Fedora numbers differently.
#
# # Why the assertion is the Version property
#
# A changed PID says something restarted. The daemon's own Version property, read over the
# user's session bus, says the thing now running is the new code. That is the claim being
# tested, so it is the one asserted. The PID is reported too, as corroboration.
#
# # Notes earned the hard way
#
# * The build containers run with --network host. Docker's bridge network has no working
#   NAT on at least one development machine. An earlier diagnostic pipeline around the
#   quiet build also masked that failure's exit status; these builds are not piped.
# * The test images carry the runtime dependencies already, so the upgrade transaction
#   needs no network and runs `dpkg -i` / `rpm -U` rather than an apt or dnf resolver.
#   That keeps the maintainer scripts the only thing under test.
# * Packages reach the test container through a bind mount, never `docker cp` into /tmp:
#   systemd mounts /tmp as a tmpfs over the image layer, so a copied file vanishes.

work=${1:-/tmp/tidemark-upgrade}
mkdir -p "$work"
work=$(CDPATH='' cd -- "$work" && pwd)

project_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)

old_version=$(sed -n '/^\[workspace\.package\]/,/^\[/ s/^version = "\(.*\)"$/\1/p' \
    "$project_root/Cargo.toml")
# Differs only in the last component, so the second build is a relink, not a rebuild.
new_version="${old_version%.*}.$(( ${old_version##*.} + 1 ))"

printf 'proving the upgrade from %s to %s\n' "$old_version" "$new_version"

# git archive rather than a bind mount: it carries exactly the committed tree, without the
# host's target/ directory, and makes the build reproduce what CI builds.
git -C "$project_root" archive --format=tar HEAD >"$work/source.tar"

build_packages() {
    distribution=$1
    package_command=$2
    old_artifact=$3
    new_artifact=$4

    printf '\n--- building the %s packages ---\n' "$distribution"
    docker volume create "tidemark-target-$distribution" >/dev/null

    docker run --rm --network host \
        -v "tidemark-target-$distribution":/build/target \
        -v "$work":/out \
        "tidemark-build-$distribution" sh -eu -c "
            mkdir -p /build && cd /build && tar -xf /out/source.tar
            cargo build --release --locked --workspace
            $package_command
            cp $old_artifact /out/

            sed -i '0,/^version = \"$old_version\"\$/s//version = \"$new_version\"/' Cargo.toml
            cargo build --release --workspace
            $package_command
            cp $new_artifact /out/
        " >/dev/null
}

# The daemon's own account of what it is, over the user's session bus.
daemon_version() {
    docker exec -u tester "$1" env "XDG_RUNTIME_DIR=/run/user/$2" \
        busctl --user get-property \
            io.github.zbndev.Tidemark.Daemon \
            /io/github/zbndev/Tidemark \
            io.github.zbndev.Tidemark.Daemon1 \
            Version \
        | sed 's/^s "//; s/"$//' | tr -d '\r\n'
}

daemon_pid() {
    docker exec -u tester "$1" env "XDG_RUNTIME_DIR=/run/user/$2" systemctl --user \
        show -p MainPID --value tidemarkd.service | tr -d '\r\n'
}

run_case() {
    distribution=$1
    install_old=$2
    install_new=$3

    printf '\n=== %s ===\n' "$distribution"

    container=$(docker run -d --rm --privileged --cgroupns=host \
        -v /sys/fs/cgroup:/sys/fs/cgroup:rw \
        -v "$work":/packages:ro \
        "tidemark-test-$distribution" /sbin/init)
    # shellcheck disable=SC2064  # $container must expand now, not when the trap fires.
    trap "docker rm -f $container >/dev/null 2>&1 || true" EXIT

    for _ in $(seq 45); do
        case $(docker exec "$container" systemctl is-system-running 2>/dev/null || true) in
            running | degraded) break ;;
        esac
        sleep 2
    done

    # Lingering gives tester a user manager and runtime directory without a login session.
    # The package transaction then reaches that manager through runuser.
    docker exec "$container" useradd -m tester
    docker exec "$container" loginctl enable-linger tester
    uid=$(docker exec "$container" id -u tester | tr -d '\r\n')
    for _ in $(seq 30); do
        docker exec "$container" systemctl is-active "user@$uid.service" >/dev/null 2>&1 && break
        sleep 2
    done

    docker exec "$container" sh -c "$install_old" >/dev/null

    # A fresh install must start nothing: there is no restart on that path, and try-restart
    # leaves an inactive unit inactive. D-Bus activation is what starts it in real use.
    if docker exec -u tester "$container" env "XDG_RUNTIME_DIR=/run/user/$uid" \
        systemctl --user is-active --quiet tidemarkd.service; then
        printf 'a fresh install left the daemon running; nothing should have started it\n' >&2
        exit 1
    fi
    printf 'fresh install: the daemon is not running, as intended\n'

    docker exec -u tester "$container" env "XDG_RUNTIME_DIR=/run/user/$uid" \
        systemctl --user start tidemarkd.service
    sleep 3

    before_version=$(daemon_version "$container" "$uid")
    before_pid=$(daemon_pid "$container" "$uid")
    [ "$before_version" = "$old_version" ] || {
        printf 'before the upgrade the daemon reported "%s", expected "%s"\n' \
            "$before_version" "$old_version" >&2
        exit 1
    }
    printf 'before: version %s, pid %s\n' "$before_version" "$before_pid"

    docker exec "$container" sh -c "$install_new" >/dev/null

    # try-restart returns before the unit is back up.
    sleep 5
    after_version=$(daemon_version "$container" "$uid")
    after_pid=$(daemon_pid "$container" "$uid")
    [ "$after_version" = "$new_version" ] || {
        printf 'after the upgrade the daemon reported "%s", expected "%s"\n' \
            "$after_version" "$new_version" >&2
        printf 'the package replaced the files and left the old daemon running\n' >&2
        exit 1
    }
    printf 'after:  version %s, pid %s\n' "$after_version" "$after_pid"

    trap - EXIT
    docker rm -f "$container" >/dev/null
}

# ---------------------------------------------------------------------------------------
# Images. Docker layers make a second run cheap; the target directories are named volumes,
# so the second version costs a relink rather than a compile.
# ---------------------------------------------------------------------------------------

printf '\n--- images ---\n'

docker build --network host -q -t tidemark-build-ubuntu - >/dev/null <<'DOCKERFILE'
FROM ubuntu:26.04
ENV DEBIAN_FRONTEND=noninteractive PATH=/root/.cargo/bin:$PATH
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential cmake libclang-dev curl ca-certificates pkg-config dpkg-dev \
        libgtk-4-dev libadwaita-1-dev libsqlite3-dev \
    && rm -rf /var/lib/apt/lists/*
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
RUN cargo install cargo-deb --locked
DOCKERFILE

docker build --network host -q -t tidemark-test-ubuntu - >/dev/null <<'DOCKERFILE'
FROM ubuntu:26.04
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
        systemd systemd-sysv dbus-user-session dbus-daemon \
        libgtk-4-1 libadwaita-1-0 libsqlite3-0 hicolor-icon-theme \
    && rm -rf /var/lib/apt/lists/*
DOCKERFILE

docker build --network host -q -t tidemark-build-fedora - >/dev/null <<'DOCKERFILE'
FROM fedora:44
ENV PATH=/root/.cargo/bin:$PATH
RUN dnf install -y git gcc gcc-c++ cmake clang-devel curl pkgconf-pkg-config rpm-build \
        gtk4-devel libadwaita-devel sqlite-devel && dnf clean all
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
RUN cargo install cargo-generate-rpm --locked
DOCKERFILE

docker build --network host -q -t tidemark-test-fedora - >/dev/null <<'DOCKERFILE'
FROM fedora:44
RUN dnf install -y systemd dbus-daemon gtk4 libadwaita sqlite-libs hicolor-icon-theme \
        util-linux \
    && dnf clean all
DOCKERFILE

# ---------------------------------------------------------------------------------------

build_packages ubuntu 'cargo deb --no-build -p tidemark' \
    "target/debian/tidemark_${old_version}-1_amd64.deb" \
    "target/debian/tidemark_${new_version}-1_amd64.deb"

build_packages fedora 'cargo generate-rpm -p crates/tidemark' \
    "target/generate-rpm/tidemark-${old_version}-1.x86_64.rpm" \
    "target/generate-rpm/tidemark-${new_version}-1.x86_64.rpm"

rm -f "$work/source.tar"

run_case ubuntu \
    "dpkg -i /packages/tidemark_${old_version}-1_amd64.deb" \
    "dpkg -i /packages/tidemark_${new_version}-1_amd64.deb"

run_case fedora \
    "rpm -i /packages/tidemark-${old_version}-1.x86_64.rpm" \
    "rpm -U /packages/tidemark-${new_version}-1.x86_64.rpm"

printf '\nboth formats restart the daemon into the new binary on upgrade\n'
