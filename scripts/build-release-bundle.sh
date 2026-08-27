#!/usr/bin/env bash
set -euo pipefail

readonly PROJECT_ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
readonly DIST_DIR="${PROJECT_ROOT}/dist"

cd "${PROJECT_ROOT}"
cargo build --locked --release
version=$(target/release/isuscope --version | awk '{print $2}')
host=$(rustc -vV | awk '/^host:/ {print $2}')
bundle="isuscope-${version}-${host}"
temporary=$(mktemp -d "${DIST_DIR}.tmp.XXXXXX")
verification=$(mktemp -d "${DIST_DIR}.verify.XXXXXX")
trap 'rm -rf -- "${temporary}" "${verification}"' EXIT
install -m 0755 target/release/isuscope "${temporary}/isuscope"
mkdir -p "${temporary}/docs"
cp README.md LICENSE "${temporary}/"
cp docs/contest-day.md "${temporary}/docs/contest-day.md"
mkdir -p "${DIST_DIR}"
tar -C "${temporary}" -czf "${DIST_DIR}/${bundle}.tar.gz" isuscope README.md LICENSE docs/contest-day.md
if command -v sha256sum >/dev/null 2>&1; then
  sha256sum "${DIST_DIR}/${bundle}.tar.gz" >"${DIST_DIR}/${bundle}.tar.gz.sha256"
else
  shasum -a 256 "${DIST_DIR}/${bundle}.tar.gz" >"${DIST_DIR}/${bundle}.tar.gz.sha256"
fi
tar -C "${verification}" -xzf "${DIST_DIR}/${bundle}.tar.gz"
test "$("${verification}/isuscope" --version)" = "isuscope ${version}"
test -f "${verification}/README.md"
test -f "${verification}/LICENSE"
test -f "${verification}/docs/contest-day.md"
echo "created ${DIST_DIR}/${bundle}.tar.gz"
echo "verified ${bundle} (binary and documentation)"
