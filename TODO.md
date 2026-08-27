# TODO

未完了の項目だけを記載します。

## ISUCON相当のLinux実環境で検証すること

- Dockerでは再現できないhost kernel上のperfについて、ISUCONと同系統のkernelで権限・hardware counter・完全な出力を採取する。
- database、perf、HTTP、host resourceを同時に取得した実ISUCON runを作り、複数sourceの時系列相関とbottleneck候補を検証する。
- 上記runから個人情報や生logを除いた回帰fixtureを作り、`isupipe-practice-bottleneck.json`を複数sourceの相関回帰へ拡張する。

## collectorとbottleneck推測

- ALPへ正規化規則を解析前に渡せるversionでは、正規化後routeの厳密なp95を生成する。現在のadapterは分割済みrouteを統合するときrequest数を合計し、p95は保守的に最大値を採用する。
- HTTP route、DB digest、perf symbol、host saturationをnodeと時間帯で関連付け、複数sourceの裏付けをevidenceへ表示する。
- 観測coverageと裏付け強度を含む、説明可能なカテゴリ横断の優先度を検証する。検証できるまでは表示番号を改善優先順位として扱わない。

## 時系列データの活用CLI

- 取得済みseriesを利用者が迷わず発見できるmetric/label一覧を実runで検討する。標準collectorのstatus・exit code・errorは表のcoverageへ表示済み。
- route・device詳細、期間・node・label filter、異なるbucket幅への再集約をどのCLIとして公開するか決める。
- SQLiteを正本として使う前提で、保持期間とデータ量上限が必要か実runの保存量から判断する。
