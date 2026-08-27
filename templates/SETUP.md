# isuscope setup

このディレクトリは`isuscope init`が一度だけ生成する雛形です。AIまたは担当者は、ベンチを開始する前に次の順序で準備します。

1. ベンチ起動方法を調べ、`benchmark.sh`冒頭の契約に従って当日実装する
2. 必要なら`parse-benchmark.sh`で問題固有のbenchmark出力をmetric JSONLへ変換する
3. SSH可能なnode、role、identity fileを`[[nodes]]`と`[ssh]`へ設定する。roleは固定的な種類ではなく、複数指定・run間の変更が可能なcollector選択tag
4. Nginxアクセスログに時刻、匿名化session、method、URIがあるか確認する
5. 生成済みのsysstat、perf、host-sampler、alp、slpと時系列collectorを確認し、アクセスログ・slow logのpathとformat（時系列用field名を含む）だけ実環境へ合わせる。既定commandはalp 1.0.21とslp 0.2.1で検証済み
6. `fingerprint.sh`のapp binaryやservice名を問題環境へ合わせ、各nodeへ冪等配置する
7. `bash -n benchmark.sh`、`bash -n parse-benchmark.sh`、`bash -n setup.sh`、`isuscope show`を実行してから、不足する場合だけ`setup.sh`の`apply_environment`へ冪等な導入処理を追加する
8. ここで初めて`setup.sh`を実行し、`setup-state.json`が生成されることを確認する。標準ツールは自動installされない
9. `isuscope doctor`を実行し、failureを解消する
10. `isuscope discovery-run --hypothesis "初期状態の負荷構造を記録する"`を一度実行し、collectorとtransitionが0件でないことを確認する。PASS後は`isuscope analyze latest`で結果を記録する

remote変更を行う場合は、既存ファイルのbackup、設定検証、atomicな配置、必要最小限のreloadを行います。パッケージ導入やremote build、常駐agentは既存機能で代替できない場合だけ使用します。

標準log collectorは`sha256sum`、`gzip`、`tail`、`wc`を使い、`.1`〜`.5`と各`.gz`から開始時のlogを照合します。保持世代を越えたrotationや中間世代の欠落は、壊れた差分を返さず終了コード75で`unavailable`になります。非空logをalp/slpが1件も解析できなかった場合は設定不一致として`failed`になります。

`benchmark.sh`、`parse-benchmark.sh`、`setup.sh`、`config.toml`、`routes.toml`、`setup-state.json`およびisuscopeのversionは各runの`tooling/`へsnapshotされます。以後は仮説付きの`run`、結果を残す`analyze`、`discovery-run`、`score-run`、`enrich`、`show`を使います。
