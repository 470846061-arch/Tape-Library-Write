#!/bin/sh
# Scan VTL SCSI hosts one at a time (safe after insmod with noscan=1).
# Usage: sudo sh vtl-scsi-scan-all-hosts.sh [sleep_sec_between_hosts]

set -eu

SLEEP_SEC="${1:-5}"

if ! lsmod 2>/dev/null | awk '{print $1}' | grep -qx vtl; then
  echo "vtl.ko not loaded" >&2
  exit 1
fi

_n=0
for _h in /sys/class/scsi_host/host*; do
  [ -d "$_h" ] || continue
  [ -f "$_h/proc_name" ] || continue
  _pn=$(tr -d '\n\r ' <"$_h/proc_name" 2>/dev/null) || continue
  case "$_pn" in vtl|VTL) ;; *) continue ;; esac
  _n=$((_n + 1))
  echo "scan $_h ..."
  printf '%s\n' '- - -' >"$_h/scan"
  if [ "$SLEEP_SEC" -gt 0 ] 2>/dev/null; then
  sleep "$SLEEP_SEC"
  fi
done

if [ "$_n" -eq 0 ]; then
  echo "no vtl SCSI hosts found (insmod finished? check dmesg)" >&2
  exit 1
fi

echo "scanned $_n VTL host(s); check: lsscsi -g | grep -i VTL"
