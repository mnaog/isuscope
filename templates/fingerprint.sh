#!/bin/sh
set -eu

escape_json() {
  sed 's/\\/\\\\/g; s/"/\\"/g; s/[[:cntrl:]]/ /g'
}

emit() {
  name=$1
  value=$(printf '%s' "$2" | escape_json)
  printf '{"type":"fingerprint","name":"%s","value":"%s"}\n' "$name" "$value"
}

hash_file() {
  if [ -f "$1" ]; then
    sha256sum "$1" | awk '{print $1}'
  else
    printf 'missing'
  fi
}

emit kernel "$(uname -srmo)"
# /etc/os-release is the standard source on the supported Linux hosts.
# shellcheck disable=SC1091
emit os.release "$(. /etc/os-release; printf '%s %s' "${ID:-unknown}" "${VERSION_ID:-unknown}")"
emit app.binary.sha256 "$(hash_file /home/isucon/webapp/rust/target/release/isupipe)"
emit app.env.sha256 "$(hash_file /home/isucon/env.sh)"
emit nginx.version "$(nginx -v 2>&1 || true)"
emit nginx.config.sha256 "$(nginx -T 2>&1 | sha256sum | awk '{print $1}')"
if command -v mysql >/dev/null 2>&1; then
  emit mysql.version "$(mysql --version)"
else
  emit mysql.version unavailable
fi
