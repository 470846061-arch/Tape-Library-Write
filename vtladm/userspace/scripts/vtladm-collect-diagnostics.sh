#!/bin/sh
# 收集 VTL / LIO / SCSI / 内核排障所需日志，打成单个 .tar.gz，便于上传或发邮件。
# 用法:
#   sudo sh vtladm-collect-diagnostics.sh [输出路径.tar.gz]
# 未指定输出路径时，在当前目录生成: vtl-diagnostics-<主机名>-<时间>.tar.gz
#
# 收集内容（尽量不全盘复制大文件）:
#   uname、/proc、内存、dmesg/journal 内核段（含上一启动 -b -1 最近 N 行及同窗口 -r 新到旧）、lsscsi、vtl 模块参数、modinfo、
#   configfs/target 是否存在、targetcli 树（超时）、vtl.conf（脱敏少量键）、
#   vtladm/vtladm-iscsi 版本、进程快照、/var/crash 列表等。
# 不含: 完整 vmcore、磁带镜像、数据库文件。
#
# 环境变量:
#   VTL_DIAG_JOURNAL_PREV_LINES      上一启动内核日志条数（journalctl -k -b -1 -n N），默认 8000。
#   VTL_DIAG_JOURNAL_PREV_REV_LINES  同上但 -r 新到旧，便于抓 boot 末段 panic/Oops，默认 800。
#
# CRLF 会破坏 sh: 若遇 /bin/sh^M，请 sed -i 's/\r$//' 本文件。

set -e
umask 077

OUT="${1:-}"
if [ -z "$OUT" ]; then
  OUT="vtl-diagnostics-$(hostname)-$(date +%Y%m%d-%H%M%S).tar.gz"
fi
case "$OUT" in
  /*) ;;
  *) OUT="$(pwd)/$OUT" ;;
esac

WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/vtl-diag.XXXXXX")"
COL="$WORKDIR/collect"
mkdir -p "$COL"

cleanup() {
  rm -rf "$WORKDIR"
}
trap cleanup EXIT

run() {
  _f="$1"
  shift
  {
    echo "### command: $*"
    echo
    "$@" 2>&1 || echo "[exit non-zero or command missing]"
    echo
  } >>"$COL/$_f.txt"
}

: >"$COL/00-meta.txt"
{
  echo "generated_utc=$(date -u +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date)"
  echo "local_time=$(date 2>/dev/null)"
  echo "user=$(id 2>/dev/null)"
  echo "hostname=$(hostname 2>/dev/null)"
  echo "script=$0"
  echo "output_tarball_will_be=$OUT"
} >>"$COL/00-meta.txt"

{
  uname -a 2>&1 || true
  echo
  cat /proc/version 2>/dev/null || true
} >"$COL/uname-version.txt"

uptime >"$COL/uptime.txt" 2>&1 || true
free -m >"$COL/free-m.txt" 2>&1 || true

if command -v dmesg >/dev/null 2>&1; then
  ( dmesg -T 2>/dev/null || dmesg 2>/dev/null ) | tail -n 8000 >"$COL/dmesg-tail.txt" 2>&1 || true
else
  echo "dmesg not available" >"$COL/dmesg-tail.txt"
fi

if command -v journalctl >/dev/null 2>&1; then
  journalctl -k -n 4000 --no-pager -o short-iso >"$COL/journal-kernel-recent.txt" 2>&1 || true
  JOURNAL_PREV_N="${VTL_DIAG_JOURNAL_PREV_LINES:-8000}"
  echo "journal_prev_boot_lines=$JOURNAL_PREV_N" >>"$COL/00-meta.txt"
  {
    echo "### journalctl -k -b -1 -n $JOURNAL_PREV_N (oldest-first; set VTL_DIAG_JOURNAL_PREV_LINES)"
    echo
    journalctl -k -b -1 -n "$JOURNAL_PREV_N" --no-pager -o short-iso 2>&1
  } >"$COL/journal-kernel-prev-boot.txt" || true
  JOURNAL_PREV_REV_N="${VTL_DIAG_JOURNAL_PREV_REV_LINES:-800}"
  echo "journal_prev_boot_rev_lines=$JOURNAL_PREV_REV_N" >>"$COL/00-meta.txt"
  {
    echo "### journalctl -k -b -1 -r -n $JOURNAL_PREV_REV_N (newest-first; set VTL_DIAG_JOURNAL_PREV_REV_LINES)"
    echo
    journalctl -k -b -1 -n "$JOURNAL_PREV_REV_N" -r --no-pager -o short-iso 2>&1
  } >"$COL/journal-kernel-prev-boot-rev.txt" || true
fi

run lsscsi lsscsi -g

run lsmod-filtered sh -c "lsmod 2>/dev/null | grep -E '^(vtl|tcm_|iscsi|target_core|loop|dm_)' || true"

if [ -d /sys/module/vtl ]; then
  mkdir -p "$COL/sys-module-vtl-parameters"
  for p in /sys/module/vtl/parameters/*; do
    [ -e "$p" ] || continue
    bn="$(basename "$p")"
    cat "$p" >"$COL/sys-module-vtl-parameters/$bn.txt" 2>&1 || true
  done
  {
    echo ""
    echo "### vtl kernel module (triage summary)"
    echo "vtl_loaded=yes"
    for key in serial_scsi_scan scan_async_quiesce_ms hotgeom_quiesce_ms noscan scan_delay_ms post_add_scan_delay_ms bringup_stagger_ms scan_host_stagger_ms move_delay_ms num_drives num_slots; do
      f="/sys/module/vtl/parameters/$key"
      if [ -r "$f" ]; then
        echo "vtl_param_${key}=$(cat "$f" 2>/dev/null || echo "?")"
      fi
    done
    if [ -r /sys/module/vtl/parameters/vtl_instances ]; then
      echo "vtl_param_vtl_instances=$(cat /sys/module/vtl/parameters/vtl_instances 2>/dev/null | head -c 200)"
    fi
    echo "vtl_note_ioctl_path=SET_INSTANCES runs on dedicated workqueue vtl_geom (see kernel vtl_main.c)."
  } >>"$COL/00-meta.txt"
else
  echo "no /sys/module/vtl (module not loaded?)" >"$COL/sys-module-vtl-missing.txt"
  {
    echo ""
    echo "### vtl kernel module (triage summary)"
    echo "vtl_loaded=no"
    echo "vtl_params_skipped=/sys/module/vtl absent (insmod vtl.ko before collect for serial_scsi_scan etc.)"
  } >>"$COL/00-meta.txt"
fi

run modinfo-vtl modinfo vtl 2>/dev/null

run scsi-hosts sh -c "ls -d /sys/class/scsi_host/host* 2>/dev/null | head -n 64 | while read -r d; do echo \"== \$d ==\"; cat \"\$d/unique_id\" 2>/dev/null; cat \"\$d/proc_name\" 2>/dev/null; done"

run dev-vtl ls -l /dev/vtl 2>/dev/null

run dev-sg-head sh -c "ls -l /dev/sg* 2>/dev/null | head -n 80 || true"

run st-head sh -c "ls -l /dev/st* /dev/nst* 2>/dev/null | head -n 40 || true"

run configfs-target sh -c "test -d /sys/kernel/config/target && ls -la /sys/kernel/config/target | head -n 80 || echo 'no configfs target'"

run mount-grep sh -c "mount | grep -E 'configfs|debugfs|tracefs' || true"

run ps-vtl sh -c "ps auxww 2>/dev/null | grep -E '[v]tladm|[t]argetcli|[i]scsi' || true"

for vbin in vtladm vtladm-iscsi; do
  if command -v "$vbin" >/dev/null 2>&1; then
    ( command -v "$vbin"; ls -l "$(command -v "$vbin")" 2>/dev/null ) >"$COL/${vbin}-path.txt" 2>&1 || true
    "$vbin" --help >"$COL/${vbin}--help.txt" 2>&1 || true
    "$vbin" -V >"$COL/${vbin}-V.txt" 2>&1 || true
  fi
done

# vtl.conf：常见路径；脱敏（若存在类似 key= 的敏感行可扩展 sed）
for conf in /opt/vtladm/var/vtl.conf /var/lib/vtl/vtl.conf /etc/vtl/vtl.conf; do
  if [ -f "$conf" ]; then
    sed -e 's/^[Pp]assword=.*/password=***REDACTED***/' \
        -e 's/^[Pp]ass=.*/pass=***REDACTED***/' \
        -e 's/^[Ss]ecret=.*/secret=***REDACTED***/' \
        -e 's/^[Tt]oken=.*/token=***REDACTED***/' \
        "$conf" >"$COL/$(echo "$conf" | tr / _).txt" 2>/dev/null || true
  fi
done

if command -v targetcli >/dev/null 2>&1; then
  if command -v timeout >/dev/null 2>&1; then
    timeout 45 targetcli ls / >"$COL/targetcli-ls-root.txt" 2>&1 || true
  else
    targetcli ls / >"$COL/targetcli-ls-root.txt" 2>&1 || true
  fi
else
  echo "targetcli not in PATH" >"$COL/targetcli-ls-root.txt"
fi

run ip-addr-head sh -c "command -v ip >/dev/null && ip -br addr 2>/dev/null | head -n 40 || ifconfig -a 2>/dev/null | head -n 60 || true"

run crash-dir sh -c "ls -la /var/crash 2>/dev/null | tail -n 50 || true"

# Latest kdump dir: panic/Oops lines from vmcore-dmesg (head is reboot banner; use tail + grep)
{
  _latest="$(ls -td /var/crash/127.0.0.1-* 2>/dev/null | head -1)"
  if [ -n "$_latest" ] && [ -f "$_latest/vmcore-dmesg.txt" ]; then
    echo "latest_crash_dir=$_latest"
    echo "### tail -400 vmcore-dmesg.txt"
    tail -n 400 "$_latest/vmcore-dmesg.txt" 2>/dev/null || true
    echo
    echo "### grep panic|Oops|Call Trace|vtl (vmcore-dmesg)"
    grep -nE 'panic|Oops|BUG:|Call Trace| RIP:|vtl|VTL:' "$_latest/vmcore-dmesg.txt" 2>/dev/null | tail -n 120 || true
  else
    echo "no vmcore-dmesg.txt in latest /var/crash/* (or kdump empty)"
  fi
} >"$COL/crash-vmcore-dmesg-hint.txt" 2>&1 || true

run kdump sh -c "grep -E '^path|^default' /etc/kdump.conf 2>/dev/null | grep -v '^#' || true"

run lscpu-head lscpu 2>/dev/null | head -n 40

run scsi-disk-summary sh -c "ls /sys/block 2>/dev/null | grep -E '^sd' | head -n 40 || true"

(
  cd "$WORKDIR"
  tar -czf "$OUT" collect
)

echo "Diagnostics archive written: $OUT"
echo "Inspect: tar -tzf '$OUT' | head"
