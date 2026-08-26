# isuscope

`isuscope`は、ISUCONのベンチマーク1回分を再現可能なrunとして記録するツールです。ベンチ結果、ソースコードの状態、構造化メトリクス、圧縮した生ログへの参照を、CLIを実行した手元のマシンへ保存します。

公開CLIは意図的に次の3コマンドだけにしています。

```console
isuscope run
isuscope discovery-run
isuscope show [latest|RUN_ID]
```

開始直後に一度だけ使う`isuscope init`もあります。日常操作には使用しません。

## 現在の状態

このリポジトリには、実際に利用できる最初のMVPが入っています。コマンドによるベンチ起動、外部ベンチの対話操作、local/SSH collector、SQLite索引、Git snapshot、ログ圧縮、失敗runの保存を実装済みです。

大会ポータル固有のベンチ開始・終了の自動検知は、実際のポータル仕様が分かってから追加します。

## インストール

```console
cargo install --path .
```

大会前に、network不要で同じマシンへ再導入できるrelease bundleを作成します。

```console
./scripts/build-release-bundle.sh
```

`dist/`へhost triple付きtarballとSHA-256が生成されます。依存crateを更新しない通常build・installには`--locked`を指定します。

競技開始後の導入・受け入れ確認・通常運用は、[ISUCON当日の導入・運用手順](docs/contest-day.md)にまとめています。

[`examples/config.toml`](examples/config.toml)をISUCONプロジェクトへコピーします。

```text
your-isucon-project/
└── .isuscope/
    └── config.toml
```

`isuscope`は、現在のディレクトリから親ディレクトリへ向かって`.isuscope/config.toml`を探索します。

または、非対話型scaffoldを生成します。既存ファイルは上書きしないため再実行できます。

```console
isuscope init
.isuscope/setup.sh
```

## コマンド

低負荷のcollectorで通常のスコア計測を行います。

```console
isuscope run
```

アクセスログやユーザー行動遷移など、調査用collectorも有効にして実行します。

```console
isuscope discovery-run
```

最近のrun一覧、または指定したrunの詳細を表示します。

```console
isuscope show
isuscope show latest
isuscope show 8c9f021a
```

`show`が表示する短縮IDはUUIDの末尾8文字です。そのまま`show`の引数として利用できます。

## 当日のベンチ起動アダプター

ベンチの起動方法は大会ごとに異なる前提です。`isuscope init`は`.isuscope/benchmark.sh`を生成し、`config.toml`から直接呼び出します。ISUCON開始後に調査したCLIやportal APIとの接続処理は、この1ファイルだけへ実装します。

scriptはベンチを開始して完了まで待ち、最後に次の形式を1行でstdoutへ出します。詳細な契約と直接実行・API polling双方への注意は、生成されたscript冒頭のコメントにあります。

```json
{"type":"isuscope.result","score":12345,"pass":true,"messages":[]}
```

initializeの開始・終了を取得できる場合は、任意のevent行も出力できます。

```json
{"type":"isuscope.event","name":"initialize-started"}
{"type":"isuscope.event","name":"initialize-finished"}
```

アダプターはrunごとに`tooling/extra/benchmark.sh`へsnapshotされるため、当日コードを何度直しても、どの実装で得たスコアか後から追跡できます。`isuscope init`の再実行は既存のアダプターを上書きしません。

## 保存先

```text
.isuscope/
├── config.toml
├── isuscope.sqlite3
└── runs/
    └── <run-id>/
        ├── run.json
        ├── source/
        │   ├── git.json
        │   └── working-tree.patch
        ├── tooling/
        │   ├── config.toml
        │   ├── routes.toml
        │   ├── setup.sh
        │   ├── setup-state.json
        │   ├── extra/benchmark.sh
        │   └── isuscope-version.txt
        └── logs/
            ├── benchmark-stdout.zst
            └── <collector-log-id>.zst
```

SQLiteにはrunのメタデータ、スコア、数値メトリクス、行動遷移の集計、log IDを保存します。生ログの本文はSQLiteへ入れません。

`tooling/`には実際に使った設定、route規則、setup状態、isuscope versionをrunごとに保存します。各ファイルのSHA-256も`run.json`へ入るため、計測構成の違いを後から判別できます。

SQLiteファイルを失った場合でも、`run.json`とcollectorの生ログからrun索引と構造化データを再構築できます。

### SQLite索引の復旧

`runs/`以下の完成済みrunが正本で、SQLiteは検索・比較用の索引です。`isuscope`は起動時にSQLiteへ未登録のrunを検出すると、`run.json`とcollectorの圧縮ログから自動的に索引を復元します。公開CLIに復旧専用コマンドはありません。

SQLiteが壊れた場合は、isuscopeが動いていないことを確認してから`isuscope.sqlite3`、`isuscope.sqlite3-wal`、`isuscope.sqlite3-shm`を別ディレクトリへ退避し、プロジェクト内で次を実行します。

```console
isuscope show
```

復元したrunは標準エラーへ`reindexed`と表示されます。圧縮ログが欠損・破損している場合、そのログに含まれていた構造化メトリクスまでは復元できません。退避したSQLiteファイルは、内容を確認するまで削除しないでください。

収集中のrunは、最初に`runs/.incomplete/`へ書き込みます。収集とSQLiteへの記録が完了したときだけ最終ディレクトリへ移動します。ベンチやinitializeが失敗した場合も、failed runとして確定・保存します。

signalを受けたrunはbenchmarkのprocess groupを停止し、after collectorを実行して`aborted`として保存します。強制終了で残った`.incomplete` runは次回の`run`開始時に自動確定し、run ID規約に従うremote一時ファイルも削除します。

## Git snapshot

ベンチを開始する前に、次の情報を記録します。

- `HEAD`とbranch
- worktreeがdirtyかどうか
- binary形式のworking tree patch
- 未追跡ファイルのパスとSHA-256
- 記録したソース状態全体を表すSHA-256

Gitリポジトリが見つからない場合は、`.git`、`.isuscope`、`target`を除外したsource treeのdigestを記録します。

Git管理外の大きな計測データがプロジェクト内にある場合は、追加の除外対象を設定できます。

```toml
[source]
repo = "."
exclude = [".pprotein", "tmp"]
```

## Collector

collectorは`.isuscope/config.toml`に記述する外部コマンドです。問題固有の計測ツールを`isuscope`本体へ組み込まずに追加できます。

```toml
[[collectors]]
name = "nginx-access"
phase = "during"             # before、during、afterのいずれか
transport = "ssh"            # localまたはssh
roles = ["edge"]
modes = ["discovery-run"]     # run、discovery-runの一方または両方
command = ["sudo", "tail", "-n", "0", "-F", "/var/log/nginx/access.log"]
timeout_seconds = 90
max_output_bytes = 1073741824
required = false
```

`during` collectorは、ベンチプロセスの開始前に起動し、終了後に停止します。SSH collectorは、指定したroleのいずれかに一致するnodeごとに1回実行します。

SSH collectorが出力したmetricには、対象node名が`node` labelとして自動追加されます。collector自身が`node` labelを出力した場合はその値を維持します。

コマンド引数では、次のplaceholderを利用できます。

- `{run_id}`
- `{run_dir}`
- `{node}`
- `{host}`

collectorの失敗は記録しますが、ベンチは停止しません。例外として、`before` phaseで`required = true`を指定したcollectorが失敗した場合は、ベンチを開始しません。

`max_output_bytes`はstdoutとstderrへ個別に適用します。上限に達した後もpipeの読み取り自体は続けるため、観測対象のプロセスをblockしません。runはdegradedとなり、保存ログには途中で打ち切られたことが記録されます。

### 構造化collector出力

collectorの出力はすべて圧縮ログとして保存します。加えて、1行に1つのJSON objectをstdoutへ出力すると、計算済みの値をSQLiteにも記録できます。

```json
{"type":"metric","name":"process.cpu_percent","value":34.2,"unit":"percent","labels":{"role":"app"}}
{"type":"fingerprint","name":"app.binary.sha256","value":"012345..."}
{"type":"transition","from":"GET /api/livestream/:id","to":"GET /api/livecomments","count":4875,"p50_ms":3.0,"p95_ms":8.0}
```

この形式以外の行は生ログとして残し、構造化parserでは無視します。

`fingerprint`は文字列値をSQLiteへ保存します。app binary、設定、service unit、OSやtool versionなど、スコア取得時のremote実体を記録する用途です。

### ユーザー行動遷移collector

discovery用テンプレートから呼び出す、非公開のcollector helperをバイナリへ内蔵しています。このhelperは次の処理を行います。

1. 複数nodeの圧縮済みNginx LTSVログをRFC 3339、Unix秒、Nginx時刻で統合
2. cookie/sessionごとにrequestを分類
3. 正規表現規則で動的routeを正規化
4. 隣接するAPI間のedge、回数、遷移時間p50/p95を構造化collector形式で出力
5. route/node別のrequest数、status class、response bytes、接続再利用、request/upstream時間p50/p95/p99をmetricとして出力

[`examples/nginx-transition-log.conf`](examples/nginx-transition-log.conf)を参考に、discovery用アクセスログへ`time`、`session`、`method`、`uri`を追加してください。

[`examples/routes-isupipe.toml`](examples/routes-isupipe.toml)は、ISUCON13用のroute正規化規則です。ISUCONプロジェクトの`.isuscope/routes.toml`へコピーします。[`examples/config.toml`](examples/config.toml)には、このhelperを呼び出す`after` collectorも含まれています。

session cookieが発行される前のrequestは、同一ユーザーとして関連付けできないため集計から除外します。ベンチ中のSSH転送を避けるには、開始前にaccess logのbyte offsetを記録し、終了後に差分だけ回収する[`examples/config.toml`](examples/config.toml)の構成を使用します。

ベンチ出力は常に`benchmark-stdout.zst`と`benchmark-stderr.zst`へ保存します。大量出力による端末のノイズと計測への影響を避けるため、既定では端末へ転送しません。実行中にも全行を見たい場合は`[benchmark]`へ`stream_output = true`を追加します。

## 外部ベンチモード

大会ポータルからベンチを起動する場合は、設定を次のように変更します。

```toml
[benchmark]
mode = "external"
```

`isuscope run`または`isuscope discovery-run`を実行するとcollectorを待機状態にし、ポータルからベンチを起動するよう案内します。ベンチ終了後、スコアとpass/failを対話形式で入力します。

ポータル固有の自動結果取得は、run形式や公開CLIを変更せず後から追加できます。

## SQLiteの照会

比較専用コマンドを追加しなくても、SQLiteを直接照会できます。

```sql
SELECT started_at, commit_hash, score
FROM runs
WHERE passed = 1
ORDER BY started_at;
```

秘密情報の自動maskingは行いません。短時間かつ管理されたISUCON環境で使用するという要件に基づく仕様です。

## ライセンス

[MIT License](LICENSE)で公開しています。

isuscopeはISUCON運営による公式ツールではありません。
