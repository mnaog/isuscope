# isuscope setup

このディレクトリは`isuscope init`が一度だけ生成する雛形です。AIまたは担当者は、ベンチを開始する前に次の順序で準備します。

1. ベンチ起動方法を調べ、`benchmark.sh`冒頭の契約に従って当日実装する
2. 必要なら`parse-benchmark.sh`で問題固有のbenchmark出力をmetric JSONLへ変換する
3. Codex会話履歴とrunを紐付ける場合は、新しいCodexセッションを開始する前に`UserPromptSubmit` hookを導入・信頼し、`[context.codex]`を有効化する
4. SSH可能なnode、role、identity fileを`[[nodes]]`と`[ssh]`へ設定する。roleは固定的な種類ではなく、複数指定・run間の変更が可能なcollector選択tag
5. Nginxアクセスログに時刻、匿名化session、method、URIがあるか確認する
6. 生成済みのsysstat、perf、host-sampler、service-sampler、alp、slp、optionalなperf-flamegraph/offcpu collectorを確認する。`[observability].service_units`には実際に負荷を担う少数のsystemd unitだけを指定し、不要なら空のままにする。アクセスログ・slow logのpathとformat（時系列用field名を含む）を実環境へ合わせる。Flame Graph scriptsや`offcputime-bpfcc`がなければcollectorは`unavailable`になる。既定commandはalp 1.0.21とslp 0.2.1で検証済み。ALPの正確なcount、status、sum/avg、p50/p95/p99集約のため、`routes.toml`はpatternにcomma、replaceに`$1`などのcaptureを使わず、1規則から固定canonical routeへ置換する
7. `fingerprint.sh`のapp binaryやservice名を問題環境へ合わせ、各nodeへ冪等配置する
8. `bash -n benchmark.sh`、`bash -n parse-benchmark.sh`、`bash -n setup.sh`、`isuscope list`を実行してから、不足する場合だけ`setup.sh`の`apply_environment`へ冪等な導入処理を追加する
9. ここで初めて`setup.sh`を実行し、`setup-state.json`が生成されることを確認する。標準ツールは自動installされない
10. `isuscope doctor`を実行し、failureを解消する
11. `isuscope survey-run --hypothesis "初期状態の負荷構造を記録する"`を一度実行し、`isuscope report latest`でcollector、metric、時系列とtransitionが0件でないことを確認する。必要な時間帯は`isuscope series latest`で掘り下げ、PASS後は出力されたIDを指定して`isuscope analyze RUN_ID VERDICT --analysis "結果"`で記録する

remote変更を行う場合は、既存ファイルのbackup、設定検証、atomicな配置、必要最小限のreloadを行います。パッケージ導入やremote build、常駐agentは既存機能で代替できない場合だけ使用します。

標準log collectorは`sha256sum`、`gzip`、`tail`、`wc`を使い、`.1`〜`.5`と各`.gz`から開始時のlogを照合します。保持世代を越えたrotationや中間世代の欠落は、壊れた差分を返さず終了コード75で`unavailable`になります。非空logをalp/slpが1件も解析できなかった場合は設定不一致として`failed`になります。

`benchmark.sh`、`parse-benchmark.sh`、`setup.sh`、`config.toml`、`routes.toml`、`setup-state.json`およびisuscopeのversionは各runの`tooling/`へsnapshotされます。序盤の`survey-run`完了後は、仮説付きの`run`、結果を残す`analyze`、`enrich`、一覧JSONの`list`、小さい判断用JSONの`brief`、詳細JSONの`report`、対象metricを絞る`query`、比較JSONの`diff`、人間向けHTMLの`ui`を使います。

改善前後の対象を絞った比較には`isuscope query CANDIDATE --base BASE ...`を使います。両runへ同じfilterを適用し、全件比較後にlimitされます。

`[context.codex]`を有効にした場合、runは`CODEX_SESSION_ID`または`CODEX_THREAD_ID`と一致するhistory fileだけを採用し、最後のUser `turn_id`をinput IDとして保存します。通常ターミナル、別セッション、hook未起動ではfallbackせず、benchmarkを開始しません。
