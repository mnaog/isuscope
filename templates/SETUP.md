# isuscope setup

このディレクトリは`isuscope init`が一度だけ生成する雛形です。AIまたは担当者は、ベンチを開始する前に次の順序で準備します。

1. ベンチ起動方法を調べ、`benchmark.sh`冒頭の契約に従って当日実装する。初回は出力の形が分からないので、scoreと合否を読めなければadapterは判定を保留（`pending`）して出力を残してよい。保存された出力から読み取りを確かめたら、`[benchmark] rescore_command`（例: `[".isuscope/benchmark.sh", "--extract", "{benchmark_stdout}"]`）で`isuscope rescore <run>`が確定する
2. benchmarkerが運営向けにだけ出す行があれば、`config.toml`の`[benchmark] operator_line_pattern`へ正規表現を書く（保存前に捨てられ、log・metric・parserに残らない）
3. 問題固有のbenchmark出力（FAILの理由、エラーの実例、scenarioごとの件数）は`parse-benchmark.sh`でmetric・message JSONLへ変換し、`[[benchmark.parsers]]`へ登録する。登録しないと`isuscope enrich`もparserを動かさない
4. ベンチの前後に動くcommandを必要なだけ`[benchmark]`へ書く。`prepare_command`は毎回のベンチの前（アプリの再起動とlistenの待機など）、`identity_command`は配ったものを識別する値（JSON objectならfieldごとに比べる）、`cleanup_command`はisuscopeがベンチを止めたときの後始末。期限は`timeout_seconds`（ベンチ全体）、`idle_timeout_seconds`（出力の途絶え）、`hook_timeout_seconds`（各command）。identity・fingerprint・計測設定がどれも直前のrunと同じなら、isuscopeはベンチを始めない（意図した再測定は`isuscope run --repeat "理由"`）
5. initializeと負荷走行の境界を決める。benchmarkerの出力に開始・完了の文言があれば`initialize_start_marker`・`initialize_finish_marker`へ書く。無ければ（または確かでなければ）`routes.toml`の`[initialize]`へinitializeの要求の`method`と`path`を書く。access logでその要求が応答を返し終えた時刻を負荷の始まりにするので、初回runから区間が分かれ、markerは要らない（`isuscope doctor`もmarkerを求めない）
6. Codex・Claude Codeの会話履歴とrunを紐付ける場合は、新しいセッションを開始する前に共通の会話履歴hookを導入・信頼し、`[context.agent]`を有効化する。ベンチはその履歴を書いているセッション（このproject内で起動したもの）から動かす
7. SSH可能なnode、role、identity fileを`[[nodes]]`と`[ssh]`へ設定する。roleは固定的な種類ではなく、複数指定・run間の変更が可能なcollector選択tag
8. Nginxアクセスログに時刻、匿名化session、method、URI、接続番号（`conn:$connection`）、応答完了時刻（`msec:$msec`）、接続先（`upstream_addr`）と`upstream_connect`・`upstream_header`があるか確認する。接続番号とmsecが無いとベンチ側の接続の使い方（`client.*`）は出ず、upstream系が無いと接続先別の内訳（briefの`upstreams`）は出ない。要求の来ないnode（ベンチの宛先でないapp）はaccess logが増えないだけで、HTTPの欠落としては数えない
9. 生成済みのsysstat、perf、host-sampler、service-sampler、alp、mysql-digest、optionalなperf-flamegraph/offcpu collectorを確認する。`[observability].service_units`には実際に負荷を担う少数のsystemd unitだけを指定し、不要なら空のままにする。アクセスログのpathとformat（時系列用field名を含む）を実環境へ合わせる。SQLは既定でperformance_schemaのstatement digestと`prepared_statements_instances`（sqlxなどのserver側prepared statement）から取るのでslow logは要らない。MariaDBやperformance_schemaが無効な環境ではslow log（`long_query_time=0`）を有効にし、slow log系collector（mysql-log-delta、slp）のpathを合わせる。Flame Graph scriptsや`offcputime-bpfcc`がなければcollectorは`unavailable`になる。既定commandはalp 1.0.21とslp 0.2.1で検証済み（awkはmawk・gawk・BSD awkで動く）。alpはnodeに入っている前提で、無ければcollectorは`failed`になる。ALPの正確なcount、status、sum/avg、p50/p95/p99集約のため、`routes.toml`はpatternにcommaと`?`（alpは`?`を含むgroupに一致させられない。省略できる部分は選択肢で書く）、replaceに`$1`などのcaptureを使わず、1規則から固定canonical routeへ置換する。同じrouteへ置換する規則は隣り合わせて並べる（alpへ1つのgroupとして渡し、分位点を正しく出す。離れていると、そのrouteの分位点は出さない）
10. `fingerprint.sh`を各nodeへ冪等配置し、比べたいpathを1行ずつ`/usr/local/lib/isuscope/fingerprint-paths`へ書く。毎回のベンチの前に全nodeで読むので、app binary、unit、環境変数file、nginx・MySQLの設定のような単一fileに絞る。配った大きなdirectory（静的file、build cacheを含むsource）は`identity_command`で比べる
11. `bash -n benchmark.sh`、`bash -n parse-benchmark.sh`、`bash -n setup.sh`、`isuscope list`を実行してから、不足する場合だけ`setup.sh`の`apply_environment`へ冪等な導入処理を追加する
12. ここで初めて`setup.sh`を実行し、`setup-state.json`が生成されることを確認する。標準ツールは自動installされない
13. `isuscope doctor`を実行し、failureを解消する
14. `isuscope survey-run --hypothesis "初期状態の負荷構造を記録する"`を一度実行し、`isuscope brief latest`でcollector、metric、時系列とtransitionが0件でないことを確認する。判定が保留されたら、出力から読み取りを確かめて`isuscope rescore <run>`で確定する。必要な時間帯は`isuscope series latest`で掘り下げ、PASS後は出力されたIDを指定して`isuscope analyze RUN_ID VERDICT --analysis "結果"`で記録する

remote変更を行う場合は、既存ファイルのbackup、設定検証、atomicな配置、必要最小限のreloadを行います。パッケージ導入やremote build、常駐agentは既存機能で代替できない場合だけ使用します。

標準log collectorは`sha256sum`、`gzip`、`tail`、`wc`を使い、`.1`〜`.5`と各`.gz`から開始時のlogを照合します。保持世代を越えたrotationや中間世代の欠落は、壊れた差分を返さず終了コード75で`unavailable`になります。非空logをalp collector（access log、LTSVのmethodとuriを1件も読めない）やslp（slow logを有効にした場合）が1件も解析できなかった場合（slow logの文がどれも`SET timestamp=`を持たず区間へ振り分けられなかった場合を含む）は設定不一致として`failed`になります。

`benchmark.sh`、`parse-benchmark.sh`、`setup.sh`、`config.toml`、`routes.toml`、`setup-state.json`およびisuscopeのversionは各runの`tooling/`へsnapshotされます。序盤の`survey-run`完了後は、仮説付きの`run`、結果を残す`analyze`、`enrich`、一覧JSONの`list`、小さい判断用JSONの`brief`、対象metricを絞る`query`（`--base`で2 runの比較）、briefとqueryで足りない問いに使う読み取り専用の`sql`を使います。

改善前後の対象を絞った比較には`isuscope query CANDIDATE --base BASE ...`を使います。両runへ同じfilterを適用し、全件比較後にlimitされます。

`[context.agent]`を有効にした場合、runはCodexの`CODEX_SESSION_ID`／`CODEX_THREAD_ID`、またはClaude Codeの`CLAUDE_CODE_SESSION_ID`と一致するhistory fileだけを採用し、最後のUser入力のID（Codexは`turn_id`、Claude Codeは`prompt_id`）をinput IDとして保存します。旧名の`[context.codex]`も読み込めます。通常ターミナル、別セッション、hook未起動ではfallbackせず、benchmarkを開始しません。
