# isuscope

isuscopeは、ISUCONのベンチマークと観測結果を1つのrunとして保存し、変更前後を再現可能に比較するためのローカルCLIです。

スコアだけでなく、仮説、Gitの状態、設定、HTTP・SQL・CPU・host metric、collectorの成否、Codex・Claude Codeの会話位置を同じrunへ紐付けます。通常は操作端末から各nodeへSSHし、競技サーバーへ専用agentを常駐させません。

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

### 仮説の判定と変更の採否

仮説判定、スコア差、変更を残す判断は独立しています。スコアが下がっても局所改善や単純化を理由に採用できます。採用したことを理由に過去の仮説判定を書き換える必要はありません。

```bash
isuscope analyze RUN_ID inconclusive --base BASE_RUN \
  --analysis "対象の長時間SQLは減ったが、新規登録が遅くなりスコアは低下"
isuscope change create user-items-update \
  --description "既存user_itemsをUPDATEし、不足行のみINSERT" \
  --target "変更commitまたは変更範囲"
isuscope change decide user-items-update accepted --run RUN_ID \
  --reason "長時間SQL抑制を評価して残す。新規登録の追加SELECTは別途改善"
isuscope brief RUN_ID
isuscope change list --status provisional
isuscope change show user-items-update
```

- `analyze --base`は比較元の完全なrun IDを保存します。brief/report/UIは同じ比較処理で各1走のスコア差を表示し、誤差や性能採否は自動判定しません。
- `change decide`の状態は`accepted`（採用）、`provisional`（暫定採用）、`rejected`（不採用）、`deferred`（保留）です。未記録と保留は別です。
- `provisional`には`--revisit "再評価条件"`が必須です。理由と1件以上の根拠runは全状態で必須です。`--run`は繰り返せます。FAIL runも根拠にできます。
- 変更とrunは多対多です。複数変更を一度に試した場合も、一部だけ採用できます。根拠runのsource（commit、dirty、state digestなど）を採否記録に保存します。
- 同じ変更への再判断は追記され、`show`で全履歴を確認できます。`list --status`は現在の採否で絞り込みます。brief/reportの採否も「関連変更の現在の判断」であり、当時の判断はshowで確認します。
- 採否はdeploy・merge・rollbackを行わず、実環境に反映済みであることも意味しません。採否未記録や暫定採用は次のベンチを阻止せず、従来の分析gateだけを適用します。
- `data_dir/changes/<id>/change.json`と`decisions/*.json`が正本です。この軽量な履歴もGitへ含めてください。SQLiteの変更・採否・根拠run索引はファイルから復元できます。古いrunから採否を自動推定しません。

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
| `brief` | score、異常、benchmark値、主要性能sectionだけの小さいJSONを出力する |
| `diff` | 2 runを全件比較してからcompactな差分JSONを出力する |
| `metrics` | metric名、時刻範囲、label cardinalityをJSONで調べる |
| `series` | 時刻付きmetricをbucket化したJSONで調べる |
| `query` | SQLite上の保存済みmetricを絞り込み、安全な集約JSONで調べる |
| `analyze` | PASSしたrunへ仮説の判定と分析を記録する |
| `enrich` | 保存済みbenchmark logへ現在のparserを再適用する |
| `ui` | 人間向けHTML UIをlocalhostで起動する |

`list`、`brief`、`report`、`diff`、`metrics`、`series`、`query`は機械処理しやすいJSONを返します。まず`brief`で判断材料だけを確認し、上位件数から漏れた対象やrun集約metricは`query`で絞り込みます。database viewはcollector sourceを保ったままSQL digestを集約し、`--group-by sql-shape`で可変長`IN`をまとめられます。詳しい引数は`isuscope COMMAND --help`で確認できます。

```console
isuscope brief latest
isuscope query latest --metric-prefix benchmark.scenario. --group-by scenario
isuscope query latest --base previous --view database --source mysql-log-delta --label-contains digest=reservation_slots --group-by sql-shape
isuscope query latest --base previous --view http --label-contains route=reservation
```

`query --base`は同じselectorを両runへ適用し、全件をfull outer joinしてから`--limit`を適用します。base/candidate/delta/delta percentとadded/removed/bothを返すため、対象を絞った比較で上位項目の入れ替わりを失いません。SQL shapeは可変長`IN`と複数行`VALUES`をまとめ、長いdigest exampleは短縮します。SQLiteとstructured snapshotの値は変更せず、query/briefの表示値だけを単位に応じて丸めます。

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

`[context.agent]`を設定すると、run開始時のCodexまたはClaude Codeのsessionと最後のUser inputを厳密に紐付けられます。sessionは`CODEX_SESSION_ID`／`CODEX_THREAD_ID`と`CLAUDE_CODE_SESSION_ID`から解決し、history fileの`- Agent:` headerと一致するものだけを採用します。旧名の`[context.codex]`、`codex-event`マーカー、既存runの`codex_context`も読み込めます。別sessionや通常ターミナルへ推測でfallbackせず、解決できない場合はベンチ開始前に停止します。
会話履歴はcontextとして別途snapshotされるため、設定した`history_dir`は性能sourceのdirty判定とpatchから自動的に除外されます。

## 観測の考え方

標準雛形はhost sampler、sysstat、指定systemd unitのcgroup sampler、perf、Flame Graph、off-CPU、ALP、slow query、fingerprintをnodeとphase単位で記録します。依存toolや権限がないcollector、または安全に追えないログrotationは、壊れた値を成功扱いせず`unavailable`として残します。時系列は`--window whole|initialize|load`で初期化と負荷走行を分離できます。

ALPはrouteごとのcount、status、sum/avg、min/max、p50/p95/p99を保存します。設計と検証の詳細は次を参照してください。

- [標準observability](docs/standard-observability.md)
- [Report / Diff architecture](docs/report-architecture.md)
- [ISUCON13 profile collector受け入れ結果](docs/profile-acceptance-isucon13.md)
- [検証履歴](docs/validation-history.md)

## License

[MIT](LICENSE)
