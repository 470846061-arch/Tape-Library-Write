#!/bin/sh
# Optional hook for vtladm: reload vtl.ko with geometry matching all online libraries.
# Configure in vtl.conf: kernel_vtl_reload_script=/path/to/vtl-kernel-reload.sh
# Argument $1: comma-separated "drivesxslots" per library, e.g. "2x32,1x10"
#
# vtladm does NOT insmod from the Web UI; it runs: /bin/sh this_script <spec>
# and may set env: VTL_KO, VTL_SCAN_DELAY_MS (vtl.conf vtl_reload_scan_delay_ms), VTL_POST_ADD_SCAN_DELAY_MS, VTL_BRINGUP_STAGGER_MS, VTL_SCAN_HOST_STAGGER_MS.
# To skip vtladm's automatic invocation without editing vtl.conf: set VTL_SKIP_KERNEL_RELOAD=1 for the vtladm/vtladm serve process (see docs/SCSI.md).
#
# Safety (see userspace/docs/SCSI.md §1c):
# - Refuses rmmod when fuser reports holders on /dev/st*|sg*|sch*|ch* (VTL_FORCE_RMMOD=1 overrides).
# - Default: plain "insmod" (no -f). Set VTL_INSMOD_FORCE=1 for legacy "insmod -f".
# - Pause after rmmod: VTL_RELOAD_SLEEP_SEC (default 2).
# - insmod passes scan_delay_ms=… (default 500 ms), post_add_scan_delay_ms=… (default 600 ms after add_host),
#   bringup_stagger_ms=… (default 400 ms per extra host), scan_host_stagger_ms=… (default 3000 ms per host index before scsi_scan_host). Override: VTL_SCAN_DELAY_MS, VTL_POST_ADD_SCAN_DELAY_MS, VTL_BRINGUP_STAGGER_MS, VTL_SCAN_HOST_STAGGER_MS.
# - Set VTL_NOSCAN=1 to also pass noscan=1 (manual scan only; debugging).
# CRLF breaks sh: run  sed -i 's/\r$//'  on this file if you see /bin/sh^M or EOF errors.

if [ -z "${1:-}" ]; then
  echo "usage: $0 <vtl_instances>" >&2
  echo "  example: $0 '2x32,2x32'" >&2
  exit 2
fi

_SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=vtl-scsi-holders.sh
. "$_SCRIPT_DIR/vtl-scsi-holders.sh"

SPEC="$1"
KREL="$(uname -r)"
VTL_KO="${VTL_KO:-/lib/modules/${KREL}/extra/vtl.ko}"
SLEEP_SEC="${VTL_RELOAD_SLEEP_SEC:-2}"
SCAN_MS="${VTL_SCAN_DELAY_MS:-500}"
POST_ADD_SCAN_MS="${VTL_POST_ADD_SCAN_DELAY_MS:-600}"
BRINGUP_STAGGER_MS="${VTL_BRINGUP_STAGGER_MS:-400}"
SCAN_HOST_STAGGER_MS="${VTL_SCAN_HOST_STAGGER_MS:-3000}"

if ! command -v insmod >/dev/null 2>&1; then
  echo "insmod not found" >&2
  exit 1
fi

if refuse_rmmod_vtl_safety; then
  check_vtl_scsi_holders
  _hrc=$?
  if [ "${VTL_FORCE_RMMOD:-}" = "1" ]; then
    echo "WARN: VTL_FORCE_RMMOD=1 — rmmod despite holder check" >&2
  elif [ "$_hrc" -eq 2 ]; then
    echo "refusing rmmod: fuser not installed (install psmisc); use /dev/vtl SET_INSTANCES ioctl instead" >&2
    exit 1
  else
    echo "refusing rmmod: processes hold VTL /dev/st*|sg*|sch* (see lsscsi -g; use ioctl or stop holders)" >&2
    echo "override: VTL_FORCE_RMMOD=1 (unsafe; Kylin kdump risk)" >&2
    exit 1
  fi
fi

rmmod vtl 2>/dev/null || true
sleep "$SLEEP_SEC" || sleep 2

if [ ! -f "$VTL_KO" ]; then
  echo "vtl.ko not found at $VTL_KO (set VTL_KO to the built module path)" >&2
  exit 1
fi

if [ "${VTL_NOSCAN:-}" = "1" ]; then
  if [ "${VTL_INSMOD_FORCE:-}" = "1" ]; then
    insmod -f "$VTL_KO" "vtl_instances=${SPEC}" "scan_delay_ms=${SCAN_MS}" "post_add_scan_delay_ms=${POST_ADD_SCAN_MS}" "bringup_stagger_ms=${BRINGUP_STAGGER_MS}" "scan_host_stagger_ms=${SCAN_HOST_STAGGER_MS}" noscan=1 || {
      echo "insmod -f failed" >&2
      exit 1
    }
  else
    insmod "$VTL_KO" "vtl_instances=${SPEC}" "scan_delay_ms=${SCAN_MS}" "post_add_scan_delay_ms=${POST_ADD_SCAN_MS}" "bringup_stagger_ms=${BRINGUP_STAGGER_MS}" "scan_host_stagger_ms=${SCAN_HOST_STAGGER_MS}" noscan=1 || {
      echo "insmod failed (vermagic mismatch? build vtl.ko for this kernel, or set VTL_INSMOD_FORCE=1 if you accept the risk)" >&2
      exit 1
    }
  fi
else
  if [ "${VTL_INSMOD_FORCE:-}" = "1" ]; then
    insmod -f "$VTL_KO" "vtl_instances=${SPEC}" "scan_delay_ms=${SCAN_MS}" "post_add_scan_delay_ms=${POST_ADD_SCAN_MS}" "bringup_stagger_ms=${BRINGUP_STAGGER_MS}" "scan_host_stagger_ms=${SCAN_HOST_STAGGER_MS}" || {
      echo "insmod -f failed" >&2
      exit 1
    }
  else
    insmod "$VTL_KO" "vtl_instances=${SPEC}" "scan_delay_ms=${SCAN_MS}" "post_add_scan_delay_ms=${POST_ADD_SCAN_MS}" "bringup_stagger_ms=${BRINGUP_STAGGER_MS}" "scan_host_stagger_ms=${SCAN_HOST_STAGGER_MS}" || {
      echo "insmod failed (vermagic mismatch? build vtl.ko for this kernel, or set VTL_INSMOD_FORCE=1 if you accept the risk)" >&2
      exit 1
    }
  fi
fi
