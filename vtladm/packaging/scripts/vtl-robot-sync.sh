#!/bin/sh
# Periodic kernel→DB catalog hints (runtime robot stays in vtl.ko).
# Requires robot_sync=true and auto_sync_db_from_kernel!=false in vtl.conf.
set -eu

PREFIX="${VTL_PREFIX:-/opt/vtladm}"
VTL_VAR="${VTL_VAR:-$PREFIX/var}"
VTL_CONF="${VTL_CONF_PATH:-$VTL_VAR/vtl.conf}"

if [ ! -x "$PREFIX/bin/vtladm" ]; then
  echo "vtl-robot-sync: vtladm not found at $PREFIX/bin/vtladm" >&2
  exit 1
fi
if [ ! -c /dev/vtl ]; then
  echo "vtl-robot-sync: /dev/vtl missing (load vtl.ko?)" >&2
  exit 0
fi
if [ ! -f "$VTL_CONF" ]; then
  echo "vtl-robot-sync: no vtl.conf" >&2
  exit 0
fi
if ! grep -qE '^[[:space:]]*robot_sync[[:space:]]*=[[:space:]]*true' "$VTL_CONF" 2>/dev/null; then
  exit 0
fi
if grep -qE '^[[:space:]]*auto_sync_db_from_kernel[[:space:]]*=[[:space:]]*false' "$VTL_CONF" 2>/dev/null; then
  exit 0
fi

echo "=== vtl-robot-sync $(date -Iseconds 2>/dev/null || date) ==="
if "$PREFIX/bin/vtladm" robot sync-db; then
  echo "OK  kernel catalog hints synced to DB"
  exit 0
fi
echo "WARN vtl-robot-sync failed (see vtladm log)" >&2
exit 1
