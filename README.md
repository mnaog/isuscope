# isuscope

isuscopeは、ISUCONのベンチマークと観測結果を1つのrunとして保存し、変更前後を再現可能に比較するためのローカルCLIです。

スコアだけでなく、仮説、Gitの状態、設定、HTTP・SQL・CPU・host metric、collectorの成否、Codexの会話位置を同じrunへ紐付けます。通常は操作端末から各nodeへSSHし、競技サーバーへ専用agentを常駐させません。

## 基本の流れ

開始直後の全体調査では、通常の観測に匿名viewer単位の行動遷移を加える`survey-run`を1回だけ使います。

```console
isuscope doctor
isuscope survey-run --hypothesis "初期状態の負荷構造を記録する"
isuscope report latest
isuscope analyze RUN_ID supported --analysis "初期状態を記録できた"
```

環境を受け入れた後は、仮説付きの`run`を改善ごとに繰り返します。

```console
isuscope run --hypothesis "postsの複合indexで一覧のDB時間を減らす"
isuscope report latest
isuscope diff BASE_RUN latest
isuscope analyze RUN_ID supported --analysis "p95とDB時間が低下し、スコアも改善した"
```

run IDは実行結果か`isuscope list`で確認します。`analyze`は更新対象を曖昧にしないため、run ID、一意な短縮ID、または一意なtagを明示します。判定は`supported`、`rejected`、`inconclusive`、`skipped`です。

終了前は観測用設定や重いログを環境から外したうえで、同じ`run`を使って採点用構成を確認します。専用の最終計測コマンドはありません。

## インストール

Rust 1.88以降でビルドします。

```console
cargo install --path . --locked
isuscope --version
```

競技用bundleは`./scripts/build-release-bundle.sh`で作れます。生成物にはbinary、SHA-256、README、当日ランブック、LICENSEが含まれます。

## プロジェクトへ導入する

アプリケーションのGitリポジトリ直下で一度だけ初期化します。

```console
isuscope init
```

`.isuscope/`に次の雛形が作られます。既存ファイルは上書きされません。

```text
.isuscope/
├── benchmark.sh        # 当日のベンチ起動・完了待ち・結果変換
├── parse-benchmark.sh  # 問題固有のbenchmark metric変換
├── config.toml         # node、SSH、collector、保存先
├── fingerprint.sh      # remote実体の識別
├── routes.toml         # 動的URLの正規化
├── setup.sh            # remote設定の冪等な適用
└── SETUP.md            # 導入先で読む短いチェックリスト
```

最初に`.isuscope/SETUP.md`を読み、少なくともベンチ起動方法、SSHとnode、アクセスログとslow log、route正規化、app binaryと主要serviceを実環境へ合わせます。その後、shellの構文、setup、doctorを順に確認します。

```console
bash -n .isuscope/benchmark.sh
bash -n .isuscope/parse-benchmark.sh
bash -n .isuscope/setup.sh
.isuscope/setup.sh
isuscope doctor
```

当日の詳しい接続手順と受け入れ基準は[`docs/contest-day.md`](docs/contest-day.md)を参照してください。

## コマンド

| コマンド | 用途 |
|---|---|
| `init` | `.isuscope/`の雛形を生成する |
| `doctor` | ベンチを起動せず、設定・command・SSH・時刻・diskを検査する |
| `survey-run` | 序盤の全体調査を1回行い、行動遷移も収集する |
| `run` | 標準collectorでベンチを実行する |
| `list` | 保存済みrunを新しい順にJSONで一覧表示する |
| `report` | 1 runのcompactな診断JSONを出力する |
| `diff` | 2 runを全件比較してからcompactな差分JSONを出力する |
| `metrics` | metric名、時刻範囲、label cardinalityをJSONで調べる |
| `series` | 時刻付きmetricをbucket化したJSONで調べる |
| `analyze` | PASSしたrunへ仮説の判定と分析を記録する |
| `enrich` | 保存済みbenchmark logへ現在のparserを再適用する |
| `ui` | 人間向けHTML UIをlocalhostで起動する |

`list`、`report`、`diff`、`metrics`、`series`は機械処理しやすいJSONを返します。人が横断的に見る場合は`isuscope ui`を使います。詳しい引数は`isuscope COMMAND --help`で確認できます。

## 保存されるデータ

既定では`.isuscope/data/`を使います。保存先は`config.toml`の`data_dir`で変更できます。

```text
.isuscope/data/
├── isuscope.sqlite3
├── runs/<run-id>/
│   ├── run.json
│   ├── source/
│   ├── tooling/
│   ├── logs/
│   └── structured.json.zst
└── .incomplete/
```

各runにはスコアと成否、仮説と分析、Git commit・dirty patch・未追跡file hash、実行時のisuscope設定、collector出力と構造化metricを保存します。SQLiteは検索用の索引で、run directoryが記録の正本です。索引を失っても`isuscope list`の起動時に再構築されます。

`[context.codex]`を設定すると、run開始時のCodex sessionと最後のUser inputを厳密に紐付けられます。別sessionや通常ターミナルへ推測でfallbackせず、解決できない場合はベンチ開始前に停止します。

## 観測の考え方

標準雛形はhost sampler、sysstat、perf、Flame Graph、off-CPU、ALP、slow query、fingerprintをnodeとphase単位で記録します。依存toolや権限がないcollector、または安全に追えないログrotationは、壊れた値を成功扱いせず`unavailable`として残します。

ALPはrouteごとのcount、status、sum/avg、min/max、p50/p95/p99を保存します。設計と検証の詳細は次を参照してください。

- [標準observability](docs/standard-observability.md)
- [Report / Diff architecture](docs/report-architecture.md)
- [ISUCON13 profile collector受け入れ結果](docs/profile-acceptance-isucon13.md)
- [検証履歴](docs/validation-history.md)

## License

[MIT](LICENSE)
