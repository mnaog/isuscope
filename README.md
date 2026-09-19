# isuscope

isuscopeは、ISUCONのベンチマークと観測結果を1つのrunとして保存し、変更前後を再現可能に比較するためのローカルCLIです。

スコアだけでなく、仮説、Gitの状態、設定、HTTP・SQL・CPU・host metric、collectorの成否、Codex・Claude Codeの会話位置を同じrunへ紐付けます。通常は操作端末から各nodeへSSHし、競技サーバーへ専用agentを常駐させません。

## 基本の流れ

開始直後の全体調査では、通常の観測に匿名viewer単位の行動遷移を加える`survey-run`を1回だけ使います。

```console
isuscope doctor
isuscope survey-run --hypothesis "初期状態の負荷構造を記録する"
isuscope brief latest
isuscope analyze RUN_ID supported --analysis "初期状態を記録できた"
```

環境を受け入れた後は、仮説付きの`run`を改善ごとに繰り返します。

```console
isuscope run --hypothesis "postsの複合indexで一覧のDB時間を減らす"
isuscope brief latest
isuscope query latest --base BASE_RUN --view database --group-by sql-shape
isuscope analyze RUN_ID supported --analysis "p95とDB時間が低下し、スコアも改善した"
```

run IDは実行結果か`isuscope list`で確認します。`analyze`は更新対象を曖昧にしないため、run ID、一意な短縮ID、または一意なtagを明示します。`run`・`list`・`brief`はrun IDの末尾8文字を`short_id`として表示します（UUIDv7の先頭は近い時刻のrunで重なるため）。仮説や分析の本文でrunに触れるときもこの形にそろえてください。判定は`supported`、`rejected`、`inconclusive`、`skipped`です。`skipped`の理由は`--reason`でも`--analysis`でも書けます。

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

分析と採否を同時に決めた場合は、`analyze`で一度に記録できます。

```bash
isuscope analyze RUN_ID supported --base BASE_RUN \
  --analysis "アイドル接続が減りスコアも上がった" \
  --change keepalive-200ms --decision accepted
```

- 採否の理由には分析本文を、根拠runにはこのrunと`--base`を使います。
- 変更が未作成なら作成します。説明は`--description`、省略時はrunの仮説です。既存の変更に`--description`を付けるとエラーになります。
- `provisional`の`--revisit`不足などは分析を書き込む前に検査し、分析だけが残ることはありません。

- `analyze --base`は比較元の完全なrun IDを保存します。brief/UIは同じ比較処理で各1走のスコア差を表示し、誤差や性能採否は自動判定しません。
- スコア差の隣に**比較の前提**を出します。`source`（`state_sha256`。dirtyや変更commitも示す）、`observation`（config.toml、routes.toml、benchmark adapterなどのhashとisuscopeのversion）、`environment`（fingerprintのうち値が変わったもの。例: `nginx.config.sha256@app1`）、`benchmark`の4つを`same`／`changed`／`unknown`で返します。**`unknown`は`same`ではありません。** ベンチ側の条件はサーバーからは観測できないので、練習で自分がベンチを変えたときは`run --tag bench:<条件名>`で印を付けます。両方のrunに`bench:`tagがあるときだけ`same`／`changed`を判定し、無ければ`unknown`のままにします。
- `change decide`の状態は`accepted`（採用）、`provisional`（暫定採用）、`rejected`（不採用）、`deferred`（保留）です。未記録と保留は別です。
- `provisional`には`--revisit "再評価条件"`が必須です。理由と1件以上の根拠runは全状態で必須です。`--run`は繰り返せます。FAIL runも根拠にできます。
- 変更とrunは多対多です。複数変更を一度に試した場合も、一部だけ採用できます。根拠runのsource（commit、dirty、state digestなど）を採否記録に保存します。
- 同じ変更への再判断は追記され、`show`で全履歴を確認できます。`list --status`は現在の採否で絞り込みます。briefの採否も「関連変更の現在の判断」であり、当時の判断はshowで確認します。
- 採否はdeploy・merge・rollbackを行わず、実環境に反映済みであることも意味しません。採否未記録や暫定採用は次のベンチを阻止せず、従来の分析gateだけを適用します。
- `data_dir/changes/<id>/change.json`と`decisions/*.json`が正本です。この軽量な履歴もGitへ含めてください。SQLiteの変更・採否・根拠run索引はファイルから復元できます。古いrunから採否を自動推定しません。

終了前は観測用設定や重いログを環境から外したうえで、同じ`run`を使って採点用構成を確認します。専用の最終計測コマンドはありません。

## インストール

Rust 1.88以降でビルドします。

```console
cargo install --path . --locked
isuscope --version
```

競技用bundleは`./scripts/build-release-bundle.sh`で作れます。生成物にはbinary、SHA-256、README、LICENSEが含まれます。

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

## コマンド

| コマンド | 用途 |
|---|---|
| `init` | `.isuscope/`の雛形を生成する。`--print config`で、collector設定だけを実環境の値で標準出力へ出す（他のtoolが取り込む用） |
| `doctor` | ベンチを起動せず、設定・command・SSH・時刻・diskを検査する |
| `survey-run` | 序盤の全体調査を1回行い、行動遷移も収集する |
| `run` | 標準collectorでベンチを実行する |
| `list` | 保存済みrunを新しい順にJSONで一覧表示する。`--since 4h`などで開始時刻を絞る |
| `lock -- <command>` | 変更系操作の共通lockを取ってcommandを実行する。取得済みの子processでは再取得せず、実行時間を`operation-timing.tsv`へ追記する |
| `pin <run>` | runを生ログ（`logs/`）ごとGitへstageする |
| `routes suggest [run]` | 動的IDの残るHTTP routeから`[[routes]]`候補を作る。`--output`で書き出し先を指定する |
| `brief` | score、異常、benchmark値、主要性能sectionだけの小さいJSONを出力する |
| `series` | 時刻付きmetricをbucket化したJSONで調べる |
| `query` | SQLite上の保存済みmetricを絞り込み、安全な集約JSONで調べる |
| `sql` | 保存済みindexへ読み取り専用のSQLを実行する。`--schema`でtable定義、`--format tsv`で表形式 |
| `analyze` | PASSしたrunへ仮説の判定と分析を記録する |
| `enrich` | 保存済みbenchmark logへ現在のparserを再適用する |
| `ui` | 人間向けHTML UIをlocalhostで起動する |

`list`、`brief`、`series`、`query`、`sql`は機械処理しやすいJSONを返します。`brief`の`hosts`は**nodeごとに1行**（CPUの平均とピーク、最も詰まっていたコア、PSI、load、memory、disk、CPU上位のservice、詳細行数）で、全nodeの状況を最初の1画面で見切るためのものです。個々のmetricは`query --scope series --window load`や`series`へ進みます。まず`brief`で判断材料だけを確認し、上位件数から漏れた対象やrun集約metricは`query`で絞り込みます。両方で足りない問いは`sql`で直接引きます（`isuscope sql --schema`でtable定義、`isuscope sql "SELECT ..." --format tsv`で表形式。接続は読み取り専用で、書き込みは拒否されます）。runを丸ごと出す`report`と全件比較の`diff`は、SQL digestを含むJSONが1 MBを超えて読むのに向かないため廃止しました。同じ内容は`brief`・`query --base`・`sql`で取れ、人が見る場合は`ui`に同じ表示が残っています。database viewはcollector sourceを保ったままSQL digestを集約し、`--group-by sql-shape`で可変長`IN`をまとめられます。詳しい引数は`isuscope COMMAND --help`で確認できます。

どのmetricがあるかは`sql`で調べます。runごとの件数・単位・時系列の有無と、labelの種類がこれで分かります。

```console
isuscope sql "SELECT name, COUNT(*) samples, SUM(observed_at IS NOT NULL) timestamped,
  GROUP_CONCAT(DISTINCT unit) units FROM metrics
  WHERE run_id=(SELECT id FROM runs ORDER BY started_at DESC LIMIT 1)
  GROUP BY name ORDER BY samples DESC" --format tsv
isuscope sql "SELECT j.key label, COUNT(DISTINCT j.value) cardinality FROM metrics m, json_each(m.labels_json) j
  WHERE m.run_id=(SELECT id FROM runs ORDER BY started_at DESC LIMIT 1) GROUP BY j.key" --format tsv
```

```console
isuscope brief latest
isuscope query latest --metric benchmark.error --group-by error
isuscope query latest --base BASE_RUN --view database --source mysql-log-delta --label-contains digest=reservation_slots --group-by sql-shape
isuscope query latest --base BASE_RUN --view http --label-contains route=reservation
```

`query --base`は同じselectorを両runへ適用し、全件をfull outer joinしてから`--limit`を適用します。metric viewの行は、metric名ごとに値の大きい順に並べてから切ります。base/candidate/delta/delta percentとadded/removed/bothを返すため、対象を絞った比較で上位項目の入れ替わりを失いません。SQL shapeは可変長`IN`と複数行`VALUES`をまとめ、長いdigest exampleは短縮します。SQLiteとstructured snapshotの値は変更せず、query/briefの表示値だけを単位に応じて丸めます。

collectorの定義はisuscopeが配る1つのfileが正本です。`isuscope init`はそれを実環境の値で`.isuscope/config.toml`へ書き出し、独自の生成処理を持つprojectは`isuscope init --print config --no-scaffold --data-dir ... --nginx-access-log ... --service-units "..."`で同じ内容を取り込み、`[lock]`・`[ssh]`・`[[nodes]]`を自分で追記します。collectorを増やすときにprojectごとの写しを直して回らずに済みます。

## 保存されるデータ

`isuscope init`は保存先を`.isuscope/data/`にします（`config.toml`の`data_dir`で変更でき、`data_dir`を書かなければ`.isuscope/`直下です）。

```text
.isuscope/data/
├── isuscope.sqlite3
└── runs/
    ├── <run-id>/
    │   ├── run.json
    │   ├── source/
    │   ├── tooling/
    │   ├── logs/
    │   └── structured.json.zst
    └── .incomplete/          # 実行中のrun（<run-id>/と、実行中の印<run-id>.running）
```

各runにはスコアと成否、仮説と分析、Git commit・dirty patch・未追跡file hash、実行時のisuscope設定、collector出力と構造化metricを保存します。SQLiteは検索用の索引で、run directoryが記録の正本です。索引を失っても`isuscope list`の起動時に再構築されます。

`doctor`はベンチを起動せずに、`sample_output`に`initialize_start_marker`・`initialize_finish_marker`の文言が含まれるかを確認し（含まれなければ区間分けが失われるので警告）、collectorの`preflight`（例: `preflight = ["sh", "-c", "sudo -n test -r /var/log/mysql/mysql-slow.log"]`）を対象nodeで実行し、`[benchmark] sample_output`に保存した実際のベンチ出力へ全parserを適用します。parserが不正な行を出せば失敗、0件なら警告です。最新runのHTTP routeに動的IDが残っていれば、`routes suggest`の確認を促します。

`[benchmark] operator_line_pattern`に一致する行は、保存する前に捨てます。ISUCON12の`[ADMIN]`行のように、benchmarkerが運営向けにだけ出す情報はルール側であり、選手は見られません。捨てた行数だけを`run`の`operator`行と`brief`の`operator_lines_dropped`に残すので、log・metric・parser・briefのどこからも中身を読み戻せません。`doctor`が`sample_output`へparserを当てるときも同じ行を落とします。`[[nodes]]`に`rule_side = true`を付けたnode（benchmarker自身のmachineなど）では、collectorもdisk検査も動きません。

parserは`metric`に加えて`{"type":"message","kind":"failure"|"error","category":"...","text":"..."}`を出せます。`failure`はFAILした理由（最初の10件）、`error`はエラーの実例（categoryごとに最初の5件、最大20 category、各1000文字）で、runの`enrichments[].messages`に保存されます。上限を超えた件数は`omitted_message_count`に残ります。FAIL runは分析不要なので、`list`の`failure`、`brief`の`benchmark_messages`、`run`終了時の`failure`／`error`行がその理由の記録になります。parserが理由を出さなかったFAIL（adapterが何も出さずに終了した場合など）は、isuscopeが記録した`benchmark.error`を理由として表示します。benchmark出力に不正なUTF-8が混ざっても、捕捉とparserはその行だけを読み飛ばして続けます。既存runへは`isuscope enrich`で再適用できます。

### 最初のrunで確認すること

`survey-run`を1回通したら、`brief`で次を確かめてから改善へ進みます。`coverage_issues`が空であること、HTTP routeに動的IDが残っていないこと（残っていれば`routes suggest`）、`perf`系collectorが対象nodeで`complete`であること、transitionが0件でないこと（sessionのfieldがある場合）、benchmark stdout/stderrが保存されていることです。collectorが失敗したときは、`isuscope sql "SELECT name, node, status, error FROM collector_runs WHERE run_id=(SELECT id FROM runs ORDER BY started_at DESC LIMIT 1) AND status!='"'"'complete'"'"'" --format tsv`で原因を見て、run directoryの`logs/`にある該当collectorのstderrを開きます。`degraded`はベンチがPASSでもcollectorが失敗した状態です。観測条件を変えたrunは、その前後のscore比較に使いません。

### 得点が何でできているかを確定する

`survey-run`は、負荷構造に加えて**得点が何でできているか**を確定するための1回です。得点の作られ方は問題ごとに違います。ISUCON12本選はエンドポイントごとの重み付き成功数（当日マニュアルの点数表とaccess logだけで再現できます）、ISUCON13は正常に投稿されたライブコメントのtip合計で、後者はaccess logからは出ません。要求の数ではなく要求に入っていた値だからです。そのため、雛形には`score-probe`をコメントで用意しています。負荷の後に自分のアプリやDBへ問い合わせ、得点の源になる値を`score.`で始まるmetricとして出す`survey-run`専用のcollectorです。値はbriefの`score_inputs`に並びます。ベンチ側には触れません。

手順は4段階です。(1)当日マニュアルの得点計算を読み、項を書き出す。(2)重みで決まる型なら、access logのroute別成功数から模型scoreを出して実scoreと比べる。ISUCON12本選では差が0.04%で、present一覧と受取だけで得点の44%を占めると分かりました。(3)値の合計で決まる型なら`score-probe`でその値を読む。(4)どちらでも差が大きいなら、速さが次の負荷を呼ぶ型を疑い、1か所だけ遅らせてscoreの変化を見る実験へ切り替える。ここを飛ばすと、得点にならない経路を速くする作業に時間を使います。

模型が合っても分かるのは**今の得点の内訳**だけで、「このrouteを速くすると何点増える」ではありません。増分は、そのrouteが実際に上限になっている場合にしか出ません。上限が別（他のroute、CPU、ベンチ側の並列数）なら、得点の44%を占めるrouteを2倍速くしてもscoreは動きません。内訳は候補を絞る材料として使い、増えるかどうかは1回のベンチで確かめます。実際、ISUCON12本選の練習では全routeのサーバー時間を2.07 msから0.41 msにしても処理量は+3.6%で、上限はベンチ側にありました。

access logに`conn:$connection`と`msec:$msec`があると、`alp` collectorがnode上でベンチ側の接続の使い方も出します。`client.connections_in_use`（最初の要求から最後の応答までを積んだ同時接続数）、`client.connections_opened`（毎bucketの新規接続）、`client.request_gap`（応答を返してから同じ接続に次の要求が来るまでの時間。5秒ごとのp50/p95/p99）、`client.connection_requests_mean`／`_max`です。briefの`clients`にnodeごとに1行（同時接続数の平均とピーク、毎秒の新規接続、`request_gap`の分位、接続あたりの要求数）で出ます。

`client.connections_in_use`は**保持している接続数ではありません**。最後の応答の後にkeepaliveで開いたままの時間はaccess logに出ないので入りません。保持中の接続はkernelから取る`host.tcp_established`（`/proc/net/snmp`の`Tcp:CurrEstab`）で見て、2つを分けて扱います。`keepalive_timeout`のような「遊休接続を減らす」変更の効果は後者に出ます。サーバーの処理時間が短いのに`client.request_gap`が伸び、`client.connections_in_use`が並列数と一緒に増えるだけなら、上限はサーバーの外にある可能性が高くなります（サーバー側の計測だけでは確定できません）。

集約は意味を壊さないよう、`core`や`quantile`といったlabelを保ったまま行います。分位点は平均し直さず（`aggregation`が`max-of-quantile`）、再集約できないものは`value`が`null`になります。p50とp99を1行に混ぜた「平均の分位点」は出しません。

`host-sampler`はコア別の使用率（`host.core_busy_percent`と、最も詰まっているコアの`host.core_busy_max_percent`）と、PSI（`host.psi_cpu_some_percent`など、CPU・memory・I/Oの不足でtaskが足止めされた時間の割合）も出します。全体のCPUに余裕があっても1コアだけ飽和している構成を見落とさないためです。`service-throttle`は、cgroupのCPU上限で止められた時間（`service.cpu_throttled_percent`、`service.cpu_throttled_periods_per_second`）と割当量（`service.cpu_quota_cores`）をunitごとに出します。

slow logは、`mysql-log-delta`がベンチ中の差分をDB node上に用意し、`slp`がその場でinitializeと負荷区間に分けて集計します。区間は各文の`SET timestamp=`（開始時刻のepoch秒）で決めるのでtimezoneに依存せず、一括INSERTの`VALUES`と`IN`の並びは`--bundle-values --bundle-where-in`で1つの文にまとめます。文ごとに回数・合計・最大・p95・p99・lock・rowsを`window` label付きで出し、DB全体の5秒ごとの回数と時間（`db.calls`、`db.duration`）も出します。briefのDB欄は負荷区間（`database_window`）の行だけで順位を付けるので、initializeの一括投入が上位を占めません。区間は`isuscope query latest --view database --window load`で選べます。生のslow logは通常のrunでは運ばず、`survey-run`でだけ`mysql-log-raw`が持ち帰ります（DB 1台あたり1 runで数十MBになるため）。slpはAnsibleのobservability roleが入れる前提で、無いnodeでは`doctor`と`slp` collectorが失敗します。

`mysql-status`は2秒ごとに`SHOW GLOBAL STATUS`から、実行中thread、行lock待ちの回数と時間、log flush待ち、buffer poolの読み込み、fsyncを出します（`mysql.threads_running`、`mysql.row_lock_waits_per_second`など）。SQLが遅い理由が実行そのものか、競合やI/Oかを分けるための最小限で、Performance Schemaは有効化しません（有効化すると測る対象が変わるため、必要なときだけ別に行います）。

access logに`upstream_addr`と`upstream_connect`・`upstream_header`があると、接続先ごとに`http.upstream_requests`、`http.upstream_retried_requests`、`http.upstream_connect_duration`、`http.upstream_header_duration`、`http.upstream_response_duration`を出し、briefの`upstreams` sectionに要求数・再試行・p95を並べます。同じrouteでも特定のbackendだけ遅い、接続に時間がかかっている、ヘッダーは早いが応答完了が遅い、といった切り分けに使います。upstream別の値は**試行単位**です。nginxはretryした試行を各変数に同じ並びで記録するので、backend Aで1秒待って失敗しBが20 msで返した要求は、Aに1,000 ms、Bに20 msとして数えます（合計を最後の接続先へ付けると、正常なBが遅く見え、原因のAが消えます）。`http.upstream_requests`はその接続先への試行数、`http.upstream_retried_requests`はその接続先で失敗して次へ回された試行の数です。再試行を含む要求全体の時間はroute別の`http.request_duration`で見ます。

`host-sampler`はCPU・memory・loadに加えて、接続とネットワークのcounterも1秒ごとに出します。`host.tcp_passive_opens_per_second`、`host.tcp_established`、`host.tcp_time_wait`、`host.tcp_listen_overflows_per_second`、`host.tcp_listen_drops_per_second`、`host.tcp_syn_cookies_sent_per_second`、`host.tcp_time_wait_overflow_per_second`、`host.tcp_retransmit_segments_per_second`、NICごとの`host.net_rx_packets_per_second`などと、`host.cpu_softirq_percent`です。取りこぼしのcounterが0のままなら、少なくとも受け付けの取りこぼしとしては説明できません（接続の失敗がサーバー側にないことの証明にはなりません）。毎秒値は`/proc/uptime`で測った実際の間隔で割るので、samplerが1秒より遅れても率が水増しされません。読むのは`/proc`の小さなfileだけで、追加のtoolもroot権限も要りません。

ベンチ前後のcollectorは**node単位で並列**に実行します。同じnode内では設定順を保つので、perf-stopの後にperf-series、log markの後にdeltaという受け渡しは崩れません。localのcollectorは、nodeから持ち帰った成果物を読むため最後にまとめて実行します。practice-12ではベンチ後の後処理が中央値106秒（ベンチ本体は92秒）かかっており、その大半はnodeごとのSSHを1本ずつ待っていた時間でした。slow logとaccess logは生のまま運ばず、node上でslp・alpが集計した結果（1 nodeあたり数十KB）だけを持ち帰ります。practice-12ではaccess logの差分が1 runで76万行・272 MBあり、これを毎回転送して手元で解析し直していました。生のlogは`survey-run`でだけ`mysql-log-raw`・`nginx-log-raw`が持ち帰ります。perf.dataは`perf-series`の1回の`perf script`でrun全体と5秒ごとのsymbol別の割合を作ります（perf reportを重ねて走らせません）。perf flame graphはperf.dataを読み直す2つ目のpassになるため、既定では`survey-run`のときだけ作ります。

`[disk]`で、全nodeの空き容量を`doctor`と各ベンチの開始前に`df -Pk`で調べます。既定は`paths = ["/", "/var/log", "/tmp"]`、`node_warn_free_mb = 4096`（警告）、`node_min_free_mb = 1024`（`doctor`は失敗、`run`はベンチを開始しない。0で無効）です。ログはベンチごとに増え、node側のdiskが尽きるとdeployやDBが先に壊れるためです。雛形のaccess log・slow logの`*-log-mark` collectorは、前回までの差分を回収済みのログが1 GiBを超えていれば、ベンチ開始前に空にします（nginx・mysqldは追記モードで書くため、以後の行は先頭から入ります）。

`[lock] path`を指定すると、`run`と`survey-run`はベンチ全体でそのlockを持ちます。deployなど他の変更系操作も`isuscope lock --path <同じpath> -- <command>`で実行すれば、ベンチと重なりません。lockはdirectoryの中の`lock` fileを`flock`で握るもので、kernelが所有processの終了時に外すため、落ちたprocessのlockは残りません。`owner`は誰が握っているかの説明だけで、生きた所有者がいれば終了code 75で止まります。lock fileは消さずに残ります。

`[lock]`が無くても、同じdata directoryで2つの`run`は重なりません。実行中のrunは`runs/.incomplete/<run-id>.running`を`flock`で握り、後から始めた`run`はそれを見て開始を拒みます。握られていない`.incomplete`のrunだけを、中断されたrunとして`aborted`で確定します。

`[ssh] known_hosts_file`を指定すると、全SSH呼び出しがprojectのknown_hostsを`StrictHostKeyChecking=accept-new`で使います。ベンチ前のSSH collectorがあるnodeで、そのすべてがSSH接続自体の失敗（exit 255）になった場合は、計測のないrunを残さないようベンチを開始せずに失敗させます。

`[context.agent]`を設定すると、run開始時のCodexまたはClaude Codeのsessionと最後のUser inputを厳密に紐付けられます。sessionは`CODEX_SESSION_ID`／`CODEX_THREAD_ID`と`CLAUDE_CODE_SESSION_ID`から解決し、history fileの`- Agent:` headerと一致するものだけを採用します。旧名の`[context.codex]`、`codex-event`マーカー、既存runの`codex_context`も読み込めます。別sessionや通常ターミナルへ推測でfallbackせず、解決できない場合はベンチ開始前に停止します。
会話履歴はcontextとして別途snapshotされるため、設定した`history_dir`は性能sourceのdirty判定とpatchから自動的に除外されます。

## 観測の考え方

標準雛形はhost sampler、sysstat、指定systemd unitのcgroup sampler、perf、Flame Graph、off-CPU、ALP、slow query、fingerprintをnodeとphase単位で記録します。依存toolや権限がないcollector、または安全に追えないログrotationは、壊れた値を成功扱いせず`unavailable`として残します。時系列は`--window whole|initialize|load`で初期化と負荷走行を分離できます。briefの`hosts`と`clients`は、initializeの終わりが分かるrunでは負荷区間だけで要約し、どちらで要約したかを`hosts_window`（`load`か`whole`）に出します。

ALPはrouteごとのcount、status、sum/avg、min/max、p50/p95/p99を保存します。標準collectorを最初から全部入れる理由と、その受け入れ基準は[標準observability](docs/standard-observability.md)にあります。

## License

[MIT](LICENSE)
