#!/usr/bin/env bash
set -euo pipefail

# 問題固有のbenchmark出力を、isuscopeの汎用metric JSONLへ変換します。
# このscriptはベンチ終了後にだけ実行されるため、採点中の負荷にはなりません。
# 第一引数はbenchmark stdoutのzstd圧縮logです。
#
# 出力例:
#   printf '%s\n' \
#     '{"type":"metric","name":"benchmark.scenario.success","value":123,"unit":"runs","labels":{"scenario":"viewer"}}'
#
# 当日は、まず何も解析せずベンチを保存し、このscriptを編集してから次を実行できます。
#   isuscope enrich latest

input=${1:?benchmark stdout path is required}

# --- 当日にこの範囲を実装する ---
# zstd -dc -- "$input" | awk '...'
zstd -dc -- "$input" >/dev/null
# --- ここまで ---
