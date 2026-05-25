#!/bin/sh
# Stress insmod/rmmod cycles for vtl.ko (QA). Requires rebuilt ko with rmmod quiesce fixes.
# Usage: sudo sh vtl-ko-insmod-stress.sh /path/to/vtl.ko
#
# Prefer vtl-kernelctl stop/start in production; raw rmmod while scans run can panic on old ko.

set -eu

KO="${1:-/opt/vtladm/ko/vtl.ko}"
CYCLE_SLEEP="${VTL_STRESS_SLEEP_SEC:-120}"
RMMOD_QUIESCE_MS="${VTL_RMMOD_QUIESCE_MS:-5000}"

if [ ! -f "$KO" ]; then
  echo "missing ko: $KO" >&2
  exit 1
fi

_insmod_spec() {
  _spec="$1"
  echo "=== insmod vtl_instances=${_spec} ==="
  # shellcheck disable=SC2086
  insmod "$KO" vtl_instances="${_spec}" tape_dir=/opt/vtladm/var/tapes \
    scan_delay_ms=500 bringup_stagger_ms=400 scan_host_stagger_ms=3000 \
    scan_async_quiesce_ms=1000 rmmod_quiesce_ms="${RMMOD_QUIESCE_MS}"
  echo "sleep ${CYCLE_SLEEP}s (bringup/scan)..."
  sleep "$CYCLE_SLEEP"
}

_rmmod_vtl() {
  echo "=== rmmod vtl ==="
  if ! lsmod | awk '{print $1}' | grep -qx vtl; then
    echo "vtl not loaded"
    return 0
  fi
  rmmod vtl
  echo "sleep ${CYCLE_SLEEP}s after rmmod..."
  sleep "$CYCLE_SLEEP"
}

_specs="
2x8,2x8,2x8,2x8,2x8,2x8
2x8,2x8,2x8,2x8,2x8
2x8,2x8,2x8,2x8
2x8,2x8,2x8
2x8,2x8
2x8
2x32
2x32,2x32
2x32,2x32,2x32
2x32,2x32,2x32,2x32
2x32,2x32,2x32,2x32,2x32
"

for _spec in $_specs; do
  _insmod_spec "$_spec"
  _rmmod_vtl
done

echo "stress complete"
