#!/usr/bin/env bash
set -euo pipefail

# isuscope benchmark adapter protocol v1
#
# ISUCON開始後に、AIまたは担当者が調査したベンチ起動方法をこのファイルへ実装します。
# isuscope本体や日常CLIを変更する必要はありません。直接bench binaryを実行する方式、
# portal APIをcurlで開始してpollする方式のどちらでも、以下の契約へ合わせます。
#
# 必須の契約:
#   1. ベンチを1回だけ開始し、結果が確定するまで待つ。
#   2. 最後に結果を1行JSONでstdoutへ出す。
#      {"type":"isuscope.result","score":12345,"pass":true,"messages":[]}
#   3. 結果を取得できた場合は、ベンチ判定がfailでもexit 0にする。passで判定を伝える。
#      起動・認証・poll・parseなどアダプター自体の失敗時だけnon-zeroで終了する。
#
# 任意のinitialize境界:
#   printf '%s\n' '{"type":"isuscope.event","name":"initialize-started"}'
#   printf '%s\n' '{"type":"isuscope.event","name":"initialize-finished"}'
#
# 任意のmetric（問題固有parserを使わず直接保存する場合）:
#   printf '%s\n' '{"type":"metric","name":"benchmark.viewer.completed","value":123,"unit":"viewers"}'
#
# 利用できる環境変数:
#   ISUSCOPE_BENCHMARK_PROTOCOL=v1
#   ISUSCOPE_PROJECT_ROOT=<対象プロジェクトの絶対path>
#   ISUSCOPE_RUN_DIR=<今回の一時run directoryの絶対path>
#
# stdout/stderrはrunへ保存されます。token、cookieなどの秘密情報を表示しないでください。

# --- 当日にこの範囲を実装する ---
printf '%s\n' \
  '{"type":"isuscope.result","pass":false,"messages":[".isuscope/benchmark.sh is not configured"]}'
exit 2
# --- ここまで ---
