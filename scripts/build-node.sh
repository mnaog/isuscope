#!/usr/bin/env bash
# isuscope-nodeを、nodeへ置くLinuxの静的binary（musl、amd64とarm64）としてbuildする。
# 依存はregexだけなので、linkerはRustに付属するrust-lldで足り、macOSからもDocker無しで作れる。
#   ./scripts/build-node.sh OUT_DIR
# OUT_DIRへisuscope-node-linux-amd64、isuscope-node-linux-arm64、BUILD_ID、SHA256SUMSを書く。
# BUILD_IDは集計のsource（src/access_summary.rs）から決まり、同じsourceからbuildしたisuscopeの
# `isuscope __access-summary --build-id`と一致する。collectorはこれで照合する。
set -euo pipefail

PROJECT_ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
readonly PROJECT_ROOT
out=${1:?usage: scripts/build-node.sh OUT_DIR}
mkdir -p "${out}"
out=$(cd -- "${out}" && pwd)

cd "${PROJECT_ROOT}"
for pair in x86_64-unknown-linux-musl:amd64 aarch64-unknown-linux-musl:arm64; do
  target=${pair%%:*}
  arch=${pair##*:}
  if command -v rustup >/dev/null 2>&1; then
    rustup target add "${target}" >/dev/null
  fi
  linker_variable="CARGO_TARGET_$(printf '%s' "${target}" | tr 'a-z-' 'A-Z_')_LINKER"
  env "${linker_variable}=rust-lld" \
    cargo build --locked --release -p isuscope-node --target "${target}"
  install -m 0755 "target/${target}/release/isuscope-node" "${out}/isuscope-node-linux-${arch}"
done
cargo run --locked --quiet --release -p isuscope-node -- --build-id >"${out}/BUILD_ID"
if command -v sha256sum >/dev/null 2>&1; then
  (cd "${out}" && sha256sum isuscope-node-linux-amd64 isuscope-node-linux-arm64 >SHA256SUMS)
else
  (cd "${out}" && shasum -a 256 isuscope-node-linux-amd64 isuscope-node-linux-arm64 >SHA256SUMS)
fi
echo "isuscope-node $(cat "${out}/BUILD_ID") -> ${out}"
