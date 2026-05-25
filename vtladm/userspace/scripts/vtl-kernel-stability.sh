#!/bin/sh
# Staged soak / stress checks for vtl.ko stability (Kylin, openEuler, etc.).
#
# Goal: narrow "reboot after some time with vtl loaded" to a specific trigger
# (default insmod vs vtl_instances vs light I/O vs ioctl vs rescan).
#
# Usage (root):
#   sudo sh userspace/scripts/vtl-kernel-stability.sh preflight
#   sudo sh userspace/scripts/vtl-kernel-stability.sh phase-a          # default insmod, soak
#   sudo VTL_INST_SPEC='1x4,2x8' sudo -E sh ... phase-b
#   sudo sh userspace/scripts/vtl-kernel-stability.sh phase-c          # light mt on st*
#   sudo VTL_INST_SPEC='1x4' sudo -E sh ... phase-d                     # SET_INSTANCES ioctl (needs python3)
#   sudo sh userspace/scripts/vtl-kernel-stability.sh phase-e          # SCSI rescan script
#   sudo sh userspace/scripts/vtl-kernel-stability.sh snapshot         # diagnostics only
#   sudo sh userspace/scripts/vtl-kernel-stability.sh status
#
# Environment:
#   VTL_KO              path to vtl.ko (default: ./kernel/vtl.ko or /lib/modules/$(uname -r)/extra/vtl.ko)
#   VTL_INST_SPEC       e.g. 1x4 or 1x4,2x8 (phase-b/d; default 1x4)
#   VTL_SOAK_SEC        soak duration per phase (default 3600 = 1h; you used 25200 for ~7h)
#   VTL_LOG_DIR         log root (default /var/log/vtl-stability)
#   VTL_SKIP_RMMOD      1 = do not rmmod before load (module already present; recommended on Kylin)
#   VTL_FORCE_RMMOD     1 = rmmod even when fuser reports /dev/st*|sg*|sch*|ch* in use (unsafe)
#   VTL_INSMOD_EXTRA    extra insmod/modprobe args (e.g. scan_host_stagger_ms=5000)
#   VTL_POST_INSMOD_WAIT_SEC  seconds to sleep after insmod before lsscsi/dmesg (default 120;
#                         Kylin 4.19 defers scsi_add_host/scan — probing too early can race)
#   VTL_SOAK_POLL_SEC   seconds between soak snapshots in phase-a/b (default 60)
#   VTL_SOAK_NO_SNAPSHOT 1 = soak loop only sleeps, no periodic lsscsi (phase-a-idle)
#   VTL_INTEGRATION_SOAK_SEC  soak per phase in integration-smoke (default 600)
#
# Kylin 4.19 notes (2026-05-16): rmmod with open /dev/st*|sg* can kdump; periodic lsscsi after
# scan is OK. Prefer phase-a-idle + phase-a with VTL_SKIP_RMMOD=1, or release holders before rmmod.
# phase-b: when vtl is loaded and fuser shows holders, applies VTL_INST_SPEC via SET_INSTANCES ioctl
# instead of rmmod+insmod. With VTL_SKIP_RMMOD=1 and vtl already up, soak only (run phase-d first).
#
# On reboot: enable persistent journal + kdump first; then journalctl -b -1 -k.

set -u

PHASE="${1:-}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
# shellcheck source=vtl-scsi-holders.sh
. "$SCRIPT_DIR/vtl-scsi-holders.sh"

VTL_KO="${VTL_KO:-}"
VTL_INST_SPEC="${VTL_INST_SPEC:-1x4}"
VTL_SOAK_SEC="${VTL_SOAK_SEC:-3600}"
VTL_LOG_DIR="${VTL_LOG_DIR:-/var/log/vtl-stability}"
VTL_SKIP_RMMOD="${VTL_SKIP_RMMOD:-0}"
VTL_FORCE_RMMOD="${VTL_FORCE_RMMOD:-0}"
VTL_INSMOD_EXTRA="${VTL_INSMOD_EXTRA:-}"
VTL_INTEGRATION_SOAK_SEC="${VTL_INTEGRATION_SOAK_SEC:-600}"
VTL_POST_INSMOD_WAIT_SEC="${VTL_POST_INSMOD_WAIT_SEC:-120}"
VTL_SOAK_POLL_SEC="${VTL_SOAK_POLL_SEC:-60}"
VTL_SOAK_NO_SNAPSHOT="${VTL_SOAK_NO_SNAPSHOT:-0}"

RUN_ID="$(date +%Y%m%d-%H%M%S)"
LOG="$VTL_LOG_DIR/run-$RUN_ID"
mkdir -p "$LOG"

log() {
  _ts="$(date -Iseconds 2>/dev/null || date)"
  echo "[$_ts] $*" | tee -a "$LOG/run.log"
}

log_env_once() {
  [ -f "$LOG/env.txt" ] && return 0
  {
    echo "RUN_ID=$RUN_ID"
    echo "PHASE=$PHASE"
    echo "VTL_KO=$VTL_KO"
    echo "VTL_INST_SPEC=$VTL_INST_SPEC"
    echo "VTL_SOAK_SEC=$VTL_SOAK_SEC"
    echo "VTL_SKIP_RMMOD=$VTL_SKIP_RMMOD"
    echo "VTL_FORCE_RMMOD=$VTL_FORCE_RMMOD"
    echo "VTL_POST_INSMOD_WAIT_SEC=$VTL_POST_INSMOD_WAIT_SEC"
    echo "VTL_INSMOD_EXTRA=$VTL_INSMOD_EXTRA"
    echo "VTL_INTEGRATION_SOAK_SEC=$VTL_INTEGRATION_SOAK_SEC"
  } >"$LOG/env.txt"
}

die() {
  log "ERROR: $*"
  exit 1
}

need_root() {
  [ "$(id -u)" -eq 0 ] || die "must run as root (sudo)"
}

resolve_vtl_ko() {
  if [ -n "$VTL_KO" ] && [ -f "$VTL_KO" ]; then
    return 0
  fi
  for p in \
    "$REPO_ROOT/kernel/vtl.ko" \
    "/lib/modules/$(uname -r)/extra/vtl.ko" \
    "/lib/modules/$(uname -r)/kernel/drivers/scsi/vtl.ko"
  do
    if [ -f "$p" ]; then
      VTL_KO="$p"
      return 0
    fi
  done
  return 1
}

vtl_loaded() {
  lsmod 2>/dev/null | awk '{print $1}' | grep -qx vtl
}

# Return 0 if any holder on VTL lsscsi paths (or fuser missing — treat as unsafe).
scsi_tape_holders_present() {
  check_vtl_scsi_holders "$LOG/fuser-before-rmmod.txt"
  _rc=$?
  case "$_rc" in
    0) return 1 ;;
    1) return 0 ;;
    2)
      log "WARN: fuser not installed — treating as unsafe to rmmod (install psmisc)"
      return 0
      ;;
  esac
  return 0
}

cmd_check_holders() {
  need_root
  log "=== check-holders: VTL nodes from lsscsi -g (fuser) ==="
  if scsi_tape_holders_present; then
    log "holders present (see $LOG/fuser-before-rmmod.txt)"
    log "stop backup/monitoring, or use VTL_SKIP_RMMOD=1 for probe-only phases"
    exit 1
  fi
  log "no fuser holders on tape/sg devices (safe to rmmod vtl if loaded)"
  vtl_loaded && log "vtl is loaded" || log "vtl not loaded"
}

safe_rmmod() {
  if ! vtl_loaded; then
    log "vtl not loaded, skip rmmod"
    return 0
  fi
  log "rmmod vtl (requires no open /dev/st*|sg*|sch*|ch*)"
  check_vtl_scsi_holders "$LOG/fuser-before-rmmod.txt"
  _hrc=$?
  if [ "$_hrc" -eq 2 ] && [ "$VTL_FORCE_RMMOD" != "1" ]; then
    die "refusing rmmod: fuser not installed (install psmisc). Use ioctl or VTL_SKIP_RMMOD=1"
  fi
  if scsi_tape_holders_present; then
    if [ "$VTL_FORCE_RMMOD" = "1" ]; then
      log "WARN: VTL_FORCE_RMMOD=1 — rmmod despite open VTL tape/sg (Kylin may kdump)"
    else
      die "refusing rmmod: processes hold VTL /dev/st*|sg*|sch* (see $LOG/fuser-before-rmmod.txt). Stop them, or VTL_SKIP_RMMOD=1, or VTL_FORCE_RMMOD=1 (unsafe)"
    fi
  else
    log "no fuser holders on VTL lsscsi paths — proceeding with rmmod"
  fi
  rmmod vtl 2>&1 | tee -a "$LOG/rmmod.txt" || die "rmmod vtl failed"
  sleep 2
}

load_vtl() {
  _extra="$1"
  if vtl_loaded; then
    log "vtl already loaded; skip insmod"
    return 0
  fi
  resolve_vtl_ko || die "vtl.ko not found; set VTL_KO="
  log "insmod $VTL_KO $_extra"
  # shellcheck disable=SC2086
  insmod "$VTL_KO" $_extra 2>&1 | tee -a "$LOG/insmod.txt" || die "insmod failed"
}

# insmod returns before deferred scsi_add_host / scsi_scan_host finish (see vtl_main.c).
wait_after_insmod() {
  _w="$VTL_POST_INSMOD_WAIT_SEC"
  log "waiting ${_w}s after insmod before lsscsi/sysfs probes (VTL_POST_INSMOD_WAIT_SEC)"
  log "tip: on Kylin 4.19, immediate lsscsi after insmod may race deferred scan"
  _i=0
  while [ "$_i" -lt "$_w" ]; do
    _left=$((_w - _i))
    if [ "$((_left % 30))" -eq 0 ] || [ "$_i" -eq 0 ]; then
      log "post-insmod quiesce: ${_left}s left"
    fi
    sleep 1
    _i=$((_i + 1))
  done
}

post_reboot_capture_hint() {
  log "if the host rebooted: journalctl -b -1 -k | tail -200"
  log "if the host rebooted: ls -lt /var/crash | head -5"
  _c="$(ls -td /var/crash/127.0.0.1-* 2>/dev/null | head -1)"
  if [ -n "$_c" ] && [ -f "$_c/vmcore-dmesg.txt" ]; then
    log "panic hint: tail -200 $_c/vmcore-dmesg.txt | grep -E 'Oops|panic|Call Trace|vtl'"
  fi
  log "if the host rebooted: sh $SCRIPT_DIR/vtladm-collect-diagnostics.sh /tmp/vtl-diag.tar.gz"
}

snapshot() {
  _tag="${1:-snapshot}"
  _d="$LOG/$_tag-$(date +%H%M%S)"
  mkdir -p "$_d"
  log "writing snapshot -> $_d"
  {
    echo "phase=$PHASE tag=$_tag"
    echo "VTL_KO=$VTL_KO"
    echo "VTL_INST_SPEC=$VTL_INST_SPEC"
    echo "VTL_SOAK_SEC=$VTL_SOAK_SEC"
    uptime
    uname -a
  } >"$_d/meta.txt"
  free -m >"$_d/free-m.txt" 2>&1 || true
  ( dmesg -T 2>/dev/null || dmesg ) | tail -n 2000 >"$_d/dmesg-tail.txt" 2>&1 || true
  lsscsi -g >"$_d/lsscsi-g.txt" 2>&1 || true
  if vtl_loaded; then
    modinfo vtl >"$_d/modinfo-vtl.txt" 2>&1 || true
    if [ -d /sys/module/vtl/parameters ]; then
      ls -la /sys/module/vtl/parameters >"$_d/vtl-parameters-ls.txt" 2>&1 || true
      for f in /sys/module/vtl/parameters/*; do
        [ -f "$f" ] || continue
        bn="$(basename "$f")"
        cat "$f" >"$_d/param-$bn.txt" 2>&1 || true
      done
    fi
    [ -c /dev/vtl ] && ls -la /dev/vtl >"$_d/dev-vtl.txt" 2>&1 || true
  fi
  if command -v journalctl >/dev/null 2>&1; then
    journalctl -k -n 500 --no-pager >"$_d/journal-k-n500.txt" 2>&1 || true
  fi
}

soak() {
  _label="$1"
  _sec="$2"
  if [ "$VTL_SOAK_NO_SNAPSHOT" = "1" ]; then
    log "SOAK $_label: ${_sec}s idle (no periodic lsscsi/dmesg — VTL_SOAK_NO_SNAPSHOT=1)"
    sleep "$_sec"
    log "SOAK $_label: idle sleep finished"
    return 0
  fi
  log "SOAK $_label: ${_sec}s (snapshot every ${VTL_SOAK_POLL_SEC}s; Ctrl+C to stop early)"
  _start="$(date +%s)"
  _end=$((_start + _sec))
  _i=0
  while [ "$(date +%s)" -lt "$_end" ]; do
    _i=$((_i + 1))
    snapshot "soak-${_label}-${_i}"
    _left=$((_end - $(date +%s)))
    log "soak $_label: ~${_left}s remaining"
    sleep "$VTL_SOAK_POLL_SEC"
  done
  log "SOAK $_label: finished ${_sec}s without script abort"
}

# --- phases ---

cmd_integration_smoke() {
  need_root
  post_reboot_capture_hint
  _soak="$VTL_INTEGRATION_SOAK_SEC"
  log "=== integration-smoke: preflight + A-idle(${_soak}s) + A-probe(${_soak}s) ==="
  log "Never rmmod (VTL_SKIP_RMMOD=1): safe when /dev/st*|sg* held after prior load or system services."
  log "Full 1h soak: VTL_INTEGRATION_SOAK_SEC=3600 … integration-smoke"
  log "Cold start (no vtl): A-idle insmods + waits; warm start (vtl loaded): both phases soak only."
  cmd_preflight
  VTL_SKIP_RMMOD=1 VTL_SOAK_SEC="$_soak" cmd_phase_a_idle
  VTL_SKIP_RMMOD=1 VTL_SOAK_SEC="$_soak" cmd_phase_a
  log "integration-smoke complete — LOG=$LOG"
}

cmd_preflight() {
  need_root
  log "=== preflight ==="
  log "LOG=$LOG"
  resolve_vtl_ko && log "VTL_KO=$VTL_KO" || log "WARN: vtl.ko not found yet"
  if vtl_loaded && scsi_tape_holders_present; then
    log "WARN: vtl loaded and tape/sg devices have fuser holders (rmmod phases will refuse until released)"
  fi
  modinfo "$VTL_KO" 2>/dev/null | grep -E '^(filename|vermagic|version):' | tee -a "$LOG/preflight-modinfo.txt" || true
  log "uname -r: $(uname -r)"
  if command -v journalctl >/dev/null 2>&1; then
    if journalctl --list-boots 2>/dev/null | grep -q .; then
      log "journal: persistent boots visible (good for post-reboot analysis)"
    else
      log "WARN: journal has no boot list; enable Storage=persistent in journald.conf"
    fi
  fi
  if [ -d /var/crash ] && ls /var/crash 2>/dev/null | grep -q .; then
    log "NOTE: /var/crash has entries (check for vmcore)"
  fi
  snapshot preflight
  log "preflight done"
}

cmd_status() {
  log "=== status ==="
  uptime | tee -a "$LOG/run.log"
  vtl_loaded && log "vtl: loaded" || log "vtl: not loaded"
  lsscsi -g | tee -a "$LOG/run.log"
  snapshot status
}

cmd_phase_a() {
  need_root
  post_reboot_capture_hint
  log "=== phase A: default insmod (no vtl_instances), soak ${VTL_SOAK_SEC}s ==="
  log "Runs periodic lsscsi after load. On Kylin: use phase-a-idle first, or VTL_SKIP_RMMOD=1 if vtl already up."
  [ "$VTL_SKIP_RMMOD" = "1" ] || safe_rmmod
  if vtl_loaded; then
    log "vtl already loaded; skip insmod"
    if [ "$VTL_SKIP_RMMOD" = "1" ]; then
      log "VTL_SKIP_RMMOD=1: skip post-insmod wait (scan already done)"
    else
      wait_after_insmod
    fi
  else
    load_vtl "$VTL_INSMOD_EXTRA"
    wait_after_insmod
  fi
  snapshot phase-a-start
  soak phase-a "$VTL_SOAK_SEC"
  snapshot phase-a-end
  log "phase A complete"
}

# Same as phase-a but never rmmod — matches production (ioctl/rescan, no module cycle).
cmd_phase_a_probe() {
  VTL_SKIP_RMMOD=1
  cmd_phase_a
}

# Matches manual test: insmod only, no periodic lsscsi/dmesg during soak.
cmd_phase_a_idle() {
  need_root
  post_reboot_capture_hint
  log "=== phase A-idle: default insmod, soak ${VTL_SOAK_SEC}s without periodic SCSI probes ==="
  [ "$VTL_SKIP_RMMOD" = "1" ] || safe_rmmod
  if vtl_loaded; then
    log "vtl already loaded; skip insmod"
    if [ "$VTL_SKIP_RMMOD" != "1" ]; then
      wait_after_insmod
    else
      log "VTL_SKIP_RMMOD=1: skip post-insmod wait (module already up)"
    fi
  else
    load_vtl "$VTL_INSMOD_EXTRA"
    wait_after_insmod
  fi
  _save="$VTL_SOAK_NO_SNAPSHOT"
  VTL_SOAK_NO_SNAPSHOT=1
  soak phase-a-idle "$VTL_SOAK_SEC"
  VTL_SOAK_NO_SNAPSHOT="$_save"
  snapshot phase-a-idle-end
  log "phase A-idle complete"
}

cmd_phase_reload() {
  need_root
  post_reboot_capture_hint
  log "=== phase reload: safe rmmod (no holders) + insmod + wait + snapshot ==="
  scsi_tape_holders_present && die "holders present — run check-holders, stop services, then retry"
  safe_rmmod
  load_vtl "$VTL_INSMOD_EXTRA"
  wait_after_insmod
  snapshot phase-reload-end
  log "phase reload complete"
}

# Return 0 when phase-b should apply geometry via ioctl (loaded + holders, no rmmod).
phase_b_use_ioctl() {
  vtl_loaded || return 1
  [ -c /dev/vtl ] || return 1
  scsi_tape_holders_present
}

apply_set_instances_ioctl() {
  [ -c /dev/vtl ] || die "/dev/vtl missing (rebuild vtl.ko with vtl_misc)"
  command -v python3 >/dev/null 2>&1 || die "python3 required for SET_INSTANCES ioctl"
  log "SET_INSTANCES ioctl spec=${VTL_INST_SPEC}"
  if ! python3 - "$VTL_INST_SPEC" <<'PY' >>"$LOG/set-instances-ioctl.txt" 2>&1
import sys, fcntl
spec = sys.argv[1]
if len(spec) >= 384:
    sys.exit("spec too long")
buf = bytearray(384)
buf[: len(spec)] = spec.encode("ascii")
ioctl_cmd = 0x41805605
fd = open("/dev/vtl", "rb+", buffering=0)
try:
    fcntl.ioctl(fd, ioctl_cmd, buf)
    print("ioctl SET_INSTANCES ok:", spec)
except OSError as e:
    print("ioctl failed:", e, file=sys.stderr)
    sys.exit(1)
finally:
    fd.close()
PY
  then
    cat "$LOG/set-instances-ioctl.txt" | tee -a "$LOG/run.log" >&2
    die "SET_INSTANCES ioctl failed (see $LOG/set-instances-ioctl.txt)"
  fi
  cat "$LOG/set-instances-ioctl.txt" | tee -a "$LOG/run.log"
}

wait_after_set_instances() {
  log "waiting 120s after ioctl for deferred scan work"
  sleep 120
}

cmd_phase_b() {
  need_root
  post_reboot_capture_hint
  log "=== phase B: vtl_instances=${VTL_INST_SPEC}, no user I/O, soak ${VTL_SOAK_SEC}s (no rmmod when skip-rmmod or ioctl path) ==="
  if ! vtl_loaded; then
    [ "$VTL_SKIP_RMMOD" = "1" ] || safe_rmmod
    log "insmod vtl_instances=${VTL_INST_SPEC}"
    load_vtl "vtl_instances=${VTL_INST_SPEC} $VTL_INSMOD_EXTRA"
    wait_after_insmod
  elif [ "$VTL_SKIP_RMMOD" = "1" ]; then
    log "vtl already loaded; VTL_SKIP_RMMOD=1 — soak only (geometry via phase-d or prior ioctl; no wait)"
  elif phase_b_use_ioctl; then
    log "holders on /dev/st*|sg*|sch*|ch* — SET_INSTANCES ioctl (no rmmod)"
    apply_set_instances_ioctl
    wait_after_set_instances
  else
    log "rmmod vtl (requires no open /dev/st*|sg*|sch*|ch*)"
    safe_rmmod
    load_vtl "vtl_instances=${VTL_INST_SPEC} $VTL_INSMOD_EXTRA"
    wait_after_insmod
  fi
  snapshot phase-b-start
  soak phase-b "$VTL_SOAK_SEC"
  snapshot phase-b-end
  log "phase B complete"
}

find_first_vtl_st() {
  if [ -f "$SCRIPT_DIR/vtl-scsi-holders.sh" ]; then
    # shellcheck source=/dev/null
    . "$SCRIPT_DIR/vtl-scsi-holders.sh"
    for _d in $(vtl_scsi_dev_paths); do
      case "$_d" in
        /dev/st*|/dev/nst*) echo "$_d"; return 0 ;;
      esac
    done
    return 1
  fi
  lsscsi -g 2>/dev/null | awk '/\/dev\/st/ {print $NF; exit}'
}

cmd_phase_c() {
  need_root
  vtl_loaded || die "load vtl first (phase-a or phase-b)"
  _st="$(find_first_vtl_st)"
  [ -n "$_st" ] || die "no /dev/st* for VTL in lsscsi -g"
  log "=== phase C: light I/O on $_st (mt status every 5 min, ${VTL_SOAK_SEC}s) ==="
  snapshot phase-c-start
  _start="$(date +%s)"
  _end=$((_start + VTL_SOAK_SEC))
  _n=0
  while [ "$(date +%s)" -lt "$_end" ]; do
    _n=$((_n + 1))
    if command -v mt >/dev/null 2>&1; then
      log "mt -f $_st status"
      mt -f "$_st" status 2>&1 | tee -a "$LOG/phase-c-mt.txt" || true
    else
      log "WARN: mt not installed; skip tape ioctl"
    fi
    snapshot "phase-c-${_n}"
    sleep 300
  done
  snapshot phase-c-end
  log "phase C complete"
}

# SET_INSTANCES via python3 (same ioctl encoding as userspace kernel_geom_ioctl.rs)
cmd_phase_d() {
  need_root
  vtl_loaded || die "load vtl first"
  log "=== phase D: VTL_IOCTL_SET_INSTANCES spec=${VTL_INST_SPEC} (same spec rebuild) ==="
  snapshot phase-d-pre
  apply_set_instances_ioctl
  snapshot phase-d-post
  wait_after_set_instances
  snapshot phase-d-post-wait
  log "phase D complete"
}

cmd_phase_e() {
  need_root
  vtl_loaded || die "load vtl first"
  _rescan="$SCRIPT_DIR/vtl-scsi-rescan.sh"
  [ -x "$_rescan" ] || _rescan="sh $_rescan"
  log "=== phase E: mhVTL-style SCSI rescan (${VTL_SOAK_SEC}s soak with rescan every 10 min) ==="
  _start="$(date +%s)"
  _end=$((_start + VTL_SOAK_SEC))
  _n=0
  while [ "$(date +%s)" -lt "$_end" ]; do
    _n=$((_n + 1))
    log "rescan round $_n"
    $_rescan 2>&1 | tee -a "$LOG/phase-e-rescan.txt" || true
    snapshot "phase-e-${_n}"
    sleep 600
  done
  log "phase E complete"
}

cmd_snapshot() {
  need_root
  snapshot manual
  log "snapshot written under $LOG"
}

cmd_help() {
  cat <<'EOF'
vtl-kernel-stability.sh — staged vtl.ko stability tests

Phases (run one at a time; compare reboot / dmesg after each):

  preflight   Check ko path, vermagic, journal/kdump hints
  status      Module + lsscsi snapshot
  check-holders  Fail if fuser shows open /dev/st*|sg*|sch*|ch* (before rmmod)
  phase-a     Default insmod + wait + periodic lsscsi (refuses rmmod if tape/sg busy)
  phase-a-idle  insmod + wait + soak without periodic lsscsi (baseline on Kylin)
  phase-a-probe Same as phase-a with VTL_SKIP_RMMOD=1 (no rmmod; periodic lsscsi OK)
  phase-reload  safe rmmod + insmod + wait (only when check-holders passes)
  integration-smoke  preflight + A-idle + A-probe (VTL_SKIP_RMMOD=1 both legs; VTL_INTEGRATION_SOAK_SEC each)
  phase-b     vtl_instances=VTL_INST_SPEC + soak (ioctl if holders; skip-rmmod+loaded = soak only)
  phase-c     Light mt status on first VTL st*, 5 min interval
  phase-d     SET_INSTANCES ioctl with VTL_INST_SPEC (python3, /dev/vtl)
  phase-e     vtl-scsi-rescan.sh every 10 min during soak
  snapshot    Diagnostics bundle only

Examples:

  # Kylin integration (10 min per phase; no rmmod on probe leg)
  sudo sh vtl-kernel-stability.sh integration-smoke
  sudo VTL_INTEGRATION_SOAK_SEC=3600 sh vtl-kernel-stability.sh integration-smoke

  # 1h: idle baseline then probe without rmmod
  sudo VTL_SOAK_SEC=3600 sh vtl-kernel-stability.sh phase-a-idle
  sudo VTL_SKIP_RMMOD=1 VTL_SOAK_SEC=3600 sh vtl-kernel-stability.sh phase-a-probe

  # Full rmmod cycle only when nothing holds tape/sg:
  sudo sh vtl-kernel-stability.sh check-holders
  sudo sh vtl-kernel-stability.sh phase-reload

  # Multi-library soak: ioctl when tape/sg busy; cold rmmod+insmod only if check-holders passes
  sudo sh vtl-kernel-stability.sh check-holders || true
  sudo VTL_SOAK_SEC=28800 VTL_INST_SPEC='1x4,2x8' sh vtl-kernel-stability.sh phase-b

  # After stable phase-b, light tape status polling
  sudo VTL_SOAK_SEC=7200 sh vtl-kernel-stability.sh phase-c

Logs: VTL_LOG_DIR (default /var/log/vtl-stability)

If the host reboots: journalctl -b -1 -k  OR  crash/vmcore  OR  run:
  sudo sh vtladm-collect-diagnostics.sh /tmp/vtl-diag.tar.gz
EOF
}

main() {
  log_env_once
  case "${PHASE:-}" in
    preflight) cmd_preflight ;;
    status) cmd_status ;;
    check-holders|holders) cmd_check_holders ;;
    phase-a|a) cmd_phase_a ;;
    phase-a-idle|a-idle) cmd_phase_a_idle ;;
    phase-a-probe|a-probe) cmd_phase_a_probe ;;
    phase-reload|reload) cmd_phase_reload ;;
    integration-smoke|integration) cmd_integration_smoke ;;
    phase-b|b) cmd_phase_b ;;
    phase-c|c) cmd_phase_c ;;
    phase-d|d) cmd_phase_d ;;
    phase-e|e) cmd_phase_e ;;
    snapshot) cmd_snapshot ;;
    help|-h|--help|"") cmd_help ;;
    *)
      die "unknown phase '$PHASE'; use: help | preflight | check-holders | integration-smoke | phase-a | phase-a-idle | phase-a-probe | phase-reload | phase-b..e | status | snapshot"
      ;;
  esac
}

main
