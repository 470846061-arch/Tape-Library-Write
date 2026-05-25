#!/bin/sh
# Uninstall vtladm installed by install.sh (default PREFIX=/opt/vtladm).
#
# Usage:
#   sudo sh uninstall.sh              # stop units, remove binaries & ko; keep data under PREFIX/var
#   sudo sh uninstall.sh --purge      # also remove PREFIX/var (db, tapes, logs, vtl.conf)
#   sudo sh uninstall.sh --remove-etc # also remove /etc/default/vtladm
#   PREFIX=/opt/vtladm sudo sh uninstall.sh
#
# Kernel uninstall (default): safe rmmod → system reboot (recommended on Kylin/openEuler).
# Stops timers/LIO, sysfs delete VTL SCSI nodes, removes ko under /lib/modules/.../extra/.
# Env: VTL_FORCE_RMMOD=1  skip holder/LIO checks (panic risk)
#      VTL_NO_REBOOT=1 | --no-reboot  keep host up after successful rmmod (no reboot at end)
#      VTL_UNINSTALL_CONTINUE_ON_RMMOD_FAIL=1  with --no-reboot: purge files even if rmmod fails
#      VTL_REBOOT_DELAY_SEC=10
set -eu

PREFIX="${PREFIX:-/opt/vtladm}"
PURGE_DATA=0
REMOVE_ETC=0
REBOOT_AFTER_KERNEL=1
_VTL_DID_RMMOD=0
_VTL_SAFE_SOURCED=0

for _arg in "$@"; do
  case "$_arg" in
    --purge) PURGE_DATA=1 ;;
    --remove-etc) REMOVE_ETC=1 ;;
    --no-reboot) REBOOT_AFTER_KERNEL=0 ;;
    --reboot) REBOOT_AFTER_KERNEL=1 ;;
    -h|--help)
      echo "usage: $0 [--purge] [--remove-etc] [--no-reboot]"
      echo "  PREFIX=$PREFIX"
      echo "  default (kernel): safe rmmod vtl.ko, remove files, then reboot host"
      echo "  --no-reboot: keep host up after rmmod (long post-rmmod wait; not recommended)"
      echo "  --purge: delete $PREFIX/var (vtl.conf, db, tapes, logs)"
      echo "  --remove-etc: delete /etc/default/vtladm"
      echo "  Env: VTL_FORCE_RMMOD=1  VTL_NO_REBOOT=1  VTL_UNINSTALL_CONTINUE_ON_RMMOD_FAIL=1"
      echo "       VTL_REBOOT_DELAY_SEC=10"
      exit 0
      ;;
    *)
      echo "unknown option: $_arg" >&2
      exit 2
      ;;
  esac
done

if [ "${VTL_NO_REBOOT:-}" = "1" ]; then
  REBOOT_AFTER_KERNEL=0
fi

if [ "$(id -u)" -ne 0 ] 2>/dev/null; then
  echo "uninstall.sh: run as root (sudo)" >&2
  exit 1
fi

echo "=== vtladm uninstall PREFIX=$PREFIX ==="

_VTL_RMMOD_FAILED=0

_vtl_uninstall_abort() {
  echo "ERROR: $1" >&2
  echo "  Aborting uninstall — forced rmmod while LIO/backup holds VTL nodes can kernel panic (kdump)." >&2
  echo "  1) targetcli clearconfig confirm=true  (or Web library-unexport per library)" >&2
  echo "  2) stop MBA/backup agents; fuser -v on VTL /dev/sg* /dev/st* /dev/sch*" >&2
  echo "  3) ensure sysfs delete runs (host proc_name must be vtl — update vtl-scsi-holders.sh if old)" >&2
  echo "  4) lsscsi -g | grep -i vtl  should be empty before rmmod" >&2
  echo "  No reboot, files only: VTL_UNINSTALL_CONTINUE_ON_RMMOD_FAIL=1 sudo sh uninstall.sh --purge --no-reboot" >&2
  echo "  Panic risk: VTL_FORCE_RMMOD=1 sudo sh uninstall.sh" >&2
  exit 1
}

if command -v systemctl >/dev/null 2>&1; then
  systemctl stop vtladm-web.service vtl-kernel.service 2>/dev/null || true
  for _u in vtladm-web.service vtl-kernel.service \
    vtl-patrol.timer vtl-patrol.service \
    vtl-robot-sync.timer vtl-robot-sync.service; do
    systemctl stop "$_u" 2>/dev/null || true
    systemctl disable "$_u" 2>/dev/null || true
    rm -f "/etc/systemd/system/$_u"
  done
  systemctl daemon-reload 2>/dev/null || true
  echo "systemd units stopped and disabled"
fi

if [ -f "$PREFIX/scripts/vtl-kernel-safe.sh" ]; then
  # shellcheck source=/dev/null
  . "$PREFIX/scripts/vtl-kernel-safe.sh"
  _VTL_SAFE_SOURCED=1
  vtl_vtladm_timers_stop
  if lsmod 2>/dev/null | awk '{print $1}' | grep -qx vtl; then
    [ "$REBOOT_AFTER_KERNEL" -eq 1 ] && export VTL_SKIP_POST_RMMOD_WAIT=1
    echo ">> safe vtl.ko unload$([ "$REBOOT_AFTER_KERNEL" -eq 1 ] && echo '; host will reboot')"
    if ! vtl_safe_rmmod "$PREFIX/sbin/vtl-kernelctl"; then
      _VTL_RMMOD_FAILED=1
      if [ "$REBOOT_AFTER_KERNEL" -eq 0 ] \
        && [ "${VTL_UNINSTALL_CONTINUE_ON_RMMOD_FAIL:-}" = "1" ]; then
        echo "WARN: vtl safe rmmod failed — continuing file removal (VTL_UNINSTALL_CONTINUE_ON_RMMOD_FAIL=1)" >&2
        echo "WARN: vtl.ko still loaded; reboot or fix holders then: rmmod vtl" >&2
      else
        _vtl_uninstall_abort "vtl safe rmmod failed"
      fi
    else
      _VTL_DID_RMMOD=1
    fi
  else
    echo "vtl: not loaded"
  fi
elif [ -x "$PREFIX/sbin/vtl-kernelctl" ]; then
  if ! "$PREFIX/sbin/vtl-kernelctl" stop; then
    _vtl_uninstall_abort "vtl-kernelctl stop failed"
  fi
  sleep "${VTL_POST_RMMOD_WAIT_SEC:-20}"
elif command -v rmmod >/dev/null 2>&1; then
  if lsmod 2>/dev/null | awk '{print $1}' | grep -qx vtl; then
    if [ -f "$PREFIX/scripts/vtl-scsi-holders.sh" ]; then
      # shellcheck source=/dev/null
      . "$PREFIX/scripts/vtl-scsi-holders.sh"
      if [ "${VTL_FORCE_RMMOD:-}" != "1" ]; then
        if refuse_rmmod_vtl_safety || lio_pscsi_references_vtl_sg; then
          _vtl_uninstall_abort "VTL devices or LIO still in use"
        fi
      fi
      delete_vtl_scsi_devices || true
    fi
    if ! rmmod vtl; then
      _vtl_uninstall_abort "rmmod vtl failed"
    fi
    _VTL_DID_RMMOD=1
    if [ "$REBOOT_AFTER_KERNEL" -eq 0 ]; then
      sleep "${VTL_POST_RMMOD_WAIT_SEC:-20}"
    fi
  fi
else
  echo "WARN: no vtl-kernelctl — assuming vtl.ko not loaded" >&2
fi

_krel=$(uname -r 2>/dev/null || echo "")
if [ -n "$_krel" ] && [ -f "/lib/modules/${_krel}/extra/vtl.ko" ]; then
  rm -f "/lib/modules/${_krel}/extra/vtl.ko"
  command -v depmod >/dev/null 2>&1 && depmod -a "$_krel" 2>/dev/null || depmod -a 2>/dev/null || true
  echo "removed /lib/modules/${_krel}/extra/vtl.ko"
fi

rm -f /usr/local/bin/vtladm /usr/local/bin/vtladm-iscsi /usr/local/sbin/vtl-kernelctl 2>/dev/null || true

if [ "$PURGE_DATA" -eq 1 ]; then
  if [ -d "$PREFIX/var" ]; then
    echo "purging $PREFIX/var"
    rm -rf "$PREFIX/var"
  fi
else
  echo "keeping data: $PREFIX/var (db, tapes, logs, vtl.conf)"
fi

for _d in bin ko sbin scripts docs lib; do
  [ -d "$PREFIX/$_d" ] && rm -rf "$PREFIX/$_d"
done
# remove PREFIX if empty
if [ -d "$PREFIX" ]; then
  rmdir "$PREFIX" 2>/dev/null && echo "removed empty $PREFIX" || echo "left $PREFIX (var/ or other files remain)"
fi

if [ "$REMOVE_ETC" -eq 1 ] && [ -f /etc/default/vtladm ]; then
  rm -f /etc/default/vtladm
  echo "removed /etc/default/vtladm"
fi

if [ "$_VTL_RMMOD_FAILED" -eq 1 ] && [ "$REBOOT_AFTER_KERNEL" -eq 0 ]; then
  echo "done (vtl.ko still loaded — stop backup agents and rmmod vtl, or reboot)."
  exit 0
fi

if [ "$_VTL_DID_RMMOD" -eq 1 ] && [ "$REBOOT_AFTER_KERNEL" -eq 1 ]; then
  if [ "$_VTL_SAFE_SOURCED" -eq 1 ]; then
    vtl_schedule_system_reboot "vtladm uninstall: vtl.ko removed"
  else
    echo ">> rebooting in ${VTL_REBOOT_DELAY_SEC:-10}s (vtl.ko removed)"
    sleep "${VTL_REBOOT_DELAY_SEC:-10}"
    sync 2>/dev/null || true
    if command -v systemctl >/dev/null 2>&1; then
      exec systemctl reboot
    fi
    if command -v reboot >/dev/null 2>&1; then
      exec reboot
    fi
    echo "ERROR: no systemctl/reboot — reboot manually after uninstall" >&2
    exit 1
  fi
fi

echo "done."
