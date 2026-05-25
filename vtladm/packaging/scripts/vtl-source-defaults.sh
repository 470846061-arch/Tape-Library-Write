# Source /etc/default/vtladm without failing on Windows CRLF (common when copied from git on Windows).
# Usage: . /opt/vtladm/scripts/vtl-source-defaults.sh && _vtl_source_defaults /etc/default/vtladm

_vtl_source_defaults() {
  _f="${1:-/etc/default/vtladm}"
  [ -f "$_f" ] || return 0
  command -v mktemp >/dev/null 2>&1 || return 1
  _t=$(mktemp "${TMPDIR:-/tmp}/vtladm-defaults.XXXXXX") || return 1
  if ! sed 's/\r$//' "$_f" > "$_t"; then
    rm -f "$_t"
    return 1
  fi
  # shellcheck disable=SC1090
  . "$_t"
  rm -f "$_t"
}
