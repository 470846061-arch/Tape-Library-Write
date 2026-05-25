#!/bin/sh
# Thin wrapper: full patrol logic lives in `vtladm patrol` (userspace/src/patrol.rs).
# Exit: 0 OK, 1 warning, 2 critical.
set -eu

PREFIX="${VTL_PREFIX:-/opt/vtladm}"
if [ ! -x "$PREFIX/bin/vtladm" ]; then
  echo "CRIT vtladm not found at $PREFIX/bin/vtladm" >&2
  exit 2
fi
exec "$PREFIX/bin/vtladm" patrol
