#!/bin/sh
set -eu

# Proves that installing a newer package leaves the *new* tidemarkd running, in both
# package formats, against a real systemd and a real package transaction.
#
#   scripts/test-package-upgrade.sh [output directory]
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
# The package would not install, or would install and not start, and either way the
# question this script exists to answer would go unanswered.
#
# # Why the assertion is the Version property
#
# A changed PID says something restarted. The daemon's own Version property, read over the
# user's session bus, says the thing now running is the new code. That is the claim being
# tested, so it is the one asserted.
#
# Both stages are cached: the build images are Docker layers and each target directory is a
# named volume, so a second run costs a relink rather than a full compile.

output=${1:-/tmp/tidemark-upgrade}
mkdir -p "$output"
output=$(CDPATH='' cd -- "$output" && pwd)

project_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)

old_version=$(sed -n '/^\[workspace\.package\]/,/^\[/ s/^version = "\(.*\)"$/\1/p' \
    "$project_root/Cargo.toml")
# A version that differs only in its last component, so the second build is a relink.
new_version="${old_version%.*}.$(( ${old_version##*.} + 1 ))"

printf 'building %s and %s of each package\n' "$old_version" "$new_version"

# ---------------------------------------------------------------------------------------
# Build stage: one image per distribution, carrying the toolkit and the packaging tool.
# ---------------------------------------------------------------------------------------

build_image() {
    distribution=$1
    docker build -q -t "tidemark-build-$distribution" - >/dev/null
}

build_packages() {
    distribution=$1
    package_command=$2
    artifacts=$3

    volume="tidemark-upgrade-target-$distribution"
    docker volume create "$volume" >/dev/null

    # git archive rather than a bind mount: it carries exactly the committed tree, without
    # the host's target/ directory, and makes the build reproduce what CI builds.
    git -C "$project_root" archive --format=tar HEAD >"$output/source.tar"

    docker run --rm \
        -v "$volume":/build/target \
        -v "$output":/out \
        "tidemark-build-$distribution" sh -eux -c "
            mkdir -p /build && cd /build
            tar -xf /out/source.tar

            cargo build --release --locked --workspace
            $package_command
            cp $artifacts /out/

            # Only the version string changes, so this is a relink rather than a rebuild.
            sed -i '0,/^version = \"$old_version\"\$/s//version = \"$new_version\"/' Cargo.toml
            cargo build --release --workspace
            $package_command
            cp $artifacts /out/
        "

    rm -f "$output/source.tar"
}

# ---------------------------------------------------------------------------------------
# Test stage: systemd as PID 1, a lingering user, and two real package transactions.
# ---------------------------------------------------------------------------------------

read_version() {
    container=$1
    uid=$(docker exec "$container" id -u tester)
    docker exec -u tester "$container" env "XDG_RUNTIME_DIR=/run/user/$uid" \
        busctl --user get-property \
            io.github.zbndev.Tidemark.Daemon \
            /io/github/zbndev/Tidemark \
            io.github.zbndev.Tidemark.Daemon1 \
            Version \
        | sed 's/^s "//; s/"$//' | tr -d '\r\n'
}

as_tester() {
    container=$1
    shift
    uid=$(docker exec "$container" id -u tester)
    docker exec -u tester "$container" env "XDG_RUNTIME_DIR=/run/user/$uid" "$@"
}

run_case() {
    distribution=$1
    install_old=$2
    install_new=$3

    printf '\n=== %s ===\n' "$distribution"

    container=$(docker run -d --rm --privileged --cgroupns=host \
        -v /sys/fs/cgroup:/sys/fs/cgroup:rw \
        -v "$output":/packages:ro \
        "tidemark-test-$distribution" /usr/sbin/init)
    # shellcheck disable=SC2064  # $container must expand now, not when the trap fires.
    trap "docker rm -f $container >/dev/null 2>&1 || true" EXIT

    docker exec "$container" sh -c \
        'for _ in $(seq 90); do systemctl is-system-running >/dev/null 2>&1 && break; sleep 1; done'

    # Lingering gives tester a user manager without a login session, which is what makes
    # `systemctl --user --machine=tester@.host` reachable from the root transaction.
    docker exec "$container" useradd -m tester
    docker exec "$container" loginctl enable-linger tester
    docker exec "$container" sh -c \
        'for _ in $(seq 60); do systemctl is-active "user@$(id -u tester).service" >/dev/null 2>&1 && break; sleep 1; done'

    docker exec "$container" sh -c "$install_old"

    # A fresh install must not start anything: post-install runs no restart, and try-restart
    # on the upgrade path leaves an inactive unit inactive.
    if as_tester "$container" systemctl --user is-active --quiet tidemarkd.service; then
        printf 'a fresh install left the daemon running; nothing should have started it\n' >&2
        exit 1
    fi
    printf 'fresh install: the daemon is not running, as intended\n'

    as_tester "$container" systemctl --user start tidemarkd.service
    reported=$(read_version "$container")
    [ "$reported" = "$old_version" ] || {
        printf 'before the upgrade the daemon reported "%s", expected "%s"\n' \
            "$reported" "$old_version" >&2
        exit 1
    }
    printf 'before: %s\n' "$reported"

    docker exec "$container" sh -c "$install_new"

    # try-restart returns before the unit is back up.
    sleep 5
    reported=$(read_version "$container")
    [ "$reported" = "$new_version" ] || {
        printf 'after the upgrade the daemon reported "%s", expected "%s"\n' \
            "$reported" "$new_version" >&2
        printf 'the package replaced the files and left the old daemon running\n' >&2
        exit 1
    }
    printf 'after:  %s\n' "$reported"

    trap - EXIT
    docker rm -f "$container" >/dev/null
}

# ---------------------------------------------------------------------------------------
# Ubuntu
# ---------------------------------------------------------------------------------------

build_image ubuntu <<'DOCKERFILE'
FROM ubuntu:26.04
ENV DEBIAN_FRONTEND=noninteractive PATH=/root/.cargo/bin:$PATH
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential curl ca-certificates pkg-config dpkg-dev \
        libgtk-4-dev libadwaita-1-dev libsqlite3-dev \
    && rm -rf /var/lib/apt/lists/*
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
RUN cargo install cargo-deb --locked
DOCKERFILE

docker build -q -t tidemark-test-ubuntu - >/dev/null <<'DOCKERFILE'
FROM ubuntu:26.04
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
        systemd systemd-sysv dbus-user-session \
    && rm -rf /var/lib/apt/lists/*
DOCKERFILE

build_packages ubuntu 'cargo deb --no-build -p tidemark' 'target/debian/*.deb'

# ---------------------------------------------------------------------------------------
# Fedora
# ---------------------------------------------------------------------------------------

build_image fedora <<'DOCKERFILE'
FROM fedora:44
ENV PATH=/root/.cargo/bin:$PATH
RUN dnf install -y gcc curl git pkgconf-pkg-config rpm-build \
        gtk4-devel libadwaita-devel sqlite-devel && dnf clean all
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
RUN cargo install cargo-generate-rpm --locked
DOCKERFILE

docker build -q -t tidemark-test-fedora - >/dev/null <<'DOCKERFILE'
FROM fedora:44
RUN dnf install -y systemd dbus-daemon && dnf clean all
DOCKERFILE

build_packages fedora 'cargo generate-rpm -p crates/tidemark' 'target/generate-rpm/*.rpm'

# ---------------------------------------------------------------------------------------

ls -1 "$output"

run_case ubuntu \
    "apt-get update && apt-get install -y /packages/tidemark_${old_version}-1_amd64.deb" \
    "apt-get install -y /packages/tidemark_${new_version}-1_amd64.deb"

run_case fedora \
    "dnf install -y /packages/tidemark-${old_version}-1.x86_64.rpm" \
    "dnf upgrade -y /packages/tidemark-${new_version}-1.x86_64.rpm"

printf '\nboth formats restart the daemon on upgrade\n'
