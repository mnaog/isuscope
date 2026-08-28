# isuscope

`isuscope`は、ISUCONのベンチマーク1回分を再現可能なrunとして記録するツールです。ベンチ結果、ソースコードの状態、構造化メトリクス、圧縮した生ログへの参照を、CLIを実行した手元のマシンへ保存します。

日常の計測は「仮説を書く → ベンチを回す → PASSなら結果を分析する」を1単位にします。

```console
isuscope run --hypothesis "変更理由と改善を期待する観測値"
isuscope analyze latest --verdict supported --analysis "観測結果と判断"
isuscope discovery-run --hypothesis "ボトルネックがDBからHTTPへ移ったはず"
isuscope show [latest|RUN_ID]
isuscope ui
isuscope report [latest|RUN_ID]
isuscope metrics [latest|RUN_ID]
isuscope series [latest|RUN_ID]
```

開始直後に一度だけ使う`isuscope init`もあります。日常操作には使用しません。

## 現在の状態

このリポジトリには、実際に利用できる最初のMVPが入っています。コマンドによるベンチ起動、外部ベンチの対話操作、local/SSH collector、SQLite索引、Git snapshot、ログ圧縮、失敗runの保存を実装済みです。

大会ポータル固有のベンチ開始・終了の自動検知は、実際のポータル仕様が分かってから追加します。

実環境のtool versionや時系列相関が必要な未完了項目は[`TODO.md`](TODO.md)で追跡します。

## インストール

ソースからのbuild・installにはRust 1.88以降が必要です。大会当日は事前に作成した
release bundleのbinaryを利用するため、競技環境へRust toolchainを導入する必要はありません。

```console
cargo install --path .
```

大会前に、network不要で同じマシンへ再導入できるrelease bundleを作成します。

```console
./scripts/build-release-bundle.sh
```

`dist/`へhost triple付きtarballとSHA-256が生成されます。依存crateを更新しない通常build・installには`--locked`を指定します。

競技開始後の導入・受け入れ確認・通常運用は、[ISUCON当日の導入・運用手順](docs/contest-day.md)にまとめています。

推奨する開始方法は、標準collectorを含む非対話型scaffoldの生成です。既存ファイルは上書きしないため再実行できます。

```console
isuscope init
```

### 初回runまでの8ステップ

1. `isuscope init`でscaffoldを生成する。この時点ではまだ`setup.sh`を実行しない。
2. Codex会話履歴を紐付ける場合は、hookを導入・信頼して新しいCodexセッションを開始し、`.isuscope/config.toml`の`[context.codex]`を有効化する。
3. `.isuscope/benchmark.sh`へベンチの起動、完了待ち、結果JSON出力を実装する。
4. `.isuscope/config.toml`の`[ssh]`と`[[nodes]]`を実環境へ合わせる。
5. alpのaccess log path・format、slpのslow log path・format、perfの`sudo -n`権限を確認する。
6. `bash -n .isuscope/benchmark.sh`、`bash -n .isuscope/setup.sh`、`isuscope show`で副作用なしの検査をする。
7. 不足する環境変更だけを`setup.sh`へ記述し、`.isuscope/setup.sh`を実行する。
8. `isuscope discovery-run --hypothesis "初期状態の負荷構造を記録する"`、`isuscope report latest`の順に実行し、必要な時間帯だけ`isuscope series latest`で掘り下げる。

最低限編集するのは`benchmark.sh`、`config.toml`の`[ssh]`、`[[nodes]]`です。標準ツールの自動installやsudoers変更は行いません。必要な場合だけ、利用者が`setup.sh`の`apply_environment`へ冪等な処理を追加します。

問題固有collectorや複数node設定の詳細な参考例は[`examples/config.toml`](examples/config.toml)にあります。

```text
your-isucon-project/
└── .isuscope/
    └── config.toml
```

`isuscope`は、現在のディレクトリから親ディレクトリへ向かって`.isuscope/config.toml`を探索します。

## コマンド

標準collectorを有効にした通常のスコア計測を行います。perfを含めて毎回同じ観測条件にするため、collectorを変更したrun同士は同条件として比較しません。

```console
isuscope run --hypothesis "allocation削減によりviewer完走数が増える"
```

標準collectorは通常runと同じです。加えて、cookieなどの匿名識別子を利用したユーザー行動遷移を集計します。

```console
isuscope discovery-run --hypothesis "routing変更により支配的な待ち時間が移動した"
```

collectorとbenchmark parserを起動せず、スコア取得だけを行います。source、tooling、
benchmark結果とstdout/stderrは保存されます。

```console
isuscope score-run --hypothesis "観測負荷を外すと最終スコアが上がる" --tag final --note "観測なしの最終確認"
```

`--hypothesis`は全ベンチで必須です。ベンチ開始前に`run.json`とSQLiteへ保存され、後から変更できません。PASSしたrunは`analysis_status=pending`となり、結果分析を記録するまで次のベンチを開始できません。FAILまたは中断したrunは分析不要として確定するため、失敗原因を直した次のベンチを妨げません。

### Codex会話コンテキスト

Codexの`UserPromptSubmit` hookが`docs/codex-history`へ保存する会話とrunを紐付ける場合は、初回runより前に次を有効化します。

```toml
[context.codex]
history_dir = "docs/codex-history"
```

この設定はopt-inですが、有効化後は必須条件です。`CODEX_SESSION_ID`または`CODEX_THREAD_ID`と一致するMarkdownの`- Session:` headerを探し、最後の`<!-- codex-event:<session>:<turn>:user -->`に含まれるturn IDをinput IDとして採用します。通常ターミナル、別セッション、hookを導入する前から継続しているセッション、marker欠損では推測によるfallbackを行わず、benchmark開始前にrunを拒否します。

解決した元path、session ID、input ID、SHA-256は`run.json`とSQLiteの`run_codex_context`へ保存します。会話本文もrun時点の内容を`context/codex-history.md`へsnapshotするため、元ファイルが未追跡・後から追記・移動されても当時の文脈が残ります。hookの`session_id`と`turn_id`はCodex公式のevent fieldです。[OpenAI Docs: Hooks](https://learn.chatgpt.com/docs/hooks)

分析では、仮説が支持されたか、棄却されたか、1回の結果では判断不能かを明示します。再評価した場合も上書きせず、revisionとして追記します。

```console
isuscope analyze latest --verdict supported --analysis "scoreが8%増え、エラー数は不変。想定したCPU時間も低下した"
isuscope analyze latest --verdict rejected --analysis-file analysis.md
```

時間切れなどで分析しない場合だけ、理由付きでskipできます。skipも履歴に残り、次のベンチを許可します。

```console
isuscope analyze latest --skip --reason "競技終了前の最終計測のため"
```

補助的な説明と検索用tagも記録できます。noteは`runs.note`、tagは`run_tags`へ保存されるため、SQLiteから直接検索できます。

```console
isuscope run --hypothesis "admission 64で待機を抑えつつviewer完走数が増える" --tag admission-64 --note "admission 63→64"
isuscope annotate latest --tag baseline --remove-tag admission-64
isuscope show baseline
```

ベンチを起動せず、設定、local command、tooling script、SSH疎通、node間時刻差、
data directoryとdisk空き容量を検査します。

```console
isuscope doctor
```

最近のrun一覧、または指定したrunの詳細を表示します。

```console
isuscope show
isuscope show latest
isuscope show 8c9f021a
```

`show`が表示する短縮IDはUUIDの末尾8文字です。そのまま`show`の引数として利用できます。

run詳細には、SQLiteへ保存されたcollectorごとの`complete` / `unavailable` / `failed`、metric名ごとの行数・最小値・最大値・単位も表示します。ユーザーはSQLを直接書かなくても、どの観測が取れていて、どのmetricが存在するかを確認できます。SQLiteは検索・比較用の索引であり、collectorの生出力はrun配下の圧縮ログが正本です。

各log IDの直下には、圧縮ログを読むための`zstd -dc -- '<path>'`も表示します。collectorが`failed`の場合はstderr logの`view`行をコピーして実行します。

SQLiteはrunごとに独立しておらず、1つの`data_dir`につき1ファイルを全runで共有します。各tableの`run_id`でrunを区別するため、commit、score、同じmetric・labelなどをJOINしてrun間比較できます。run配下の`run.json`と圧縮ログはrunごとに独立した正本で、SQLiteはそれらを横断する索引です。

`report`は指定runのmanifest、カテゴリ別coverageと欠測metric、HTTP、database、CPU、host、profile artifact、transition、run正本logの場所をcompactな単一JSONへ出力します。各sectionは上位20件、全件数、打ち切り有無を返します。HTTP表にはcount、total/avg/min/p50/p95/p99/max、error数・率、status class別count、取得できる場合はresponse bytesを保持し、total時間順に並べます。DBはtotal時間、CPUはsample比率、hostはaverage/peakと発生時刻を保持します。異なる指標を一つの推測scoreへ潰しません。

全summary metricと時刻付きmetricが必要な場合だけ`--full`を付けます。

```console
isuscope report latest --full
```

同じReport modelから、server不要の人間向けHTMLも生成できます。

```console
isuscope report latest --format html --output .isuscope/latest/report.html
```

最新runをブラウザで確認する場合は、オプションなしでlocalhost専用UIを起動します。`http://127.0.0.1:3000`を開き、終了するときはCtrl-Cを押します。外部interfaceへはbindしません。

```console
isuscope ui
```

SQLite、Report生成、CLI JSON、静的HTML、将来のlocalhost UIの責務分担は[ReportとUIのアーキテクチャ](docs/report-architecture.md)にまとめています。

`show`の末尾にはSQLiteファイルの絶対パスと、そのままコピーして実行できる`sqlite3`のquery例を表示します。一覧ではrun履歴、run詳細ではそのrunのmetric全行を確認するqueryになり、さらに同じmetricをrun横断で並べる比較queryも表示します。独自の比較やlabel単位の調査が必要な場合だけquery例を入口にSQLiteを直接参照します。

## Benchmark parserと後処理

問題固有の件数がbenchmark stdoutへ出る場合は、`[[benchmark.parsers]]`を追加します。
parserはベンチ終了後にだけ動くため、採点中の負荷にはなりません。

```toml
[[benchmark.parsers]]
name = "contest-output"
command = [".isuscope/parse-benchmark.sh", "{benchmark_stdout}"]
timeout_seconds = 30
```

parserは`{run_id}`、`{run_dir}`、`{benchmark_stdout}`、`{benchmark_stderr}`を
command引数で利用できます。stdoutへ1行1 JSON objectでmetricを出します。
`isuscope.parser` labelはisuscopeが自動追加します。

```json
{"type":"metric","name":"benchmark.scenario.success","value":5783,"unit":"runs","labels":{"scenario":"viewer"}}
```

benchmark adapter自身がこのmetric JSONLをstdoutへ出せる場合は、外部parserなしでも直接保存されます。

最初のベンチ後にparserを実装・修正した場合は、保存済みの圧縮logへ再適用できます。
以前の外部parserが生成したmetricとlogを現在のparser構成で置換し、inline metricとcollector由来の値は変更しません。

```console
isuscope enrich latest
```

再適用時のconfigと`[tooling].include`も対象runの`tooling/enrichments/<id>/`へ保存されます。

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
└── data/
    ├── isuscope.sqlite3
    ├── runs/
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
            │   ├── extra/parse-benchmark.sh
            │   ├── enrichments/
            │   └── isuscope-version.txt
            ├── context/
            │   └── codex-history.md
            ├── structured.json.zst
            └── logs/
                ├── benchmark-stdout.zst
                ├── benchmark-stderr.zst
                └── <collector-log-id>.zst
    └── latest/
        ├── run.json
        ├── logs.json
        └── logs/<log-id>.log
```

SQLiteにはrunのメタデータ、仮説、分析状態、追記式の分析履歴、Codex context参照、note/tag、スコア、数値メトリクス、行動遷移の集計、log IDを保存します。生ログとCodex会話の本文はSQLiteへ入れません。`structured.json.zst`にはSQLiteへ入れたmetric、fingerprint、transitionの正規化済み行を圧縮保存します。完成済みrunは従来どおり圧縮した正本を保持し、直近runだけは`latest/logs/`へ自動展開するため、`zstd`なしですぐ読めます。次のrunが完成すると内容は置き換わります。

`tooling/`には実際に使った設定、route規則、setup状態、isuscope versionをrunごとに保存します。各ファイルのSHA-256も`run.json`へ入るため、計測構成の違いを後から判別できます。

SQLiteファイルを失った場合でも、`run.json`と`structured.json.zst`からrun索引と構造化データを完全に再構築できます。collectorの生ログも根拠データとして残ります。

### SQLite索引の復旧

`runs/`以下の完成済みrunが正本で、SQLiteは検索・比較用の索引です。`isuscope`は起動時にSQLiteへ未登録のrunを検出すると、`run.json`と`structured.json.zst`から自動的に索引を復元します。旧runにsnapshotがなければJSON protocol logの再解析へfallbackしますが、native adapterの結果まで完全に戻る保証はありません。公開CLIに復旧専用コマンドはありません。

SQLiteが壊れた場合は、isuscopeが動いていないことを確認してから`isuscope.sqlite3`、`isuscope.sqlite3-wal`、`isuscope.sqlite3-shm`を別ディレクトリへ退避し、プロジェクト内で次を実行します。

```console
isuscope show
```

復元したrunは標準エラーへ`reindexed`と表示されます。`structured.json.zst`が欠損・破損した旧runでは復元が不完全になり得ます。退避したSQLiteファイルは、内容を確認するまで削除しないでください。

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

roleはマシンの固定的な種類ではなく、collectorの実行先を選択する任意のtagです。1台へ`roles = ["edge", "app", "db"]`のように複数指定でき、役割が移動したら次のrunの前に変更します。設定はrunごとにsnapshotされるため、過去runが参照した役割は保持されます。

コマンド引数では、次のplaceholderを利用できます。

- `{run_id}`
- `{run_dir}`
- `{node}`
- `{host}`

collectorの失敗は記録しますが、ベンチは停止しません。例外として、`before` phaseで`required = true`を指定したcollectorが失敗した場合は、ベンチを開始しません。

標準観測collectorのように「毎回試すが、そのrunでは対象が存在しない」ものは、終了コード`75`で終了してください。isuscopeはこれを失敗ではなく`unavailable`として記録します。たとえばMySQLを退役してPostgreSQLへ移行した後は、MySQL slow log collectorが`75`を返せばrunをdegradedにせず、観測対象が消えた事実を履歴へ残せます。終了コードはcollectorごとの`unavailable_exit_codes = [75]`で変更できます。

perf、alp、slp、sysstatを常設する場合の役割分担、collector構成、ツールやDBが消えた場合の扱い、および共通metric契約は[標準観測スタックの設計](docs/standard-observability.md)にまとめています。

`isuscope init`が生成するconfigにはsysstat、perf、alp、slpの標準collectorが最初から含まれます。roleを空にして全nodeを対象にし、両方のrun modeで同じ観測を試みます。ツール、権限、対象ログがなければ終了コード75で`unavailable`になります。alpとslpのログpath・formatだけは問題環境に合わせてください。

`max_output_bytes`はstdoutとstderrへ個別に適用します。上限に達した後もpipeの読み取り自体は続けるため、観測対象のプロセスをblockしません。runはdegradedとなり、保存ログには途中で打ち切られたことが記録されます。

### 構造化collector出力

collectorの出力はすべて圧縮ログとして保存します。加えて、1行に1つのJSON objectをstdoutへ出力すると、計算済みの値をSQLiteにも記録できます。

標準ツールのnative出力には`parser = "sysstat"`、`parser = "alp-json"`、`parser = "slp-tsv"`を指定できます。`alp-json`はalp 1.0.21のheader付き表形式JSON、`slp-tsv`はslp 0.2.1のheaderなし4列TSVを受け取ります。旧設定向けの`slp-json`も読み込み互換性のため残しています。adapterは生ログを残したまま共通metricへ変換し、変換できない出力をcollectorの成功として扱いません。

```json
{"type":"metric","name":"process.cpu_percent","value":34.2,"unit":"percent","labels":{"role":"app"}}
{"type":"metric","name":"host.cpu_percent","value":82.1,"unit":"percent","timestamp":"2026-08-27T12:34:56.789Z","labels":{"node":"isu1"}}
{"type":"fingerprint","name":"app.binary.sha256","value":"012345..."}
{"type":"transition","from":"GET /api/livestream/:id","to":"GET /api/livecomments","count":4875,"p50_ms":3.0,"p95_ms":8.0}
```

この形式以外の行は生ログとして残し、構造化parserでは無視します。

metricへ任意の`timestamp`を付けると、RFC 3339文字列またはUnix秒（小数可）を観測時刻としてSQLiteの`metrics.observed_at`へ保存します。1秒ごとなどに同じ`name`とlabelで出力すれば、ベンチ中のCPU、memory、queue長、request数などを時系列として保持できます。`timestamp`を省略したALP集計値などは従来どおりrun全体の集計値として保存されます。

時刻付きmetricはCLIから5秒bucketの表として確認できます。画面や常駐serverは不要です。

```console
isuscope metrics latest
isuscope series latest
isuscope series latest --metric http.requests --node app1 --label route=/api/livestream/:id --bucket 10
isuscope series latest --metric db.query.total_duration --from 20 --to 60
isuscope series latest --metric cpu.sample_count --limit 2000
```

`metrics`はmetric名、行数、時刻付き行数、単位、時刻範囲、labelの種類・cardinality・例を一覧にします。最初にこれを実行すると、SQLiteのschemaを知らなくても指定可能なfilterを確認できます。

引数なしの`series`はUTCの5秒境界で揃えたmetricをbenchmark区間で切り出し、開始からの相対秒とともにnodeごとのhost、HTTP、database指標を表形式で並べます。`--metric`を1回以上指定すると任意metricの汎用表になり、`--node`、複数の`--label key=value`、`--from`、`--to`、`--bucket`で絞り込み・再集約できます。汎用表は高cardinalityなperf metricでも端末を埋めないよう既定1000行で打ち切り、`--limit`で変更できます。count、duration合計、bytes、sample countは合計し、gaugeは平均します。p95などのquantileは元sampleなしには再計算できないため、明示的に`max-of-quantile`として表示します。欠測は`-`です。

表の前には標準時系列collectorごとの`complete` / `unavailable` / `failed`、exit code、errorを表示します。各metricには生成したcollector名が記録され、CPUは`host-sampler`を優先し、利用できない場合だけsysstatへfallbackするため二重集計しません。HTTP、MySQL、sysstat parserはbenchmark区間外のsampleを除外します。

`isuscope init`の標準構成では、追加package不要の`host-sampler`が`/proc`からCPU、memory、load averageを1秒間隔で取得します。利用可能ならsysstatも各CPU/disk sampleを時系列化します。discovery用access logとMySQL slow logは5秒bucketのHTTP/DB metricへ変換できます。

保存データは自動削除しません。実際のISUCON13練習データ18 runではSQLite約64 MiB、run配下約133 MiB、合計約197 MiBでした。古いrunを機械的に消すより、`doctor`のdisk空き容量確認を使い、必要になった時だけ正本のrunディレクトリとSQLite索引を一緒に保全・整理します。

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

### discovery-runのHTTP body capture

最初の`discovery-run`で通常HTTPの入力値と出力値を後から調査する場合は、内蔵capture proxyを`during` collectorとして起動できます。benchmarkの接続先をproxy（次の例では`127.0.0.1:18080`）へ向け、proxyから実際のHTTP applicationへ転送します。proxyはrequestとresponseを対応付けたJSON Linesをstdoutへ出し、collectorがrunの正本logとしてzstd圧縮保存します。SSE、WebSocket、HTTPS upstreamには対応しません。

```toml
[[collectors]]
name = "discovery-http"
phase = "during"
transport = "local"
modes = ["discovery-run"]
command = ["isuscope", "__discovery-capture", "--listen", "127.0.0.1:18080", "--upstream", "http://127.0.0.1:8080", "--max-body-bytes", "1048576", "--session-cookie", "session"]
```

起動前に`ISUSCOPE_DISCOVERY_SESSION_KEY`へrun内だけで使う秘密値を設定します。各eventには時刻、request ID、method、path、query、status、所要時間、request/responseのcontent type・byte数・SHA-256・bodyを含めます。JSONはJSON valueのまま、form/textは文字列として保持します。設定したsession Cookieは生値を保存せず、HMAC-SHA-256へ変換します。1 MiBを超えるbodyとJSON/form/text以外はbody本体を省略しますが、byte数、hash、content type、省略理由は残します。captureは観測用proxyでありHTTP responseをbufferするため、通常runや最終scoreの計測には使いません。

ベンチ出力は常に`benchmark-stdout.zst`と`benchmark-stderr.zst`へ保存します。大量出力による端末のノイズと計測への影響を避けるため、既定では端末へ転送しません。実行中にも全行を見たい場合は`[benchmark]`へ`stream_output = true`を追加します。

## 外部ベンチモード

大会ポータルからベンチを起動する場合は、設定を次のように変更します。

```toml
[benchmark]
mode = "external"
```

`--hypothesis`付きで`isuscope run`または`isuscope discovery-run`を実行するとcollectorを待機状態にし、ポータルからベンチを起動するよう案内します。ベンチ終了後、スコアとpass/failを対話形式で入力します。

ポータル固有の自動結果取得は、run形式や公開CLIを変更せず後から追加できます。

## SQLiteの照会

比較専用コマンドを追加しなくても、SQLiteを直接照会できます。

```sql
SELECT started_at, commit_hash, score, hypothesis, analysis_status
FROM runs
WHERE passed = 1
ORDER BY started_at;
```

metricは`run_id`を持つため、同名metricをrun横断で比較できます。

```sql
SELECT substr(m.run_id, -8) AS run,
       r.score,
       m.name,
       m.value,
       m.unit,
       m.labels_json
FROM metrics AS m
JOIN runs AS r ON r.id = m.run_id
WHERE m.name = 'http.request_duration'
ORDER BY m.labels_json, r.started_at;
```

```sql
SELECT r.started_at, r.score, r.note, t.tag
FROM runs AS r
LEFT JOIN run_tags AS t ON t.run_id = r.id
ORDER BY r.started_at;
```

仮説と分析履歴を時系列で読む例です。

```sql
SELECT r.started_at, r.score, r.hypothesis,
       a.created_at AS analyzed_at, a.verdict, a.body
FROM runs AS r
LEFT JOIN run_analyses AS a ON a.run_id = r.id
ORDER BY r.started_at, a.created_at;
```

秘密情報の自動maskingは行いません。短時間かつ管理されたISUCON環境で使用するという要件に基づく仕様です。

## ライセンス

[MIT License](LICENSE)で公開しています。

isuscopeはISUCON運営による公式ツールではありません。
