#!/bin/sh
set -eu

# 各nodeで今動いている構成を識別する値を出します。isuscopeは直前のベンチと比べ、配ったものや
# 設定が何も変わっていないrunを（`--repeat`で理由を書かない限り）始めません。
# 比べたいfileやdirectoryは、1行に1つのpathで/usr/local/lib/isuscope/fingerprint-pathsへ並べます
# （app binary、配置したsource、systemd unit、環境変数fileなど。配る対象から機械的に作るのが確実です）。
# 大きなdirectory（build cacheなど）は時間がかかるので、配る成果物に絞ります。

escape_json() {
  sed 's/\\/\\\\/g; s/"/\\"/g; s/[[:cntrl:]]/ /g'
}

emit() {
  name=$1
  value=$(printf '%s' "$2" | escape_json)
  printf '{"type":"fingerprint","name":"%s","value":"%s"}\n' "$name" "$value"
}

# fileは中身、directoryは配下の全fileの名前と中身を1つのhashにまとめる。
hash_path() {
  if [ -f "$1" ]; then
    sha256sum "$1" | awk '{print $1}'
  elif [ -d "$1" ]; then
    (cd "$1" && find . -type f -print0 | LC_ALL=C sort -z | xargs -0 -r sha256sum) | sha256sum | awk '{print $1}'
  else
    printf 'missing'
  fi
}

path_key() {
  printf '%s' "$1" | sha256sum | awk '{print substr($1, 1, 16)}'
}

emit kernel "$(uname -srmo)"
# /etc/os-release is the standard source on the supported Linux hosts.
# shellcheck disable=SC1091
emit os.release "$(. /etc/os-release; printf '%s %s' "${ID:-unknown}" "${VERSION_ID:-unknown}")"
targets_file=/usr/local/lib/isuscope/fingerprint-paths
if [ -r "${targets_file}" ]; then
  while IFS= read -r path; do
    case "${path}" in ''|'#'*) continue ;; esac
    emit "file.$(path_key "${path}").sha256" "${path}:$(hash_path "${path}")"
  done <"${targets_file}"
fi
if command -v nginx >/dev/null 2>&1; then
  emit nginx.version "$(nginx -v 2>&1)"
  emit nginx.config.sha256 "$(nginx -T 2>&1 | sha256sum | awk '{print $1}')"
else
  emit nginx.version unavailable
fi
if command -v mysql >/dev/null 2>&1; then
  emit mysql.version "$(mysql --version)"
  # 読み込まれ得る設定file（MySQL・MariaDBの標準の置き場所）の名前と中身。
  emit mysql.config.sha256 "$(
    for config in /etc/my.cnf /etc/mysql; do
      test -e "${config}" || continue
      find "${config}" -type f -name '*.cnf' -print0
    done | LC_ALL=C sort -z | xargs -0 -r sha256sum | sha256sum | awk '{print $1}'
  )"
else
  emit mysql.version unavailable
fi
if command -v systemctl >/dev/null 2>&1; then
  emit systemd.running.sha256 "$(systemctl list-units --type=service --state=running --no-legend --no-pager 2>/dev/null | sha256sum | awk '{print $1}')"
fi
