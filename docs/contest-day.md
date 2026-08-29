# ISUCON当日の導入・運用手順

この文書は、競技開始後に初めて分かるベンチ起動方法、サーバー構成、ログ形式をisuscopeへ接続し、計測可能な状態にするためのランブックです。

普段使うコマンドは`run`、`analyze`、`enrich`、`list`、`report`、`diff`です。`init`、環境設定、`survey-run`は開始直後の調査で一度だけ使います。

## 最短チェックリスト

- [ ] `isuscope --version`で事前に用意したbinaryを確認する
- [ ] 対象プロジェクトで`isuscope init`を1回実行する
- [ ] Codex会話を紐付ける場合は、事前に信頼済みhookと新しいCodexセッションを用意し、`[context.codex]`を有効化する
- [ ] `.isuscope/benchmark.sh`へ当日の起動・完了待ち・結果取得を実装する
- [ ] 必要なら`.isuscope/parse-benchmark.sh`へ問題固有の出力parserを実装する
- [ ] `.isuscope/config.toml`へSSH、node、role、collectorを設定する
- [ ] `.isuscope/fingerprint.sh`をapp binaryと主要serviceへ合わせる
- [ ] アクセスログfieldと`.isuscope/routes.toml`を確認する
- [ ] `.isuscope/setup.sh`を実行する
- [ ] `isuscope doctor`がfailure 0になることを確認する
- [ ] `isuscope survey-run --hypothesis "初期状態の負荷構造を記録する"`を1回通し、run stateが`complete`になるまで直す
- [ ] PASSしたrunへ、出力されたIDを指定して`isuscope analyze RUN_ID VERDICT --analysis "結果"`で結果分析を記録する
- [ ] `isuscope report latest`で保存内容を確認する
- [ ] 序盤調査を終えたら`survey-run`には戻らず、以後は`isuscope run`を使う
- [ ] 競技終了時にSQLiteだけでなくdata directory全体をbackupする

## 事前に済ませておくこと

- isuscopeのrelease bundleとSHA-256を手元へ保存する
- bundleを展開し、`isuscope --version`が実行できることを確認する
- SSH client、`sqlite3`、`zstd`を操作端末へ用意する
- SSH鍵と競技用credentialを、リポジトリ外の安全な場所へ用意する
- isuscopeの保存先に十分な空き容量があることを確認する
- Codex会話をrunへ残す場合は、`UserPromptSubmit` hookを導入・信頼して新しいセッションで動作確認する

release bundleにはREADME、この文書、LICENSEが含まれます。isuscope自体は操作端末で動き、通常は競技サーバーへRust agentを常駐させません。

## 開始直後に一度だけ行うこと

### 1. プロジェクトを決める

アプリケーションのソースを管理するディレクトリへ移動します。ここがGit snapshotと`.isuscope/config.toml`探索の基準になります。

Gitリポジトリがある場合、isuscopeはcommit、branch、dirty patch、未追跡ファイルのhashを記録します。Gitがなくてもsource tree digestは記録されますが、変更理由を追いやすくするためGit利用を推奨します。

### 2. scaffoldを生成する

```console
isuscope init
```

次のファイルが`.isuscope/`へ生成されます。既存ファイルは再実行しても上書きされません。

```text
.isuscope/
├── benchmark.sh     # 当日に実装するベンチ起動アダプター
├── parse-benchmark.sh # benchmark出力をmetricへ変換
├── config.toml      # node、SSH、collector設定
├── fingerprint.sh   # remote実体の識別
├── routes.toml      # 動的URLの正規化
├── setup.sh         # 必要なremote設定の冪等な適用
└── SETUP.md         # 生成先で読む短いチェックリスト
```

### 3. ベンチ起動アダプターを実装する

最初に`.isuscope/benchmark.sh`冒頭のコメントを読みます。大会固有の起動処理は、このファイルだけへ実装します。Rust本体や日常CLIは変更しません。

アダプターが担当する処理は次の3つです。

1. ベンチを1回だけ開始する
2. 結果が確定するまで待つ
3. 最後に結果を1行JSONでstdoutへ出す

```json
{"type":"isuscope.result","score":12345,"pass":true,"messages":[]}
```

直接bench binaryを実行できる場合は、そのコマンドを呼び出して出力から結果を変換します。portal APIしかない場合は、開始request、完了poll、結果取得を実装します。

結果を取得できた場合は、ベンチ判定がfailでも`exit 0`にして`"pass":false`で伝えます。認証、起動、poll、parseなどアダプター自体の失敗時だけnon-zeroで終了します。

initialize境界を取得できる場合は次のeventも出力できます。

```json
{"type":"isuscope.event","name":"initialize-started"}
{"type":"isuscope.event","name":"initialize-finished"}
```

注意事項:

- 動作確認中にベンチを二重起動しない
- token、cookie、Authorization headerをstdout/stderrへ出さない
- credentialを`config.toml`やGit管理ファイルへ直接書かない
- `bash -n .isuscope/benchmark.sh`など、ベンチを起動しない検査を先に行う
- `ISUSCOPE_PROJECT_ROOT`と`ISUSCOPE_RUN_DIR`はisuscopeから環境変数で渡される

ベンチ起動方法が時間内に自動化できない場合は、`[benchmark]`を`mode = "external"`へ変更し、portalから手動起動して結果を入力する方法を一時的に利用できます。

### 4. nodeとSSHを設定する

`.isuscope/config.toml`へ、実際の接続情報と役割を設定します。

```toml
[ssh]
user = "ubuntu"
identity_file = "/absolute/path/to/key"
connect_timeout_seconds = 5

[[nodes]]
name = "isu1"
host = "192.0.2.1"
roles = ["edge", "app", "dns"]
```

role名は自由ですが、collectorの対象選択に使うため、実際の配置と一致させます。典型的には`edge`、`app`、`db`、`dns`、`control`を使います。全nodeへ次を確認します。

- BatchModeでSSH接続できる
- collectorが読むファイルへ必要な権限がある
- `sudo`が対話入力を要求しない
- node名が重複していない

### 5. fingerprintを問題環境へ合わせる

`.isuscope/fingerprint.sh`へ、当日比較したいremote実体を追加します。

- app binaryのSHA-256
- systemd unitと環境設定
- Nginx、MySQLなどの設定hashとversion
- OS、kernel
- 当日追加したcollector script

同じコードを配ったつもりでも実体が異なる事故を検出するため、最低限app binaryと主要設定のhashを全nodeから取得します。

### 6. collectorを設定する

生成された標準collectorは`run`と`survey-run`の両方で起動を試みます。

- sysstatによるhost CPU・disk
- perfによるsystem-wide hot symbol
- alpによるHTTP集計
- slpによるslow query集計
- remote fingerprint

`survey-run`だけが、標準観測にcookieなどの匿名識別子を使った行動遷移を追加します。

- 匿名viewer単位の行動遷移

ベンチ中のSSH転送、圧縮、ログ解析はスコアへ影響しやすいため避けます。ログ系collectorは開始前にoffsetだけ記録し、終了後に差分を回収・解析する構成を推奨します。perfはベンチと同時にsamplingするため、全runで同じ条件にします。

`before` collectorでベンチ実行の前提になるものには`required = true`を設定します。標準log collectorは開始時のoffsetと先頭SHA-256で最大5世代のrotationを追跡しますが、保持世代を越える可能性があるなら、ベンチ起動コマンドによるrotationをoffset記録より先に済ませます。

### 6.1 benchmark parserを設定する

viewer完走数、シナリオ成功数、DNS成功数などがbenchmark stdoutにだけ現れる場合は、
`.isuscope/parse-benchmark.sh`で汎用metric JSONLへ変換します。

```toml
[[benchmark.parsers]]
name = "contest-output"
command = [".isuscope/parse-benchmark.sh", "{benchmark_stdout}"]
timeout_seconds = 30
```

初回runの時点でparserが完成している必要はありません。stdoutは必ず保存されるため、run後に
scriptを実装して`isuscope enrich RUN_ID`を実行できます。再解析だけではベンチを消費しません。

### 7. アクセスログとroute規則を合わせる

ユーザー行動遷移には、少なくとも次のfieldが必要です。

- 時刻
- 匿名化したsessionまたはviewer識別子
- HTTP method
- URI

HTTP集計には、さらにstatus、request time、upstream time、response bytes、connection内request番号を推奨します。既存ログに必要なfieldがある場合はNginxを変更せず利用します。

`.isuscope/routes.toml`ではID、ユーザー名、hashなどを`:id`や`:key`へ正規化します。生の動的値をroute labelへ残すと、SQLiteの行数と解析時間が急増します。

### 8. setupを適用する

既存環境で不足するものだけ、`.isuscope/setup.sh`の`apply_environment`へ冪等に実装します。

```console
.isuscope/setup.sh
```

remote変更では、既存ファイルのbackup、設定test、atomicな配置、必要最小限のreloadを行います。パッケージ導入、remote build、常駐agentは、既存コマンドで代替できない場合だけ行います。

成功すると`.isuscope/setup-state.json`が作成されます。`config.toml`、`benchmark.sh`、`setup.sh`、route規則、setup状態、isuscope versionはrunごとに保存されます。

## 初回の受け入れ確認

### 1. 副作用なしの確認

```console
isuscope --version
isuscope list
bash -n .isuscope/benchmark.sh
bash -n .isuscope/parse-benchmark.sh
bash -n .isuscope/setup.sh
isuscope doctor
```

`list`がJSONを返せれば、configの構文とdata directoryを確認できます。

### 2. survey-runを1回だけ通す

```console
isuscope survey-run --hypothesis "初期状態の負荷構造を記録する"
isuscope report latest
```

投入完了の基準:

- ベンチ結果が保存され、scoreとPASS/FAILが正しい
- run stateが`complete`である
- required collectorがすべて完走している
- 全nodeのfingerprintが存在する
- 全edge nodeのアクセスログが保存されている
- HTTP metricが0件でない
- session fieldがある場合、transitionが0件でない
- 動的routeの未正規化によるseries爆発がない
- `perf-start`、`perf-stop`、`perf-report`、`perf-series`が全対象nodeで`complete`である
- `isuscope metrics latest`で`cpu.sample_count`の時刻付き行が0件でなく、process・binary・symbol labelがある
- perf stderrにlost sample、permission、unsupported eventの警告がなく、`[unknown]`や`-`のsymbol率が調査を妨げるほど高くない
- benchmark stdout/stderrが圧縮保存されている
- `tooling/extra/benchmark.sh`に当日実装が保存されている

`degraded`はベンチ自体がPASSでもcollectorが失敗した状態です。失敗collectorのstderrを確認し、受け入れ確認では`complete`になるまで直します。

perfはhost kernel、`perf_event_paranoid`、sudoers、kernel symbol公開範囲に依存するため、macOSやDocker上のparserテストだけでは受け入れ完了にしません。最初のLinux runでは次も確認します。

```console
isuscope metrics latest
isuscope series latest --metric cpu.sample_count --bucket 5
isuscope series latest --metric cpu.sample_percent --node app1 --bucket 5
```

対象processのsampleが全bucketで0件なら、アプリが軽いと結論づける前にcollector logとprocess/binary labelを確認します。観測条件を変更した場合、その前後のrunは同条件のスコア比較に使いません。

`isuscope report latest`の`.run.collectors`と`.coverage`で異常を確認します。collector logは`.run_logs` directoryと`.run.logs[].id`から`<run_logs>/<id>.zst`として参照できます。

### 3. SQLiteを確認する

既定のdata directoryを使っている場合の例です。

```console
sqlite3 .isuscope/data/isuscope.sqlite3
```

```sql
SELECT started_at, state, score, passed
FROM runs
ORDER BY started_at DESC
LIMIT 5;

SELECT status, COUNT(*)
FROM collector_runs
WHERE run_id = (SELECT id FROM runs ORDER BY started_at DESC LIMIT 1)
GROUP BY status;

SELECT COUNT(*)
FROM transitions
WHERE run_id = (SELECT id FROM runs ORDER BY started_at DESC LIMIT 1);
```

## 競技中の通常運用

### 軽量な反復

通常の変更では次を実行します。

```console
isuscope run --hypothesis "変更が対象metricを改善し、scoreを上げる"
isuscope report latest
isuscope diff 変更前のRUN_ID latest
```

仮説はrunと同時に必ず残します。PASS後は結果を判定・分析してから次のrunへ進みます。

```console
isuscope analyze RUN_ID supported --analysis "期待したmetricが改善し、scoreも上昇した"
```

判定は`supported`、`rejected`、`inconclusive`のいずれかです。分析を行えない場合は`isuscope analyze RUN_ID skipped --reason "理由"`を使います。FAILまたは中断runには分析は要求されません。

noteとtagもrunと同時に残せます。

```console
isuscope run --hypothesis "admission 64でviewer完走数が増える" --tag admission-64 --note "POST admission 63→64"
```

最終確認では、負荷になる常駐観測、collector、benchmark parser、log設定を先に撤去し、その最終構成を通常の`run`で記録します。

```console
isuscope run --hypothesis "観測を撤去した最終構成でscoreを確認する" --tag final
```

撤去後のconfig、source/tooling snapshot、benchmark結果、stdout/stderrが同じrunへ保存されます。

runの前に、変更目的をcommitまたは作業メモへ残します。dirty worktreeでもpatchとhashは保存されますが、意味のある単位でcommitすると比較しやすくなります。

### survey-runを使うタイミング

`survey-run`は競技序盤の調査フェーズだけで実行します。

- 初回の負荷構造調査
- 序盤にroutingやnode分担を組み立て直した直後の再調査

負荷構造を把握して改善サイクルへ入った後は、ボトルネックが移動しても`run`と保存済みmetricの比較で追います。survey用ログや解析がスコアへ影響するため、小さな修正と最終スコア確認には使いません。

### 比較時に見るもの

- scoreとPASS/FAIL
- commit、dirty、source state hash
- isuscope、config、benchmark adapterのhash
- app binaryと主要設定のfingerprint
- host/service CPU・memory
- route別request数、error、latency
- collector失敗とログ欠損

スコアだけが変わりfingerprintも変わっている場合、コード以外の配置差や設定差を先に疑います。

## 障害時

### ベンチ起動に失敗した

`benchmark-stdout.zst`と`benchmark-stderr.zst`を確認します。アダプターのnon-zero終了はrunをfailedとして保存します。ベンチ判定そのものがfailの場合は、JSONの`pass:false`と`messages`へ理由を入れます。

### collectorが失敗した

`isuscope report latest`の`.run_logs`と`.run.logs`で該当collectorのstderrを特定し、圧縮logを展開します。required before collectorが失敗した場合、ベンチは開始されません。

### 実行を止めたい

Ctrl-CまたはSIGTERMを使います。isuscopeはbenchmark process groupを停止し、after collectorを実行してaborted runとして保存します。SIGKILLや電源断ではhandlerを実行できませんが、次回run時に`.incomplete`をabortedとして回収します。

### SQLiteを失った・壊した

`runs/`以下が測定結果の正本です。isuscopeを停止し、SQLite本体、WAL、SHMを削除せず別ディレクトリへ退避してから、次を実行します。

```console
isuscope list
```

未登録runは`run.json`と圧縮collectorログから自動的に再構築されます。

### diskが不足した

実行中のrunは削除せず、終了後に古いrun directoryを別diskへ移します。SQLiteのlog IDだけでは生ログを復元できないため、`runs/<run-id>/`を単位として保管します。

## 競技終了時

- 実行中のisuscopeがないことを確認する
- data directory全体を手元へbackupする
- SQLite本体だけでなく`runs/`を必ず保存する
- 最終runのID、score、commit、dirty状態を記録する
- credentialや競技サーバーのIPを公開リポジトリへ入れない

## AIへ渡す初動指示

必要なら、次の文章とこの文書をAIへ渡します。

```text
このISUCONプロジェクトへisuscopeを導入してください。
まず渡された「ISUCON当日の導入・運用手順」と.isuscope/benchmark.shの契約を読み、まだベンチを起動せずに環境を調査してください。
実際のベンチ起動方法を.isuscope/benchmark.shへ、node/role/SSH/collectorを.isuscope/config.tomlへ設定してください。
既存ログで取得できる情報を優先し、remote変更は必要最小限かつ冪等にしてください。
秘密情報をstdout、stderr、Git管理ファイルへ出さないでください。
.isuscope/setup.shと構文検査が通った時点で変更内容を報告し、その後survey-runを1回実行してください。
結果はscore、run state、collector完走数、fingerprint数、metric数、transition数、保存ログ数で報告してください。
```
