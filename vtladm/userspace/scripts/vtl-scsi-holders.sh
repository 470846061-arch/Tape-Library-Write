#!/bin/sh
# Shared VTL SCSI device holder checks (fuser on vtl.ko SCSI hosts).
# Hosts are identified by scsi_host proc_name "VTL", not INQUIRY vendor (ibm/stk/hp).
# Sourced by vtl-kernel-reload.sh, vtl-kernel-safe.sh, vtl-kernel-stability.sh.
#
# check_vtl_scsi_holders [log_file]
#   Exit 0 = no holders on VTL nodes (or no VTL rows)
#   Exit 1 = holder present
#   Exit 2 = fuser not installed (fail-closed for rmmod)

# Kernel sets scsi_host_template.proc_name to "vtl" (lowercase); accept legacy "VTL" too.
vtl_host_proc_name_ok() {
  _pn="$1"
  case "$_pn" in vtl|VTL) return 0 ;; esac
  return 1
}

# Host numbers whose /sys/class/scsi_host/hostN/proc_name is vtl (module sht.name).
vtl_scsi_host_nums() {
  for _hdir in /sys/class/scsi_host/host*; do
    [ -d "$_hdir" ] || continue
    [ -r "$_hdir/proc_name" ] || continue
    _pn=$(tr -d '\n\r ' <"$_hdir/proc_name" 2>/dev/null) || continue
    if vtl_host_proc_name_ok "$_pn"; then
      printf '%s\n' "${_hdir##*host}"
    fi
  done
}

# HCTL strings (e.g. 5:0:0:0) for all LUNs on VTL SCSI hosts.
vtl_scsi_hctl_list() {
  for _n in $(vtl_scsi_host_nums); do
    for _pdev in /sys/bus/scsi/devices/"${_n}":*; do
      [ -e "$_pdev" ] || continue
      basename "$_pdev"
    done
  done
}

# Exit 0 if any VTL-module SCSI device still exists in sysfs.
vtl_scsi_devices_remain() {
  [ -n "$(vtl_scsi_hctl_list | head -1)" ]
}

vtl_scsi_dev_paths() {
  _seen=""
  for _hctl in $(vtl_scsi_hctl_list); do
    _base="/sys/class/scsi_device/${_hctl}/device"
    [ -d "$_base" ] || continue
    for _g in "$_base"/scsi_generic/sg*; do
      [ -e "$_g" ] || continue
      _d="/dev/$(basename "$_g")"
      case " $_seen " in *" $_d "*) continue ;; esac
      _seen="$_seen $_d"
      echo "$_d"
    done
    for _t in "$_base"/scsi_tape/st* "$_base"/scsi_tape/nst*; do
      [ -e "$_t" ] || continue
      _d="/dev/$(basename "$_t")"
      case " $_seen " in *" $_d "*) continue ;; esac
      _seen="$_seen $_d"
      echo "$_d"
    done
    for _c in "$_base"/scsi_changer/sch*; do
      [ -e "$_c" ] || continue
      _d="/dev/$(basename "$_c")"
      case " $_seen " in *" $_d "*) continue ;; esac
      _seen="$_seen $_d"
      echo "$_d"
    done
  done
  if ! command -v lsscsi >/dev/null 2>&1; then
    return 0
  fi
  for _hctl in $(vtl_scsi_hctl_list); do
    lsscsi -g 2>/dev/null | awk -v h="$_hctl" '
      $1 ~ ("^\\[" h) {
        for (i = 1; i <= NF; i++)
          if ($i ~ /^\/dev\//) print $i
      }'
  done
}

check_vtl_scsi_holders() {
  _log="${1:-}"
  if ! command -v fuser >/dev/null 2>&1; then
    if [ -n "$_log" ]; then
      echo "### $(date -Iseconds 2>/dev/null || date) fuser missing (install psmisc)" >>"$_log"
    fi
    return 2
  fi
  if [ -n "$_log" ]; then
    echo "### $(date -Iseconds 2>/dev/null || date) VTL holder check (lsscsi paths)" >>"$_log"
  fi
  _found=0
  for _d in $(vtl_scsi_dev_paths); do
    [ -e "$_d" ] || continue
    if fuser "$_d" >/dev/null 2>&1; then
      _found=1
      if [ -n "$_log" ]; then
        {
          echo "=== $_d (in use) ==="
          fuser -v "$_d" 2>/dev/null || true
        } >>"$_log"
      fi
    fi
  done
  [ "$_found" -eq 1 ] && return 1
  return 0
}

# Read a vtl.ko sysfs int parameter (digits only); echo default if missing/invalid.
vtl_sysfs_int_param() {
  _name="$1"
  _def="${2:-0}"
  if [ ! -r "/sys/module/vtl/parameters/${_name}" ]; then
    printf '%s' "$_def"
    return 0
  fi
  _v=$(tr -d '\n\r ' </sys/module/vtl/parameters/"${_name}" 2>/dev/null) || _v=""
  case "$_v" in
    ''|*[!0-9]*) printf '%s' "$_def" ;;
    *) printf '%s' "$_v" ;;
  esac
}

# Pre-rmmod settle (match vtl-kernelctl; avoids GPF/slab faults during scsi_remove_host).
vtl_wait_before_rmmod() {
  _wait=15
  if [ -r /sys/module/vtl/parameters/vtl_instances ]; then
    _n=$(tr ',' '\n' </sys/module/vtl/parameters/vtl_instances | grep -c . 2>/dev/null || echo 1)
    _wait=$((_n * 3 + 15))
    [ "$_wait" -gt 45 ] && _wait=45
  fi
  _async=$(vtl_sysfs_int_param scan_async_quiesce_ms 5000)
  _sec=$((_async / 1000 + 5))
  [ "$_sec" -gt "$_wait" ] && _wait=$_sec
  echo "waiting ${_wait}s for VTL SCSI bringup/scan to quiesce before rmmod..."
  sleep "$_wait"
}

# Post-insmod settle: scan_async_quiesce_ms runs after scsi_scan_host returns, but st/sg
# uevents may still fire during scan. Wait for VTL nodes + extra margin before backup/LIO.
vtl_post_insmod_settle() {
  _wait=8
  _async=$(vtl_sysfs_int_param scan_async_quiesce_ms 5000)
  _sec=$((_async / 1000 + 5))
  [ "$_sec" -gt "$_wait" ] && _wait=$_sec
  [ "$_wait" -gt 60 ] && _wait=60
  echo "waiting ${_wait}s for VTL SCSI scan/settle after insmod..."
  _deadline=$(( $(date +%s 2>/dev/null || echo 0) + _wait ))
  while :; do
    if vtl_scsi_devices_remain; then
      break
    fi
    _now=$(date +%s 2>/dev/null || echo 0)
    [ "$_now" -ge "$_deadline" ] && break
    sleep 1
  done
  _remain=$(( _deadline - $(date +%s 2>/dev/null || echo 0) ))
  if [ "$_remain" -gt 0 ] 2>/dev/null; then
    sleep "$_remain"
  fi
}

# Snapshot dmesg line count before insmod/reload (call once per operation).
vtl_dmesg_mark() {
  if ! dmesg >/dev/null 2>&1; then
    VTL_DMESG_MARK_LINES=0
    return 1
  fi
  VTL_DMESG_MARK_LINES=$(dmesg 2>/dev/null | wc -l | tr -d ' ')
  export VTL_DMESG_MARK_LINES
  return 0
}

# Exit 0 if new kernel log since vtl_dmesg_mark() shows panic/oops (install/reload guard).
vtl_dmesg_recent_panic() {
  if ! dmesg >/dev/null 2>&1; then
    return 1
  fi
  _mark="${VTL_DMESG_MARK_LINES:-0}"
  _new=$(dmesg 2>/dev/null | tail -n +$((_mark + 1)))
  if [ -z "$_new" ]; then
    _new=$(dmesg 2>/dev/null | tail -25)
  fi
  printf '%s\n' "$_new" | grep -qiE \
    'general protection fault|kernel BUG|Oops:|BUG: unable to handle'
}

# Post-rmmod settle (st/ch/sg teardown can lag rmmod; delayed slab faults show up on unrelated fork).
vtl_wait_after_rmmod() {
  _post="${1:-}"
  if [ -z "$_post" ]; then
    _post=15
    _ms=$(vtl_sysfs_int_param rmmod_quiesce_ms 12000)
    _post=$((_ms / 1000 + 5))
    [ "$_post" -lt 15 ] && _post=15
    [ "$_post" -gt 45 ] && _post=45
  fi
  echo "waiting ${_post}s after rmmod for kernel SCSI/st/ch teardown..."
  sleep "$_post"
}

# Write delete to sysfs for one HCTL (class path and bus path).
_vtl_scsi_delete_one_hctl() {
  _d="$1"
  _ok=0
  _sys="/sys/class/scsi_device/${_d}/device"
  if [ -d "$_sys" ] && [ -w "$_sys/delete" ] 2>/dev/null; then
    echo 1 >"$_sys/delete" 2>/dev/null && _ok=1
  fi
  _bus="/sys/bus/scsi/devices/${_d}"
  if [ -d "$_bus" ] && [ -w "$_bus/delete" ] 2>/dev/null; then
    echo 1 >"$_bus/delete" 2>/dev/null && _ok=1
  fi
  [ "$_ok" -eq 1 ]
}

# Delete VTL SCSI devices via sysfs (call before rmmod when fuser is clear but module still in use).
delete_vtl_scsi_devices() {
  _pass=0
  _n=0
  while [ "$_pass" -lt 5 ]; do
    _pass=$((_pass + 1))
    _batch=0
    for _d in $(vtl_scsi_hctl_list); do
      if _vtl_scsi_delete_one_hctl "$_d"; then
        _batch=$((_batch + 1))
        _n=$((_n + 1))
      fi
    done
    if [ "$_batch" -eq 0 ]; then
      break
    fi
    _deadline=$(( $(date +%s 2>/dev/null || echo 0) + 20 ))
    while :; do
      if ! vtl_scsi_devices_remain; then
        break
      fi
      _now=$(date +%s 2>/dev/null || echo 0)
      [ "$_now" -ge "$_deadline" ] && break
      sleep 1
    done
    sleep 2
  done
  if [ "$_n" -gt 0 ]; then
    echo "deleted ${_n} VTL SCSI device(s) via sysfs (passes=${_pass})"
  fi
  return 0
}

# Hint after failed rmmod (holders, leftover sysfs, dmesg).
vtl_diagnose_rmmod_failure() {
  echo "vtl rmmod diagnostics:" >&2
  for _hdir in /sys/class/scsi_host/host*; do
    [ -r "$_hdir/proc_name" ] || continue
    _pn=$(tr -d '\n\r ' <"$_hdir/proc_name" 2>/dev/null) || _pn="?"
    _hn="${_hdir##*host}"
    _left=$(ls -1 /sys/bus/scsi/devices/"${_hn}":* 2>/dev/null | wc -l | tr -d ' ')
    echo "  host${_hn} proc_name=${_pn} sysfs_devices=${_left:-0}" >&2
  done
  if vtl_scsi_devices_remain; then
    echo "  VTL SCSI nodes still in sysfs — stop MBA/backup on /dev/sg*/st*/sch* then retry delete" >&2
  fi
  if command -v lsscsi >/dev/null 2>&1; then
    for _h in $(vtl_scsi_host_nums); do
      lsscsi -g 2>/dev/null | grep -F "[$_h" || true
    done >&2
  fi
  if command -v fuser >/dev/null 2>&1; then
    for _d in $(vtl_scsi_dev_paths); do
      [ -e "$_d" ] || continue
      fuser -v "$_d" 2>/dev/null | sed 's/^/  /' >&2 || true
    done
  else
    echo "  install psmisc for fuser holder checks" >&2
  fi
  dmesg 2>/dev/null | tail -15 | sed 's/^/  /' >&2 || true
}

# Exit 0 if LIO pscsi backstores still reference VTL /dev/sg (configfs or targetcli).
lio_pscsi_references_vtl_sg() {
  for _d in $(vtl_scsi_dev_paths); do
    [ -e "$_d" ] || continue
    if [ -d "/sys/kernel/config/target" ]; then
      if grep -rq "$_d" /sys/kernel/config/target 2>/dev/null; then
        return 0
      fi
    fi
  done
  if command -v targetcli >/dev/null 2>&1; then
    _tl=$(timeout 20 targetcli ls /backstores/pscsi 2>/dev/null) || _tl=""
    for _d in $(vtl_scsi_dev_paths); do
      case "$_tl" in *"$_d"*) return 0 ;; esac
    done
  fi
  return 1
}

# Exit 0 = must refuse rmmod (holders or cannot verify). Honors VTL_FORCE_RMMOD=1.
refuse_rmmod_vtl_safety() {
  if [ "${VTL_FORCE_RMMOD:-}" = "1" ]; then
    return 1
  fi
  check_vtl_scsi_holders "${LOG:-}"
  _rc=$?
  case "$_rc" in
    0) return 1 ;;
    1|2) return 0 ;;
  esac
  return 0
}
