#!/bin/sh
# Probe medium-changer SCSI paths that backup software (Mars/TSM) use for inventory.
# Run on VTL host as root after insmod; pass changer /dev/sg (LUN0), e.g. /dev/sg5.
set -eu

SG="${1:-}"
if [ -z "$SG" ] || [ ! -e "$SG" ]; then
  echo "usage: $0 /dev/sgN   # medium changer (lsscsi -g, LUN 0)" >&2
  exit 2
fi

echo "=== INQUIRY $SG ==="
sg_inq "$SG" 2>/dev/null || true

echo ""
echo "=== INQUIRY VPD 0x80 (unit serial) ==="
sg_vpd -p 0x80 "$SG" 2>/dev/null || echo "FAIL: sg_vpd 0x80"

echo ""
echo "=== MODE SENSE page 0x1D (element addresses) ==="
sg_modes -p 0x1d "$SG" 2>/dev/null | head -20 || echo "FAIL: sg_modes 0x1d"

echo ""
echo "=== MODE SENSE page 0x3F (all pages — Mars-style) ==="
sg_modes -p 0x3f "$SG" 2>/dev/null | head -30 || echo "FAIL: sg_modes 0x3f"

echo ""
echo "=== READ ELEMENT STATUS (voltag) via mtx ==="
if command -v mtx >/dev/null 2>&1; then
  mtx -f "$SG" status 2>&1 | head -40
else
  echo "mtx not installed"
fi

echo ""
echo "=== TEST UNIT READY (tape drives — Mars inventory probes these) ==="
_tur_probe() {
  _dev="$1"
  if command -v sg_raw >/dev/null 2>&1; then
    if sg_raw -r 0 "$_dev" 00 00 00 00 00 00 >/dev/null 2>&1; then
      echo "${_dev}: TUR OK (sg_raw)"
      return 0
    fi
    echo "${_dev}: FAIL sg_raw TUR"
  fi
  if sg_turs -l "$_dev" >/dev/null 2>&1; then
    echo "${_dev}: TUR OK (sg_turs -l)"
    return 0
  fi
  echo -n "${_dev}: "
  sg_turs "$_dev" 2>&1 || true
  return 1
}
for _d in /dev/sg6 /dev/sg7; do
  [ -e "$_d" ] && _tur_probe "$_d" || true
done

echo ""
echo "=== iSCSI export hint ==="
echo "Backup host must login the same IQN/LUN map as library-export (changer=LUN0, drives=LUN1..N)."
lsscsi -g 2>/dev/null | grep -iE 'mediumx|tape|IBM|03584|VTL|STK' || lsscsi -g 2>/dev/null | tail -15
