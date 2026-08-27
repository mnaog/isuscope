# isuscope setup

このディレクトリは`isuscope init`が一度だけ生成する雛形です。AIまたは担当者は、ベンチを開始する前に次の順序で準備します。

1. ベンチ起動方法を調べ、`benchmark.sh`冒頭の契約に従って当日実装する
2. SSH可能なnode、role、identity fileを`[[nodes]]`と`[ssh]`へ設定する。roleは固定的な種類ではなく、複数指定・run間の変更が可能なcollector選択tag
3. Nginxアクセスログに時刻、匿名化session、method、URIがあるか確認する
4. 生成済みのsysstat、perf、host-sampler、sysstat、alp、slpと時系列collectorを確認し、アクセスログ・slow logのpathとformat（時系列用field名を含む）だけ実環境へ合わせる
5. `fingerprint.sh`のapp binaryやservice名を問題環境へ合わせ、各nodeへ冪等配置する
6. `bash -n benchmark.sh`、`bash -n setup.sh`、`isuscope show`を実行してから、不足する場合だけ`setup.sh`の`apply_environment`へ冪等な導入処理を追加する
7. ここで初めて`setup.sh`を実行し、`setup-state.json`が生成されることを確認する。標準ツールは自動installされない
8. `isuscope discovery-run`を一度実行し、標準collectorの`complete`/`unavailable`、metric、transitionを確認する

remote変更を行う場合は、既存ファイルのbackup、設定検証、atomicな配置、必要最小限のreloadを行います。パッケージ導入やremote build、常駐agentは既存機能で代替できない場合だけ使用します。

`benchmark.sh`、`setup.sh`、`config.toml`、`routes.toml`、`setup-state.json`およびisuscopeのversionは各runの`tooling/`へsnapshotされます。以後の日常操作は`run`、`discovery-run`、`show`だけです。
