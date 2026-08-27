# TODO

未完了の項目だけを記載します。

## ISUCON相当のLinux実環境で検証すること

- database、perf、HTTP、host resourceを同時に取得した実ISUCON runを作り、node・時間帯を揃えた`direct` / `corroborated`判定とbottleneck候補を検証する。MySQLを退役したISUCON13最適化環境ではDB相関を検証できないため、初期実装または別問題を使う。
- 上記runから個人情報や生logを除いた回帰fixtureを作り、`isupipe-practice-bottleneck.json`を複数sourceの相関回帰へ拡張する。
