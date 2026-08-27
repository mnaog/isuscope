# TODO

実環境または時系列相関が必要で、このリポジトリ内の自動テストだけでは完了できない項目です。

## 標準collectorの実環境検証

- Dockerでは再現できないhost kernel上のperfについて、ISUCONと同系統のkernelで権限・hardware counter・完全な出力を採取する。
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
- `.1`へのrename rotationについて、開始offset以降と新logが欠損なく連結されることをUbuntu containerで検証した。
- alp 1.0.21 Linux arm64の表形式JSONとslp 0.2.1 Linux arm64の4列TSVを実行してfixture化し、標準commandとparserをそのCLI契約へ固定した。
- alp/slpの空入力と非空の解析不能入力を区別し、前者は空metric、後者はcollector failureにするwrapperを追加した。
- log差分をoffsetと先頭SHA-256で検証し、rename、gzip済みrotation、2世代rotation、copytruncateをlosslessに連結する回帰テストを追加した。保持する5世代を越えた場合や中間世代が欠けた場合は`unavailable`にする。
