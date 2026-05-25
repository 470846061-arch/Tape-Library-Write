#!/bin/sh
# Shared helpers for safe vtl.ko unload/reload (install.sh, uninstall.sh, vtl-kernelctl).
# Kylin/openEuler: rmmod while install.sh still forks (cargo/systemd) can trigger delayed
# slab corruption → GPF in clone/dup_task_struct. Always stop timers and wait after rmmod.
#
# Usage (after sourcing vtl-source-defaults.sh and vtl-scsi-holders.sh when present):
#   vtl_vtladm_timers_stop
#   vtl_preflight_before_rmmod
#   vtl_safe_rmmod [kernelctl_path]
#   vtl_safe_reload [kernelctl_path]
#   vtl_vtladm_timers_start
set -eu

PREFIX="${PREFIX:-/opt/vtladm}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

_vtl_source_helpers() {
  if [ -f "$SCRIPT_DIR/vtl-source-defaults.sh" ]; then
    # shellcheck source=/dev/null
    . "$SCRIPT_DIR/vtl-source-defaults.sh"
  fi
  if [ -f "$SCRIPT_DIR/vtl-scsi-holders.sh" ]; then
    # shellcheck source=/dev/null
    . "$SCRIPT_DIR/vtl-scsi-holders.sh"
    return 0
  fi
  if [ -f "$PREFIX/scripts/vtl-source-defaults.sh" ]; then
    # shellcheck source=/dev/null
    . "$PREFIX/scripts/vtl-source-defaults.sh"
    # shellcheck source=/dev/null
    . "$PREFIX/scripts/vtl-scsi-holders.sh"
    return 0
  fi
  return 1
}

vtl_loaded() {
  lsmod 2>/dev/null | awk '{print $1}' | grep -qx vtl
}

vtl_vtladm_timers_stop() {
  command -v systemctl >/dev/null 2>&1 || return 0
  systemctl stop vtl-robot-sync.timer vtl-robot-sync.service \
    vtl-patrol.timer vtl-patrol.service 2>/dev/null || true
  echo "stopped vtl-robot-sync.timer and vtl-patrol.timer (if present)"
}

vtl_vtladm_timers_start() {
  command -v systemctl >/dev/null 2>&1 || return 0
  systemctl enable --now vtl-patrol.timer 2>/dev/null || true
  if [ -f "${VTL_CONF_PATH:-$PREFIX/var/vtl.conf}" ] \
    && grep -qE '^[[:space:]]*robot_sync[[:space:]]*=[[:space:]]*true' \
    "${VTL_CONF_PATH:-$PREFIX/var/vtl.conf}" 2>/dev/null \
    && ! grep -qE '^[[:space:]]*auto_sync_db_from_kernel[[:space:]]*=[[:space:]]*false' \
    "${VTL_CONF_PATH:-$PREFIX/var/vtl.conf}" 2>/dev/null; then
    systemctl enable --now vtl-robot-sync.timer 2>/dev/null || true
  else
    systemctl disable vtl-robot-sync.timer 2>/dev/null || true
  fi
}

vtl_teardown_lio_before_rmmod() {
  if command -v targetcli >/dev/null 2>&1; then
    echo "targetcli clearconfig (LIO pscsi/iscsi before rmmod)..."
    if timeout 120 targetcli clearconfig confirm=true; then
      sleep 3
      if _vtl_source_helpers && lio_pscsi_references_vtl_sg; then
        if [ "${VTL_FORCE_RMMOD:-}" = "1" ]; then
          echo "WARN: LIO still references VTL /dev/sg after clearconfig" >&2
        else
          echo "ERROR: LIO still references VTL /dev/sg — aborting rmmod" >&2
          echo "  Run: targetcli clearconfig confirm=true or Web library-unexport" >&2
          return 1
        fi
      fi
    else
      if [ "${VTL_FORCE_RMMOD:-}" != "1" ]; then
        echo "ERROR: targetcli clearconfig failed" >&2
        return 1
      fi
    fi
  elif _vtl_source_helpers; then
    if lio_pscsi_references_vtl_sg; then
      echo "ERROR: LIO pscsi references VTL /dev/sg (no targetcli on PATH)" >&2
      return 1
    fi
  fi
  return 0
}

vtl_preflight_before_rmmod() {
  if [ "${VTL_FORCE_RMMOD:-}" = "1" ]; then
    echo "WARN: VTL_FORCE_RMMOD=1 — skipping holder/LIO preflight" >&2
    return 0
  fi
  if ! _vtl_source_helpers; then
    echo "WARN: vtl-scsi-holders.sh not found — cannot verify device holders" >&2
    return 0
  fi
  if refuse_rmmod_vtl_safety; then
    check_vtl_scsi_holders
    _rc=$?
    if [ "$_rc" -eq 2 ]; then
      echo "ERROR: install psmisc (fuser) to verify holders before rmmod" >&2
      return 1
    fi
    echo "ERROR: VTL /dev/st*|sg*|sch*|ch* in use — stop backup, unexport LIO, then retry" >&2
    return 1
  fi
  if lio_pscsi_references_vtl_sg; then
    echo "ERROR: LIO pscsi still references VTL /dev/sg" >&2
    return 1
  fi
  return 0
}

# Schedule host reboot (cleanest after rmmod on Kylin/openEuler — avoids delayed GPF in install.sh).
vtl_schedule_system_reboot() {
  _reason="${1:-vtl kernel maintenance}"
  _delay="${VTL_REBOOT_DELAY_SEC:-10}"
  echo ">> system reboot in ${_delay}s (${_reason})"
  echo "   Press Ctrl+C now to abort reboot"
  sleep "$_delay"
  sync 2>/dev/null || true
  if command -v systemctl >/dev/null 2>&1; then
    exec systemctl reboot
  fi
  if command -v reboot >/dev/null 2>&1; then
    exec reboot
  fi
  echo "ERROR: no systemctl/reboot command — reboot manually" >&2
  return 1
}

# Post-rmmod settle (default from module param rmmod_quiesce_ms, overridable).
vtl_post_rmmod_wait() {
  if [ "${VTL_SKIP_POST_RMMOD_WAIT:-}" = "1" ]; then
    echo "skipping post-rmmod wait (reboot scheduled)"
    return 0
  fi
  _post="${1:-}"
  if [ -z "$_post" ]; then
    if _vtl_source_helpers; then
      _ms=$(vtl_sysfs_int_param rmmod_quiesce_ms 12000)
      _post=$((_ms / 1000 + 5))
    else
      _post="${VTL_POST_RMMOD_WAIT_SEC:-20}"
    fi
    [ "$_post" -lt 15 ] 2>/dev/null && _post=15
    [ "$_post" -gt 120 ] 2>/dev/null && _post=120
  fi
  echo "waiting ${_post}s after vtl rmmod (kernel SCSI/st/ch teardown)..."
  sleep "$_post"
}

# Direct rmmod (never call vtl-kernelctl stop — avoids recursion from cmd_stop/reload).
vtl_safe_rmmod_direct() {
  if ! vtl_loaded; then
    echo "vtl: not loaded"
    return 0
  fi
  if _vtl_source_helpers; then
    echo "removing VTL SCSI devices via sysfs before rmmod..."
    delete_vtl_scsi_devices || true
    if [ "${VTL_FORCE_RMMOD:-}" != "1" ] && vtl_scsi_devices_remain; then
      echo "WARN: sysfs delete incomplete — retrying delete before rmmod wait" >&2
      delete_vtl_scsi_devices || true
    fi
    vtl_wait_before_rmmod
    if [ "${VTL_FORCE_RMMOD:-}" != "1" ] && vtl_scsi_devices_remain; then
      echo "ERROR: VTL SCSI devices still present after sysfs delete (see lsscsi / host proc_name VTL)" >&2
      vtl_diagnose_rmmod_failure
      return 1
    fi
  fi
  if ! command -v rmmod >/dev/null 2>&1; then
    echo "ERROR: rmmod not found" >&2
    return 1
  fi
  _rmmod_try=0
  while [ "$_rmmod_try" -lt 5 ]; do
    _rmmod_try=$((_rmmod_try + 1))
    if rmmod vtl 2>/dev/null; then
      break
    fi
    if [ "$_rmmod_try" -ge 5 ]; then
      echo "ERROR: rmmod vtl failed" >&2
      if _vtl_source_helpers; then
        vtl_diagnose_rmmod_failure
      fi
      return 1
    fi
    echo "WARN: rmmod vtl failed (try ${_rmmod_try}/5) — retry after scsi delete" >&2
    if _vtl_source_helpers; then
      delete_vtl_scsi_devices || true
      vtl_wait_before_rmmod
      sleep 3
    else
      sleep 5
    fi
  done
  echo "vtl: unloaded"
  vtl_post_rmmod_wait
  if _vtl_source_helpers && vtl_dmesg_recent_panic; then
    echo "ERROR: kernel panic/oops detected after vtl rmmod — see /var/crash and dmesg" >&2
    echo "  Do not start vtl or run install until the host is stable." >&2
    return 1
  fi
  return 0
}

vtl_safe_rmmod() {
  _ctl="${1:-${PREFIX}/sbin/vtl-kernelctl}"
  if ! vtl_loaded; then
    echo "vtl: not loaded"
    return 0
  fi
  vtl_vtladm_timers_stop
  vtl_teardown_lio_before_rmmod || return 1
  vtl_preflight_before_rmmod || return 1
  vtl_safe_rmmod_direct || return 1
  return 0
}

vtl_safe_reload() {
  _ctl="${1:-${PREFIX}/sbin/vtl-kernelctl}"
  vtl_vtladm_timers_stop
  if _vtl_source_helpers; then
    vtl_dmesg_mark || true
  fi
  if vtl_loaded; then
    vtl_safe_rmmod "$_ctl" || return 1
  fi
  if [ -x "$_ctl" ]; then
    if ! "$_ctl" start; then
      echo "ERROR: vtl-kernelctl start failed" >&2
      return 1
    fi
  else
    echo "ERROR: missing $_ctl" >&2
    return 1
  fi
  if _vtl_source_helpers && vtl_dmesg_recent_panic; then
    echo "ERROR: kernel panic/oops after vtl start — see /var/crash and dmesg" >&2
    return 1
  fi
  command -v systemctl >/dev/null 2>&1 && systemctl restart vtladm-web.service 2>/dev/null || true
  vtl_vtladm_timers_start
  echo "vtl: safe reload completed"
  return 0
}
