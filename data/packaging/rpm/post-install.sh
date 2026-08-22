#!/bin/sh
set -e

# rpm passes the number of installed instances of this package: 1 on a fresh install, 2 or
# more on an upgrade. On an upgrade the new package's %post runs *before* the old
# package's %postun, so the restart belongs here and nowhere else — a %postun that stopped
# anything would run afterwards and undo it. There is deliberately no %postun.
if [ "$1" -ge 2 ]; then
    /usr/lib/tidemark/restart-user-daemon
else
    cat /usr/share/doc/tidemark/first-run.txt
fi
