#!/usr/bin/env bash
set -euo pipefail

# 問題固有のbenchmark出力を、isuscopeの汎用metricとmessageのJSONLへ変換します。
# このscriptはベンチ終了後にだけ実行されるため、採点中の負荷にはなりません。
# 第一引数はbenchmark stdoutのzstd圧縮logです。
#
# 出力例:
#   printf '%s\n' \
#     '{"type":"metric","name":"benchmark.error","value":12,"unit":"errors","labels":{"error":"timeout"}}'
#
# FAILの理由とエラーの実例は、行の本文をmessageとして残します（件数はmetricで出します）。
# FAIL runは分析不要なので、ここで残した理由が`list`と`brief`での唯一の記録になります。
# benchmarkerが運営向けに出す行（ISUCON12の`[ADMIN]`など）はルール側の情報なので使いません。
#   printf '%s\n' \
#     '{"type":"message","kind":"failure","text":"整合性チェックに失敗: GET /home expected(403) != actual(401)"}' \
#     '{"type":"message","kind":"error","category":"timeout","text":"Get \"http://10.0.0.1/home\": dial tcp 10.0.0.1:80: i/o timeout"}'
#
# 当日は、まず何も解析せずベンチを保存し、このscriptを編集してから次を実行できます。
#   isuscope enrich RUN_ID

input=${1:?benchmark stdout path is required}

# --- 当日にこの範囲を実装する ---
# zstd -dc -- "$input" | awk '...'
zstd -dc -- "$input" >/dev/null
# --- ここまで ---
