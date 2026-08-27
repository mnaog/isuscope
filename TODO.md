# TODO

実環境または時系列相関が必要で、このリポジトリ内の自動テストだけでは完了できない項目です。

## 標準collectorの実環境検証

- sysstat 12系とMySQL 8.0の代表fixtureは追加済み。実環境で完全な出力を再採取し、サポート対象のsysstat、perf、alp、slp versionを確定する。
- Ubuntu上の複数versionでfixtureを再生成し、field、時間単位、locale差分が代表fixtureと一致するか検証する。
- Nginx/MySQLのlog rotation中もrun区間差分を失わない処理を追加する。現在はinode変更を`unavailable`として扱う。
- 空のrunと、recordはあるが未対応schemaのrunを実ツールで検証する。
- ALPへ正規化規則を解析前に渡せるversionでは、正規化後routeの厳密なp95を生成する。現在のadapterは分割済みrouteを統合するときrequest数を合計し、p95は保守的に最大値を採用する。

## bottleneck推測

- HTTP route、DB digest、perf symbol、host saturationをnodeと時間帯で関連付け、複数sourceの裏付けをevidenceへ表示する。
- tooling fingerprintが同じrun同士で候補の増減を比較し、改善後に次の制約へ移ったことを表示する。
- 観測coverageと裏付け強度を含む、説明可能なカテゴリ横断の優先度を検証する。検証できるまでは表示番号を改善優先順位として扱わない。
- 実際のISUCON練習runをfixture化し、上位候補が人間の調査開始点として妥当か回帰確認する。

## 時系列データの活用CLI

- 取得済みseriesを利用者が迷わず発見できるmetric/label一覧を実runで検討する。標準collectorのstatus・exit code・errorは表のcoverageへ表示済み。
- route・device詳細、期間・node・label filter、異なるbucket幅への再集約をどのCLIとして公開するか決める。
- 複数run比較、Grafana/Prometheus等へのexport、保持期間とデータ量上限は、ISUCON当日の調査速度を基準に必要性を検証する。
