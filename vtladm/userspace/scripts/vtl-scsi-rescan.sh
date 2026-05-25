#!/bin/sh
# mhVTL-style: refresh SCSI bus for existing vtl hosts (no rmmod/insmod).
# Writes "- - -" to /sys/class/scsi_host/host*/scan for each host whose proc_name is "vtl".
# Does NOT add or remove SCSI hosts — new online libraries (extra vtl_instances segment) still
# need /dev/vtl SET_INSTANCES or vtl-kernel-reload.sh. See userspace/docs/SCSI.md §1f.

set -eu
n=0
for scan in /sys/class/scsi_host/host*/scan; do
  [ -e "$scan" ] || continue
  [ -w "$scan" ] || continue
  d=$(dirname "$scan")
  [ -r "$d/proc_name" ] || continue
  pn=$(tr -d '\r\n' <"$d/proc_name")
  [ "$pn" = vtl ] || continue
  # Match userspace `SCAN_LINE` / `echo '- - -' > scan` (trailing newline).
  printf '%s\n' '- - -' >"$scan" || exit 1
  n=$((n + 1))
done
if [ "$n" -eq 0 ]; then
  echo "vtl-scsi-rescan: no vtl scsi_host found (vtl.ko loaded? run as root?)" >&2
  exit 1
fi
echo "vtl-scsi-rescan: triggered scan on $n host(s)"
