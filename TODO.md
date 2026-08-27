# TODO

実環境または時系列相関が必要で、このリポジトリ内の自動テストだけでは完了できない項目です。

## 標準collectorの実環境検証

- Dockerでは再現できないhost kernel上のperf、実環境のalp/slpについて、完全な出力を採取してサポートversionを確定する。sysstat 12.2.0、12.5.2、12.6.1とMySQL 8.0.46はfixture化済み。
- Nginx/MySQL logの`.1`へのrename rotationは旧inodeの残りと新fileを連結できる。計測中にgzipまで完了するrotation、複数回rotation、旧inodeを保持しないcopytruncateをlosslessに扱うか、明示的に非対応とするかを実環境で決める。
- 空のrunと、recordはあるが未対応schemaのrunを実ツールで検証する。
- ALPへ正規化規則を解析前に渡せるversionでは、正規化後routeの厳密なp95を生成する。現在のadapterは分割済みrouteを統合するときrequest数を合計し、p95は保守的に最大値を採用する。

## bottleneck推測

- HTTP route、DB digest、perf symbol、host saturationをnodeと時間帯で関連付け、複数sourceの裏付けをevidenceへ表示する。
- tooling fingerprintが同じrun同士で候補の増減を比較し、改善後に次の制約へ移ったことを表示する。
- 観測coverageと裏付け強度を含む、説明可能なカテゴリ横断の優先度を検証する。検証できるまでは表示番号を改善優先順位として扱わない。
- database/perfも同時に観測できた実ISUCON runが得られたら、`isupipe-practice-bottleneck.json`を複数sourceの相関回帰へ拡張する。

## 時系列データの活用CLI

- 取得済みseriesを利用者が迷わず発見できるmetric/label一覧を実runで検討する。標準collectorのstatus・exit code・errorは表のcoverageへ表示済み。
- route・device詳細、期間・node・label filter、異なるbucket幅への再集約をどのCLIとして公開するか決める。
- 複数run比較、Grafana/Prometheus等へのexport、保持期間とデータ量上限は、ISUCON当日の調査速度を基準に必要性を検証する。

## 完了済みの実環境・PC検証

- 2026-08-26のISUCON13練習run `d7555a6b`から、個人情報や生logを含まないbottleneck回帰fixtureを作成した。
- Ubuntu 20.04、22.04、24.04の公式Docker imageでsysstatのfield・単位・`LC_ALL=C`出力を採取し、parser回帰テストへ追加した。
- 公式MySQL 8.0.46 imageで制御したquery列の完全なslow logを採取し、5秒bucketのcallsとtotal durationを回帰テストへ追加した。
- `.1`へのrename rotationについて、旧inodeのoffset以降と新logが欠損なく連結されることをUbuntu containerで検証した。
