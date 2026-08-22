#!/bin/sh
set -eu

project_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
temporary=$(mktemp -d)
trap 'rm -rf "$temporary"' EXIT HUP INT TERM

calls="$temporary/calls"

printf '%s\n' '#!/bin/sh' "printf '%s\\n' '1000 herald yes active' '1001 guest no online'" \
    >"$temporary/loginctl"
# The single quotes are the generated script's quotes; expansion must happen when that
# script runs, not while this fixture is being written.
# shellcheck disable=SC2016
printf '%s\n' '#!/bin/sh' 'printf '\''%s\n'\'' "$*" >>"$TIDEMARK_TEST_CALLS"' \
    >"$temporary/systemctl"
chmod +x "$temporary/loginctl" "$temporary/systemctl"

TIDEMARK_LOGINCTL="$temporary/loginctl" \
TIDEMARK_SYSTEMCTL="$temporary/systemctl" \
TIDEMARK_TEST_CALLS="$calls" \
    "$project_root/data/restart-user-daemon"

cat >"$temporary/expected" <<'EOF'
--user --machine=herald@.host daemon-reload
--user --machine=herald@.host try-restart tidemarkd.service
--user --machine=guest@.host daemon-reload
--user --machine=guest@.host try-restart tidemarkd.service
EOF

diff -u "$temporary/expected" "$calls"
