#!/usr/bin/env bash
set -euo pipefail

readonly SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
readonly PROJECT_ROOT=$(cd -- "${SCRIPT_DIR}/.." && pwd)
readonly CONFIG_FILE="${SCRIPT_DIR}/config.toml"
readonly ROUTES_FILE="${SCRIPT_DIR}/routes.toml"
readonly STATE_FILE="${SCRIPT_DIR}/setup-state.json"

hash_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

apply_environment() {
  # AIが環境を調査し、必要な場合だけここへ冪等な注入処理を追加します。
  # 既存アクセスログで必要なfieldが得られる場合、remote変更は不要です。
  :
}

command -v isuscope >/dev/null
test -f "${CONFIG_FILE}"
apply_environment

# configが読み取れることを副作用のないshowで検証します。
(cd "${PROJECT_ROOT}" && isuscope show >/dev/null)

config_hash=$(hash_file "${CONFIG_FILE}")
routes_hash=""
if [[ -f "${ROUTES_FILE}" ]]; then
  routes_hash=$(hash_file "${ROUTES_FILE}")
fi
isuscope_version=$(isuscope --version | tr -d '\n')
temporary=$(mktemp "${STATE_FILE}.XXXXXX")
trap 'rm -f -- "${temporary}"' EXIT
printf '{\n  "isuscope_version": "%s",\n  "config_sha256": "%s",\n  "routes_sha256": "%s"\n}\n' \
  "${isuscope_version}" "${config_hash}" "${routes_hash}" >"${temporary}"
mv -- "${temporary}" "${STATE_FILE}"
trap - EXIT
echo "isuscope setup complete: ${STATE_FILE}"
