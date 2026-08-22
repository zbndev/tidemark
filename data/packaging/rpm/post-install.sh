#!/bin/sh
set -e

# rpm passes the number of installed instances of this package: 1 on a fresh install, 2 or
# more on an upgrade. On an upgrade the new package's %post runs *before* the old package's
# %postun, so the restart belongs here and nowhere else — a %postun that stopped anything
# would run afterwards and undo it. There is deliberately no %postun.
if [ "$1" -ge 2 ]; then
    /usr/lib/tidemark/restart-user-daemon
else
    # Never allowed to fail the transaction, for the reason recorded in the Debian
    # postinst: a minimal installation may carry no /usr/share/doc at all, so the notice
    # lives under /usr/lib and is printed defensively even there.
    cat /usr/lib/tidemark/first-run.txt 2>/dev/null || true
fi
