#!/bin/sh
# Build and install vtl.ko + vtladm to /opt/vtladm (create dirs if missing).
# Usage:
#   sudo sh install.sh              # build + install + /etc/default/vtladm
#   sudo sh install.sh --enable     # also systemctl enable --now vtl-kernel, vtladm-web, vtl-patrol.timer, vtl-robot-sync.timer
#   sudo sh install.sh --no-reload  # userspace/systemd only; skip vtl.ko unload/reload (safe on live backup hosts)
#   sudo sh uninstall.sh            # remove install; keep /opt/vtladm/var data (see uninstall.sh --purge)
#   PREFIX=/opt/vtladm sh install.sh --no-build   # install only (artifacts must exist)
#
# Kernel maintenance (default): safe rmmod → reboot → insmod on next boot via vtl-kernel.service (--enable).
# Accepts full system reboot on Kylin/openEuler (avoids post-rmmod GPF). Use --no-reboot only in a narrow window.
# Reload when: vtl.ko or vtl-kernelctl digest changed, this run built kernel (DO_BUILD=1), or VTL_FORCE_RELOAD=1.
# Env: VTL_SKIP_KERNEL_RELOAD=1 | VTL_USERSPACE_ONLY=1  same as --no-reload
#      VTL_FORCE_RELOAD=1  reload even if ko unchanged
#      VTL_NO_REBOOT=1 | --no-reboot  in-process vtl_safe_reload (no reboot)
set -eu

ROOT="$(cd "$(dirname "$0")" && pwd)"
PREFIX="${PREFIX:-/opt/vtladm}"
ENABLE_SYSTEMD=0
DO_BUILD=1
SKIP_KERNEL_RELOAD=0
REBOOT_AFTER_KERNEL=1
_KERNEL_BUILT=0
_VTL_TMP_FILES=""

_vtl_mktemp() {
  _tmp=$(mktemp "${TMPDIR:-/tmp}/$1.XXXXXX") || exit 1
  _VTL_TMP_FILES="${_VTL_TMP_FILES} ${_tmp}"
  printf '%s\n' "$_tmp"
}

_vtl_cleanup_tmp() {
  [ -z "${_VTL_TMP_FILES:-}" ] || rm -f $_VTL_TMP_FILES
}
trap _vtl_cleanup_tmp EXIT HUP INT TERM

for _arg in "$@"; do
  case "$_arg" in
    --enable) ENABLE_SYSTEMD=1 ;;
    --no-build) DO_BUILD=0 ;;
    --no-reload) SKIP_KERNEL_RELOAD=1 ;;
    --no-reboot) REBOOT_AFTER_KERNEL=0 ;;
    --reboot) REBOOT_AFTER_KERNEL=1 ;;
    -h|--help)
      echo "usage: $0 [--enable] [--no-build] [--no-reload] [--no-reboot]"
      echo "  PREFIX=$PREFIX (default /opt/vtladm)"
      echo "  --no-reload   install binaries/units only; do not rmmod vtl.ko"
      echo "  --no-reboot   after rmmod, reload vtl.ko in this script (no system reboot)"
      echo "  Default (kernel): rmmod vtl.ko + system reboot; use --enable for vtl-kernel.service on boot"
      echo "  Env: VTL_SKIP_KERNEL_RELOAD=1  VTL_FORCE_RELOAD=1  VTL_NO_REBOOT=1  VTL_REBOOT_DELAY_SEC=10"
      exit 0
      ;;
  esac
done

if [ "${VTL_SKIP_KERNEL_RELOAD:-}" = "1" ] || [ "${VTL_USERSPACE_ONLY:-}" = "1" ]; then
  SKIP_KERNEL_RELOAD=1
fi
if [ "${VTL_NO_REBOOT:-}" = "1" ]; then
  REBOOT_AFTER_KERNEL=0
fi

if [ "$(id -u)" -ne 0 ] 2>/dev/null; then
  echo "install.sh: run as root (sudo)" >&2
  exit 1
fi

# Reject empty/non-ELF artifacts (common when target/ was copied from Windows or interrupted).
_vtl_verify_elf_bin() {
  _path="$1"
  if [ ! -f "$_path" ]; then
    echo "missing $_path" >&2
    return 1
  fi
  _sz=$(wc -c <"$_path" 2>/dev/null | tr -d ' ')
  if [ -z "$_sz" ] || [ "$_sz" -lt 4096 ] 2>/dev/null; then
    echo "invalid $_path (${_sz:-0} bytes — expected Linux ELF release binary)" >&2
    return 1
  fi
  if command -v file >/dev/null 2>&1; then
    _ft=$(file -b "$_path" 2>/dev/null) || _ft=""
    case "$_ft" in
      ELF\ *) return 0 ;;
      *)
        echo "invalid $_path (file: ${_ft:-unknown})" >&2
        return 1
        ;;
    esac
  fi
  return 0
}

_vtl_scrub_stale_cargo_bins() {
  _need_clean=0
  for _b in target/release/vtladm target/release/vtladm-iscsi; do
    if [ -e "$_b" ] && ! _vtl_verify_elf_bin "$_b"; then
      echo "WARN: removing stale cargo output $_b" >&2
      _need_clean=1
    fi
  done
  if [ "$_need_clean" -eq 1 ]; then
    rm -f target/release/vtladm target/release/vtladm-iscsi
    rm -f target/release/deps/vtladm target/release/deps/vtladm-*
    rm -f target/release/deps/vtladm-iscsi target/release/deps/vtladm-iscsi-*
    rm -rf target/release/.fingerprint/vtladm-*
  fi
}

# Resolve linked ELF (cargo may leave a stale 0-byte target/release/vtladm).
_vtl_find_release_vtladm() {
  if _vtl_verify_elf_bin target/release/vtladm 2>/dev/null; then
    echo "target/release/vtladm"
    return 0
  fi
  _best=""
  for _p in target/release/deps/vtladm-[0-9a-f]* target/release/deps/vtladm; do
    [ -f "$_p" ] || continue
    _vtl_verify_elf_bin "$_p" 2>/dev/null || continue
    _best="$_p"
    break
  done
  if [ -n "$_best" ]; then
    echo "$_best"
    return 0
  fi
  return 1
}

_vtl_finalize_cargo_bins() {
  _artifact=$(_vtl_find_release_vtladm) || {
    echo "WARN: no ELF vtladm under target/release yet" >&2
    ls -la target/release/vtladm target/release/deps/vtladm* 2>/dev/null >&2 || true
    return 1
  }
  if [ "$_artifact" != "target/release/vtladm" ]; then
    echo ">> installing linked binary from $_artifact"
    cp -f "$_artifact" target/release/vtladm
    chmod 755 target/release/vtladm
  fi
  _vtl_verify_elf_bin target/release/vtladm || return 1
  if [ -f target/release/deps/vtladm-iscsi ] && _vtl_verify_elf_bin target/release/deps/vtladm-iscsi 2>/dev/null; then
    cp -f target/release/deps/vtladm-iscsi target/release/vtladm-iscsi 2>/dev/null || true
    chmod 755 target/release/vtladm-iscsi 2>/dev/null || true
  fi
  _vtl_verify_elf_bin target/release/vtladm-iscsi 2>/dev/null || true
  return 0
}

# Cargo.lock v4 needs cargo >= 1.83; Kylin/openEuler often ship 1.75–1.82.
_vtl_cargo_build_release() {
  _vtl_scrub_stale_cargo_bins
  if [ -f Cargo.lock ] && grep -q '^version = 4$' Cargo.lock 2>/dev/null; then
    _cv=$(cargo --version 2>/dev/null | awk '{print $2}')
    _maj=$(echo "$_cv" | cut -d. -f1)
    _min=$(echo "$_cv" | cut -d. -f2)
    _lock4_ok=0
    if [ -n "$_maj" ] && [ -n "$_min" ]; then
      if [ "$_maj" -gt 1 ] 2>/dev/null; then
        _lock4_ok=1
      elif [ "$_maj" -eq 1 ] 2>/dev/null && [ "$_min" -ge 83 ] 2>/dev/null; then
        _lock4_ok=1
      fi
    fi
    if [ "$_lock4_ok" -eq 0 ]; then
      echo "WARN: Cargo.lock v4 requires cargo >= 1.83 (found cargo ${_cv:-unknown})" >&2
      echo "      Removing Cargo.lock; build uses Cargo.toml version pins" >&2
      rm -f Cargo.lock
    fi
  fi
  _log=$(_vtl_mktemp "vtladm-cargo-build.log")
  _cargo_bins="--bin vtladm --bin vtladm-iscsi"
  if [ -f Cargo.lock ]; then
    if cargo build --release --locked $_cargo_bins >"$_log" 2>&1; then
      cat "$_log"
      rm -f "$_log"
      if _vtl_finalize_cargo_bins; then
        return 0
      fi
      echo "WARN: cargo --locked finished but vtladm binary invalid — retrying" >&2
      _vtl_scrub_stale_cargo_bins
    else
      if grep -qE 'lock file version|does not understand this lock file' "$_log" 2>/dev/null; then
        echo "WARN: this cargo cannot read Cargo.lock — removing and retrying" >&2
        rm -f Cargo.lock
      else
        cat "$_log" >&2
        rm -f "$_log"
        return 1
      fi
      rm -f "$_log"
    fi
  fi
  # shellcheck disable=SC2086
  cargo build --release $_cargo_bins
  if ! _vtl_find_release_vtladm >/dev/null 2>&1; then
    echo ">> cargo produced no valid vtladm — cargo clean + rebuild" >&2
    _vtl_scrub_stale_cargo_bins
    cargo clean -p vtladm 2>/dev/null || cargo clean 2>/dev/null || true
    _log=$(_vtl_mktemp "vtladm-cargo-build.log")
    if ! cargo build --release $_cargo_bins -vv >"$_log" 2>&1; then
      tail -40 "$_log" >&2
      rm -f "$_log"
      return 1
    fi
    tail -15 "$_log" 2>/dev/null || true
    rm -f "$_log"
  fi
  _vtl_finalize_cargo_bins
}

echo "=== vtladm install PREFIX=$PREFIX ==="
_krel="$(uname -r)"
echo ">> supported: Linux 4.18–6.10, arch x86_64/aarch64 (building for $(uname -m) $_krel)"

if [ "$DO_BUILD" -eq 1 ]; then
  echo ">> kernel module (uname -r=$_krel)"
  # Clock skew makes make skip rebuild; normalize mtimes before build.
  if find "$ROOT/kernel" -newer /proc/uptime 2>/dev/null | grep -q .; then
    echo "WARN: kernel sources newer than system clock — fixing mtimes (sync NTP if this persists)" >&2
    find "$ROOT/kernel" -exec touch -c {} + 2>/dev/null || find "$ROOT/kernel" -exec touch {} + 2>/dev/null || true
  fi
  if grep -q 'vtl_personality\.o' "$ROOT/kernel/Kbuild" 2>/dev/null \
     && [ ! -f "$ROOT/kernel/src/vtl_personality.c" ]; then
    echo "ERROR: Kbuild lists vtl_personality.o but kernel/src/vtl_personality.c is missing." >&2
    echo "  Copy kernel/include/vtl_personality.h, kernel/src/vtl_personality.c, and Kbuild from the repo." >&2
    exit 1
  fi
  if [ ! -f "$ROOT/kernel/vtl.ko" ] || grep -q 'vtl_personality\.o' "$ROOT/kernel/Kbuild" 2>/dev/null; then
    make -C "$ROOT/kernel" clean 2>/dev/null || true
  fi
  if make -C "$ROOT/kernel"; then
    _KERNEL_BUILT=1
  else
    exit 1
  fi
  if ! command -v cargo >/dev/null 2>&1; then
    echo "cargo not found; install Rust or use --no-build" >&2
    exit 1
  fi
  echo ">> userspace (cargo build --release)"
  (cd "$ROOT/userspace" && _vtl_cargo_build_release)
fi

_ko="$ROOT/kernel/vtl.ko"
_bin="$ROOT/userspace/target/release/vtladm"
_iscsi="$ROOT/userspace/target/release/vtladm-iscsi"
[ -f "$_ko" ] || { echo "missing $_ko — run build first" >&2; exit 1; }

# Detect whether installed ko / vtl-kernelctl differ (reload only when needed).
_vtl_file_digest() {
  _f="$1"
  [ -f "$_f" ] || return 1
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$_f" 2>/dev/null | awk '{print $1}'
  elif command -v md5sum >/dev/null 2>&1; then
    md5sum "$_f" 2>/dev/null | awk '{print $1}'
  else
    wc -c <"$_f" 2>/dev/null | tr -d ' '
  fi
}
_KO_OLD_DIGEST=$(_vtl_file_digest "$PREFIX/ko/vtl.ko" 2>/dev/null || true)
_KCTL_OLD_DIGEST=$(_vtl_file_digest "$PREFIX/sbin/vtl-kernelctl" 2>/dev/null || true)
_KO_NEW_DIGEST=$(_vtl_file_digest "$_ko")
_KCTL_NEW_DIGEST=$(_vtl_file_digest "$ROOT/packaging/sbin/vtl-kernelctl")
if ! (cd "$ROOT/userspace" && _vtl_verify_elf_bin "target/release/vtladm"); then
  echo "ERROR: userspace binary not built on this host." >&2
  echo "  cd $ROOT/userspace && rm -rf target/release/.fingerprint/vtladm-* target/release/vtladm*" >&2
  echo "  cargo clean -p vtladm && cargo build --release --bin vtladm -vv" >&2
  exit 1
fi

mkdir -p "$PREFIX"/{bin,ko,sbin,scripts,docs,lib/systemd/system}
mkdir -p "$PREFIX/var"/{tapes,log/vtl}

install -m 0644 "$_ko" "$PREFIX/ko/vtl.ko"

_krel="$(uname -r)"
_extradir="/lib/modules/${_krel}/extra"
if [ -d "/lib/modules/${_krel}" ]; then
  mkdir -p "$_extradir"
  install -m 0644 "$PREFIX/ko/vtl.ko" "$_extradir/vtl.ko"
  if command -v depmod >/dev/null 2>&1; then
    depmod -a "$_krel" 2>/dev/null || depmod -a
    echo "installed $_extradir/vtl.ko (modprobe vtl)"
  fi
fi
if command -v modinfo >/dev/null 2>&1; then
  _vm=$(modinfo -F vermagic "$PREFIX/ko/vtl.ko" 2>/dev/null | awk '{print $1}' || true)
  echo "vtl.ko vermagic: ${_vm:-unknown} (running $_krel)"
  case "${_vm:-}" in
    "$_krel"*) ;;
    *)
      if [ -n "${_vm:-}" ]; then
        echo "WARN: vermagic may not match running kernel — insmod may fail" >&2
      fi
      ;;
  esac
fi
install -m 0755 "$_bin" "$PREFIX/bin/vtladm"
if ! _vtl_verify_elf_bin "$PREFIX/bin/vtladm"; then
  echo "ERROR: $PREFIX/bin/vtladm install failed ELF check" >&2
  echo "  Do not rsync userspace/target/ from Windows. Re-run: sudo sh install.sh" >&2
  exit 1
fi
if [ -f "$_iscsi" ]; then
  install -m 0755 "$_iscsi" "$PREFIX/bin/vtladm-iscsi"
fi

# packaging helpers
for _f in vtl-kernelctl vtladm-web-serve vtl-link-kernel-tapes; do
  install -m 0755 "$ROOT/packaging/sbin/$_f" "$PREFIX/sbin/$_f"
done
if [ -f "$ROOT/uninstall.sh" ]; then
  install -m 0755 "$ROOT/uninstall.sh" "$PREFIX/sbin/vtl-uninstall"
fi

# userspace scripts
install -m 0755 "$ROOT/packaging/scripts/vtl-patrol.sh" "$PREFIX/scripts/vtl-patrol.sh"
install -m 0755 "$ROOT/packaging/scripts/vtl-robot-sync.sh" "$PREFIX/scripts/vtl-robot-sync.sh"
install -m 0755 "$ROOT/packaging/scripts/vtl-kernel-safe.sh" "$PREFIX/scripts/vtl-kernel-safe.sh"
install -m 0644 "$ROOT/packaging/scripts/vtl-source-defaults.sh" "$PREFIX/scripts/vtl-source-defaults.sh"

for _s in \
  vtl-scsi-holders.sh \
  vtl-scsi-rescan.sh \
  vtl-scsi-scan-all-hosts.sh \
  vtl-changer-inventory-probe.sh \
  vtl-kernel-reload.sh \
  vtl-kernel-stability.sh \
  vtladm-collect-diagnostics.sh
do
  if [ -f "$ROOT/userspace/scripts/$_s" ]; then
    install -m 0755 "$ROOT/userspace/scripts/$_s" "$PREFIX/scripts/$_s"
  fi
done
# docs
install -m 0644 "$ROOT/packaging/docs/STACK.md" "$PREFIX/docs/STACK.md"
[ -f "$ROOT/packaging/docs/INTEGRATION-TEST.md" ] && \
  install -m 0644 "$ROOT/packaging/docs/INTEGRATION-TEST.md" "$PREFIX/docs/INTEGRATION-TEST.md"
[ -f "$ROOT/packaging/docs/KERNEL-COMPAT.md" ] && \
  install -m 0644 "$ROOT/packaging/docs/KERNEL-COMPAT.md" "$PREFIX/docs/KERNEL-COMPAT.md"
[ -f "$ROOT/userspace/docs/ROBOT-SYNC.md" ] && \
  install -m 0644 "$ROOT/userspace/docs/ROBOT-SYNC.md" "$PREFIX/docs/ROBOT-SYNC.md"
[ -f "$ROOT/userspace/docs/PATROL.md" ] && \
  install -m 0644 "$ROOT/userspace/docs/PATROL.md" "$PREFIX/docs/PATROL.md"
[ -f "$ROOT/userspace/docs/SCSI.md" ] && install -m 0644 "$ROOT/userspace/docs/SCSI.md" "$PREFIX/docs/SCSI.md"
[ -f "$ROOT/userspace/docs/TRANSPORT.md" ] && install -m 0644 "$ROOT/userspace/docs/TRANSPORT.md" "$PREFIX/docs/TRANSPORT.md"

# udev: skip scsi_id on VTL devices (insmod scan race on openEuler 6.6)
if [ -f "$ROOT/packaging/udev/59-vtl-scsi.rules" ]; then
  install -m 0644 "$ROOT/packaging/udev/59-vtl-scsi.rules" /etc/udev/rules.d/59-vtl-scsi.rules
  if command -v udevadm >/dev/null 2>&1; then
    udevadm control --reload-rules 2>/dev/null || true
    udevadm trigger --subsystem-match=scsi 2>/dev/null || true
  fi
  echo "installed /etc/udev/rules.d/59-vtl-scsi.rules (ID_SCSI=skip for VTL vendor)"
fi

# /etc/default
_vtl_defaults_tmp=$(_vtl_mktemp "vtladm-defaults.install")
sed 's/\r$//' "$ROOT/packaging/etc/default/vtladm" > "$_vtl_defaults_tmp"
if [ -f /etc/default/vtladm ]; then
  cp -a /etc/default/vtladm "/etc/default/vtladm.bak.$(date +%Y%m%d%H%M%S)" 2>/dev/null || true
fi
install -m 0644 "$_vtl_defaults_tmp" /etc/default/vtladm
echo "installed /etc/default/vtladm (LF, from packaging)"
rm -f "$_vtl_defaults_tmp"

# systemd
if command -v systemctl >/dev/null 2>&1; then
  for _u in vtl-kernel.service vtladm-web.service vtl-patrol.service vtl-patrol.timer vtl-robot-sync.service vtl-robot-sync.timer; do
    install -m 0644 "$ROOT/packaging/systemd/$_u" "$PREFIX/lib/systemd/system/$_u"
    ln -sf "$PREFIX/lib/systemd/system/$_u" "/etc/systemd/system/$_u"
  done
  systemctl daemon-reload
  echo "systemd units linked under /etc/systemd/system/"
fi

# PATH symlinks (optional convenience)
ln -sf "$PREFIX/bin/vtladm" /usr/local/bin/vtladm 2>/dev/null || true
ln -sf "$PREFIX/bin/vtladm-iscsi" /usr/local/bin/vtladm-iscsi 2>/dev/null || true
ln -sf "$PREFIX/sbin/vtl-kernelctl" /usr/local/sbin/vtl-kernelctl 2>/dev/null || true

# strip CRLF on scripts
for _f in "$PREFIX"/scripts/*.sh "$PREFIX"/scripts/vtl-source-defaults.sh "$PREFIX"/sbin/*; do
  [ -f "$_f" ] && sed -i 's/\r$//' "$_f" 2>/dev/null || sed -i '' 's/\r$//' "$_f" 2>/dev/null || true
done

_vtl_kernel_reload_wanted() {
  if [ "$SKIP_KERNEL_RELOAD" -eq 1 ]; then
    return 1
  fi
  if [ "${VTL_FORCE_RELOAD:-}" = "1" ]; then
    return 0
  fi
  if [ "$_KERNEL_BUILT" -eq 1 ]; then
    return 0
  fi
  if [ -z "${_KO_OLD_DIGEST:-}" ] && [ -n "${_KO_NEW_DIGEST:-}" ]; then
    return 0
  fi
  if [ -n "${_KO_OLD_DIGEST:-}" ] && [ "${_KO_OLD_DIGEST}" != "${_KO_NEW_DIGEST}" ]; then
    return 0
  fi
  if [ -n "${_KCTL_OLD_DIGEST:-}" ] && [ "${_KCTL_OLD_DIGEST}" != "${_KCTL_NEW_DIGEST}" ]; then
    return 0
  fi
  return 1
}

_VTL_RELOADED=0
_VTL_REBOOT_PENDING=0
_VTL_DID_RMMOD=0
if _vtl_kernel_reload_wanted; then
  echo ">> kernel maintenance: unload vtl.ko$([ "$REBOOT_AFTER_KERNEL" -eq 1 ] && echo ', then reboot' || echo ', then reload in-process')"
  if [ -f "$PREFIX/scripts/vtl-kernel-safe.sh" ]; then
    # shellcheck source=/dev/null
    . "$PREFIX/scripts/vtl-kernel-safe.sh"
    vtl_vtladm_timers_stop
    if command -v systemctl >/dev/null 2>&1; then
      systemctl stop vtladm-web.service vtl-kernel.service 2>/dev/null || true
    fi
    if lsmod 2>/dev/null | awk '{print $1}' | grep -qx vtl; then
      if [ "$REBOOT_AFTER_KERNEL" -eq 1 ]; then
        export VTL_SKIP_POST_RMMOD_WAIT=1
        echo ">> safe vtl.ko unload; host will reboot (insmod on next boot via vtl-kernel.service)"
        if vtl_safe_rmmod "$PREFIX/sbin/vtl-kernelctl"; then
          _VTL_DID_RMMOD=1
          _VTL_REBOOT_PENDING=1
        else
          echo "ERROR: vtl safe rmmod failed — stop backup, targetcli clearconfig, then retry" >&2
          exit 1
        fi
      else
        echo ">> safe vtl.ko reload (no reboot; extended post-rmmod wait)"
        if vtl_safe_reload "$PREFIX/sbin/vtl-kernelctl"; then
          _VTL_RELOADED=1
        else
          echo "ERROR: vtl safe reload failed — stop backup, targetcli clearconfig, then:" >&2
          echo "  $PREFIX/sbin/vtl-kernelctl reload   or   VTL_FORCE_RELOAD=1 $0 --no-reboot" >&2
          exit 1
        fi
      fi
    else
      echo ">> vtl.ko not loaded; skip rmmod (start after install via --enable or vtl-kernelctl start)"
    fi
  elif [ -x "$PREFIX/sbin/vtl-kernelctl" ] && lsmod 2>/dev/null | awk '{print $1}' | grep -qx vtl; then
    echo "WARN: vtl-kernel-safe.sh missing — using vtl-kernelctl reload (less safe)" >&2
    systemctl stop vtladm-web.service vtl-kernel.service 2>/dev/null || true
    sleep 5
    if [ "$REBOOT_AFTER_KERNEL" -eq 1 ]; then
      if "$PREFIX/sbin/vtl-kernelctl" stop; then
        _VTL_DID_RMMOD=1
        _VTL_REBOOT_PENDING=1
      fi
    elif "$PREFIX/sbin/vtl-kernelctl" reload; then
      _VTL_RELOADED=1
    else
      echo "WARN: vtl-kernelctl reload failed" >&2
    fi
  fi
elif lsmod 2>/dev/null | awk '{print $1}' | grep -qx vtl; then
  echo ">> skipping vtl.ko reload (unchanged ko; use VTL_FORCE_RELOAD=1 or --no-build without --no-reload after kernel rebuild)"
  if command -v systemctl >/dev/null 2>&1; then
    systemctl restart vtladm-web.service 2>/dev/null || true
  fi
else
  echo ">> vtl.ko not loaded; enable/start via --enable or: $PREFIX/sbin/vtl-kernelctl start"
fi

if [ ! -f "$PREFIX/var/vtl.conf" ]; then
  echo ">> first-time: vtladm init-config (creates $PREFIX/var/vtl.conf)"
  VTL_CONF_PATH="$PREFIX/var/vtl.conf" "$PREFIX/bin/vtladm" init-config || true
elif ! grep -qE '^[[:space:]]*auto_sync_db_from_kernel[[:space:]]*=' "$PREFIX/var/vtl.conf" 2>/dev/null \
  && ! grep -qE '^[[:space:]]*robot_sync[[:space:]]*=[[:space:]]*false' "$PREFIX/var/vtl.conf" 2>/dev/null; then
  echo ">> vtl.conf: append robot sync defaults (kernel runtime + catalog hints)"
  cat >>"$PREFIX/var/vtl.conf" <<'EOF'

# Runtime robot in vtl.ko; periodic kernel→DB catalog hints
robot_sync=true
auto_reconcile_pull=true
auto_sync_db_from_kernel=true
EOF
fi
echo "=== installed to $PREFIX ==="
echo "  modinfo $PREFIX/ko/vtl.ko"
echo "  $PREFIX/sbin/vtl-kernelctl start"
echo "  $PREFIX/scripts/vtl-patrol.sh"
echo "  $PREFIX/scripts/vtl-robot-sync.sh  (kernel→DB catalog hints every 5min when robot_sync=true)"
echo "  $PREFIX/bin/vtladm serve --host 0.0.0.0 --port 8765"
echo "  data: $PREFIX/var/  (vtl.conf, vtl.db, tapes/, log/)"
echo "  uninstall: $PREFIX/sbin/vtl-uninstall  (or: sudo sh uninstall.sh from source tree)"
echo "  userspace-only update: $0 --no-reload   (or VTL_SKIP_KERNEL_RELOAD=1)"
echo "  kernel update (default): rmmod + reboot; use --enable so vtl-kernel starts on boot"
echo "  kernel update (no reboot): VTL_FORCE_RELOAD=1 $0 --no-reboot  (maintenance window)"
echo "  integration test: $PREFIX/docs/INTEGRATION-TEST.md"
echo "  Plan B (8 lib × 8 drv × 256 slots): docs/SCSI.md §1g — uncomment kernel_geometry_mode=fixed in vtl.conf"

_vtl_plan_b_mode() {
  _c="${1:-$PREFIX/var/vtl.conf}"
  [ -f "$_c" ] || return 1
  grep -qE '^[[:space:]]*kernel_geometry_mode[[:space:]]*=[[:space:]]*(fixed|plan_b|semi_thin)' "$_c" 2>/dev/null
}

_resolve_kernel_spec_for_defaults() {
  if _vtl_plan_b_mode "$PREFIX/var/vtl.conf"; then
    "$PREFIX/bin/vtladm" kernel-spec --insmod-max 2>/dev/null || true
  else
    "$PREFIX/bin/vtladm" kernel-spec 2>/dev/null || true
  fi
}

if [ "$ENABLE_SYSTEMD" -eq 1 ]; then
  _vtl_conf="$PREFIX/var/vtl.conf"
  _enable_robot_sync_timer=0
  if [ -f "$_vtl_conf" ] \
    && grep -qE '^[[:space:]]*robot_sync[[:space:]]*=[[:space:]]*true' "$_vtl_conf" 2>/dev/null \
    && ! grep -qE '^[[:space:]]*auto_sync_db_from_kernel[[:space:]]*=[[:space:]]*false' "$_vtl_conf" 2>/dev/null; then
    _enable_robot_sync_timer=1
  fi
  systemctl enable vtl-kernel.service
  systemctl enable vtladm-web.service
  if [ "${_VTL_REBOOT_PENDING:-0}" -eq 1 ]; then
    systemctl enable vtl-patrol.timer 2>/dev/null || true
    if [ "$_enable_robot_sync_timer" -eq 1 ]; then
      systemctl enable vtl-robot-sync.timer 2>/dev/null || true
    else
      systemctl disable vtl-robot-sync.timer 2>/dev/null || true
    fi
    _kspec=$(_resolve_kernel_spec_for_defaults) || _kspec=""
    if [ -n "$_kspec" ] && [ -f /etc/default/vtladm ]; then
      if grep -qE '^[[:space:]]*VTL_INSTANCES[[:space:]]*=[[:space:]]*$' /etc/default/vtladm 2>/dev/null; then
        _ed=$(_vtl_mktemp "vtladm-defaults.install2")
        sed "s/\r$//" /etc/default/vtladm | sed "s/^VTL_INSTANCES=.*/VTL_INSTANCES=${_kspec}/" >"$_ed" \
          && install -m 0644 "$_ed" /etc/default/vtladm && rm -f "$_ed"
        echo "  synced VTL_INSTANCES=${_kspec} into /etc/default/vtladm (before reboot)"
      fi
    fi
    echo "enabled for boot: vtl-kernel.service, vtladm-web.service, vtl-patrol.timer"
    if [ -f "$PREFIX/scripts/vtl-kernel-safe.sh" ]; then
      # shellcheck source=/dev/null
      . "$PREFIX/scripts/vtl-kernel-safe.sh"
      vtl_schedule_system_reboot "vtladm install: vtl.ko updated"
    else
      echo "ERROR: reboot requested but $PREFIX/scripts/vtl-kernel-safe.sh missing" >&2
      exit 1
    fi
  fi
  systemctl enable --now vtl-patrol.timer
  _web_enabled=1
  if [ "$_enable_robot_sync_timer" -eq 1 ]; then
    systemctl enable --now vtl-robot-sync.timer
  else
    systemctl disable vtl-robot-sync.timer 2>/dev/null || true
    echo "note: vtl-robot-sync.timer not enabled (need robot_sync=true and auto_sync_db_from_kernel!=false)"
  fi
  _kernel_ok=0
  if [ "${_VTL_RELOADED:-0}" -eq 1 ] && lsmod 2>/dev/null | grep -q '^vtl '; then
    echo "vtl.ko already loaded from install reload (skip vtl-kernel.service start)"
    _kernel_ok=1
  elif systemctl is-active --quiet vtl-kernel.service 2>/dev/null; then
    echo "vtl-kernel.service already active (skip second start)"
    _kernel_ok=1
  elif systemctl start vtl-kernel.service; then
    _kernel_ok=1
  else
    echo "ERROR: vtl-kernel.service failed to start" >&2
    echo "  systemctl status vtl-kernel.service -l" >&2
    echo "  journalctl -xeu vtl-kernel.service" >&2
    echo "  $PREFIX/sbin/vtl-kernelctl start" >&2
    echo "Timers vtl-patrol / vtl-robot-sync are enabled; sync-db runs after vtl.ko loads." >&2
    exit 1
  fi
  if [ "$_kernel_ok" -eq 1 ]; then
    _kspec=$(_resolve_kernel_spec_for_defaults) || _kspec=""
    if [ -n "$_kspec" ] && [ -f /etc/default/vtladm ]; then
      if grep -qE '^[[:space:]]*VTL_INSTANCES[[:space:]]*=[[:space:]]*$' /etc/default/vtladm 2>/dev/null; then
        _ed=$(_vtl_mktemp "vtladm-defaults.install2")
        sed "s/\r$//" /etc/default/vtladm | sed "s/^VTL_INSTANCES=.*/VTL_INSTANCES=${_kspec}/" >"$_ed" \
          && install -m 0644 "$_ed" /etc/default/vtladm && rm -f "$_ed"
        echo "  synced VTL_INSTANCES=${_kspec} into /etc/default/vtladm"
      fi
    fi
    if _vtl_plan_b_mode "$_vtl_conf" && [ -x "$PREFIX/scripts/vtl-scsi-scan-all-hosts.sh" ]; then
      echo ">> Plan B: staged SCSI scan after vtl-kernel start"
      "$PREFIX/scripts/vtl-scsi-scan-all-hosts.sh" 5 2>/dev/null || \
        echo "WARN: vtl-scsi-scan-all-hosts.sh failed — run manually after libraries exist" >&2
      if [ -f "$PREFIX/var/vtl.db" ] && [ -x "$PREFIX/bin/vtladm" ]; then
        "$PREFIX/bin/vtladm" kernel-align 2>/dev/null || true
      fi
    fi
    if ! systemctl restart vtladm-web.service 2>/dev/null; then
      echo "ERROR: vtladm-web.service failed to start" >&2
      echo "  systemctl status vtladm-web.service -l" >&2
      echo "  journalctl -u vtladm-web.service -n 40 --no-pager" >&2
      echo "  manual: $PREFIX/sbin/vtladm-web-serve" >&2
      _web_enabled=0
    elif ! systemctl is-active --quiet vtladm-web.service 2>/dev/null; then
      echo "ERROR: vtladm-web.service not active after start" >&2
      journalctl -u vtladm-web.service -n 20 --no-pager 2>/dev/null || true
      _web_enabled=0
    else
      _wp=8765
      if [ -f /etc/default/vtladm ]; then
        _wp=$(grep -E '^[[:space:]]*VTLADM_WEB_PORT[[:space:]]*=' /etc/default/vtladm 2>/dev/null | tail -1 \
          | sed 's/^[^=]*=//;s/^[[:space:]]*//;s/[[:space:]]*$//;s/^"//;s/"$//' | tr -d '\r') || _wp=8765
        [ -z "$_wp" ] && _wp=8765
      fi
      if command -v ss >/dev/null 2>&1; then
        ss -ltn 2>/dev/null | grep -q ":${_wp} " && \
          echo "  web UI listening on port ${_wp} (ss -ltn)"
      elif command -v netstat >/dev/null 2>&1; then
        netstat -ltn 2>/dev/null | grep -q ":${_wp} " && \
          echo "  web UI listening on port ${_wp} (netstat -ltn)"
      fi
      if [ "$_web_enabled" -eq 1 ] && command -v ss >/dev/null 2>&1 && \
        ! ss -ltn 2>/dev/null | grep -q ":${_wp} "; then
        echo "WARN: vtladm-web is active but port ${_wp} not in ss -ltn — check bind host/firewall" >&2
        journalctl -u vtladm-web.service -n 15 --no-pager 2>/dev/null || true
      fi
    fi
    echo "enabled (--now): vtl-kernel, vtl-patrol.timer$([ "$_web_enabled" -eq 1 ] && echo ', vtladm-web' || echo ' (vtladm-web FAILED — see above)')"
    echo "  robot defaults in vtl.conf: robot_sync=true, auto_sync_db_from_kernel=true"
  fi
fi

if [ "${_VTL_REBOOT_PENDING:-0}" -eq 1 ] && [ "$ENABLE_SYSTEMD" -eq 0 ]; then
  echo ">> vtl.ko unloaded; reboot recommended: sudo systemctl reboot" >&2
  echo "   then: $PREFIX/sbin/vtl-kernelctl start   or   $0 --enable" >&2
  if [ -f "$PREFIX/scripts/vtl-kernel-safe.sh" ]; then
    # shellcheck source=/dev/null
    . "$PREFIX/scripts/vtl-kernel-safe.sh"
    vtl_schedule_system_reboot "vtladm install: vtl.ko unloaded (no --enable)"
  fi
fi

echo "done."
