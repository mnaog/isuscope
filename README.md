# isuscope

isuscopeは、ISUCONのベンチマークと観測結果を1つのrunとして保存し、変更前後を再現可能に比較するためのローカルCLIです。

スコアだけでなく、仮説、Gitの状態、設定、HTTP・SQL・CPU・host metric、collectorの成否、Codex・Claude Codeの会話位置を同じrunへ紐付けます。通常は操作端末から各nodeへSSHし、競技サーバーへ専用agentを常駐させません。

## 基本の流れ

開始直後の全体調査では、通常の観測に匿名viewer単位の行動遷移を加える`survey-run`を1回だけ使います。

遷移はsession内のリクエスト開始順で集計します。nginxログに`msec`と`reqtime`があれば、完了時刻から処理時間を引いて開始時刻を推定します。`transition.ordering_version=2`、`transition.precise_events`、`transition.legacy_events`に加え、遷移のfrom/to別に`transition.overlap_count`（実行区間が重なる隣接要求）と`transition.ambiguous_count`（同一開始時刻・旧ログ）を記録します。旧ログは従来のtimeを使い、順序不確定として残します。`brief`の`transitions`は各遷移にこの`overlap_count`と`ambiguous_count`を付け、旧ログの要求があれば`warnings`で伝えます。これらは観測順であり、因果関係や公式シナリオを保証しません。通常runでsessionログを追加収集する必要はありません。

```console
isuscope doctor
isuscope survey-run --hypothesis "初期状態の負荷構造を記録する"
isuscope brief latest
isuscope analyze RUN_ID supported --analysis "初期状態を記録できた"
```

初回は、ベンチ出力の形が分からずadapterがscoreと合否を読み取れないことがあります。そのときadapterは判定を保留（`{"type":"isuscope.result","pending":true}`）でき、runは`pending`で残ります。全collectorの計測はそのまま使えます。保存された出力（`logs/benchmark-stdout.zst`）でadapterの読み取りを確かめてから、`isuscope rescore`で同じ出力を読み直して確定します。確定するまで次のベンチは始まりません。

```console
isuscope survey-run --hypothesis "初期状態の負荷構造を記録する"   # result PENDING
isuscope rescore latest                                            # result PASS / score
```

`[benchmark] prepare_command`は、毎回のベンチの前（collectorより前）に実行されます。前のベンチが途中で切ったrequestのtransactionがinitializeを止めないよう、アプリを再起動してhealth checkを待つ、などに使います。失敗するとベンチを始めずにFAILとして記録し、出力は`benchmark-prepare`のlogに残ります。

環境を受け入れた後は、仮説付きの`run`を改善ごとに繰り返します。

```console
isuscope run --hypothesis "postsの複合indexで一覧のDB時間を減らす"
isuscope brief latest
isuscope query latest --base BASE_RUN --view database --group-by sql-shape
isuscope analyze RUN_ID supported --analysis "p95とDB時間が低下し、スコアも改善した"
```

run IDは実行結果か`isuscope list`で確認します。`analyze`は更新対象を曖昧にしないため、run ID、一意な短縮ID、または一意なtagを明示します。どの出力もrunをrun IDの末尾8文字（短縮ID）で示します（UUIDv7の先頭は近い時刻のrunで重なるため）。仮説や分析の本文でrunに触れるときもこの形にそろえてください。判定は`supported`、`rejected`、`inconclusive`、`skipped`です。`skipped`の理由も`--analysis`に書きます。

### 仮説の判定と変更の採否

仮説判定、スコア差、変更を残す判断は独立しています。スコアが下がっても局所改善や単純化を理由に採用できます。採用したことを理由に過去の仮説判定を書き換える必要はありません。

```bash
isuscope analyze RUN_ID inconclusive --base BASE_RUN \
  --analysis "対象の長時間SQLは減ったが、新規登録が遅くなりスコアは低下" \
  --change user-items-update --decision accepted \
  --description "既存user_itemsをUPDATEし、不足行のみINSERT"
# 後から判断を改めるときは、根拠runを付けて追記する
isuscope change decide user-items-update provisional --run RUN_ID --run NEXT_RUN \
  --reason "長時間SQL抑制は残す。新規登録の追加SELECTは別途改善" \
  --revisit "新規登録のSELECTを減らした後に再評価"
isuscope brief RUN_ID
isuscope change list --status provisional
isuscope change show user-items-update
```

- 変更は、最初に採否を決める`analyze --change`が作ります。説明は`--description`、省略時はrunの仮説で、対象にはrunのcommitを記録します。既存の変更に`--description`を付けるとエラーになります。
- `analyze`での採否の理由には分析本文を、根拠runにはこのrunと`--base`を使います。
- `provisional`の`--revisit`不足などは分析を書き込む前に検査し、分析だけが残ることはありません。

- `analyze --base`は比較元の完全なrun IDを保存します。`--base`を省くと、このrunの直前に始まって終わった（ベンチを始めた）runを比較元にし、どれを使ったかを`base`行に出します。`brief`は各1走のスコア差を表示し、誤差や性能採否は自動判定しません。分析が無い、または分析に比較元が無いrunでも、`brief`の`review.comparison`は同じ直前のrunと比べます（`base_source`が`previous_run`。分析の比較元なら`analysis`）。
- スコア差の隣に**比較の前提**を出します。`source`（`state_sha256`。dirtyや変更commitも示す）、`observation`（config.toml、routes.toml、benchmark adapterなどのhashとisuscopeのversion）、`environment`（fingerprintのうち値が変わったもの。例: `nginx.config.sha256@app1`）、`benchmark`の4つを`same`／`changed`／`unknown`で返します。**`unknown`は`same`ではありません。** ベンチ側の条件はサーバーからは観測できないので、練習で自分がベンチを変えたときは`run --tag bench:<条件名>`で印を付けます。両方のrunに`bench:`tagがあるときだけ`same`／`changed`を判定し、無ければ`unknown`のままにします。
- `change decide`の状態は`accepted`（採用）、`provisional`（暫定採用）、`rejected`（不採用）、`deferred`（保留）です。未記録と保留は別です。
- `provisional`には`--revisit "再評価条件"`が必須です。理由と1件以上の根拠runは全状態で必須です。`--run`は繰り返せます。FAIL runも根拠にできます。
- 変更とrunは多対多です。複数変更を一度に試した場合も、一部だけ採用できます。根拠runのsource（commit、dirty、state digestなど）を採否記録に保存します。
- 同じ変更への再判断は追記され、`show`で全履歴を確認できます。`list --status`は現在の採否で絞り込みます。briefの採否も「関連変更の現在の判断」であり、当時の判断はshowで確認します。
- 採否はdeploy・merge・rollbackを行わず、実環境に反映済みであることも意味しません。採否未記録や暫定採用は次のベンチを阻止せず、従来の分析gateだけを適用します。
- `data_dir/changes/<id>/change.json`と`decisions/*.json`が正本です。この軽量な履歴もGitへ含めてください。SQLiteの変更・採否・根拠run索引はファイルから復元できます。古いrunから採否を自動推定しません。

終了前は観測用設定や重いログを環境から外したうえで、同じ`run`を使って採点用構成を確認します。専用の最終計測コマンドはありません。

## インストール

標準のビルドにはRust 1.99.0を使います。rustup経由の`cargo`はリポジトリの`rust-toolchain.toml`に従ってこの版を選びます。最低対応版はRust 1.89で、CIでは1.89.0と1.99.0の両方を検証します。競技用bundleとcoverageも1.99.0でビルドします。

```console
cargo install --path . --locked
isuscope --version
```

競技用bundleは`./scripts/build-release-bundle.sh`で作れます。生成物にはbinary、SHA-256、README、LICENSEが含まれます。

nodeでaccess logを集計する`isuscope-node`は、`./scripts/build-node.sh <出力先>`でLinuxの静的binary（amd64とarm64）として作ります。依存はregexだけで、linkerはRustに付属するrust-lldを使うので、macOSからもDocker無しで作れます。nodeのOSに依存しないので、問題の公開前に作っておけます。nodeの`/usr/local/bin/isuscope-node`に置くと、`alp` collectorがaccess logの差分をnode上で集計します（2コアで62万行を約1.2秒。以前のawkとalpでは約20秒）。無いかbuildが違えばcollectorは失敗します。置いたbinaryが手元のisuscopeと同じsourceからのものかは、`isuscope-node --build-id`と`isuscope __access-summary --build-id`で照合でき、`isuscope doctor`も確かめます（[標準観測](docs/standard-observability.md#node上の集計binaryisuscope-node)）。

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

最初に`.isuscope/SETUP.md`を読み、少なくともベンチ起動方法、initializeと負荷走行の境界（出力のmarkerか`routes.toml`の`[initialize]`）、SSHとnode、アクセスログ（MariaDBなどではslow logも）、route正規化、app binaryと主要serviceを実環境へ合わせます。その後、shellの構文、setup、doctorを順に確認します。

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
| `doctor` | ベンチを起動せず、設定・command・SSH・時刻・diskを検査する。警告と失敗だけを1行ずつ出し、通った検査は数で出す（`--verbose`で通った検査も並べる） |
| `survey-run` | 序盤の全体調査を1回行い、行動遷移も収集する |
| `run` | 標準collectorでベンチを実行する |
| `list` | 保存済みrunを新しい順にJSONで一覧表示する |
| `lock -- <command>` | 変更系操作の共通lockを取ってcommandを実行する。取得済みの子processでは再取得せず、実行時間を`operation-timing.tsv`へ追記する |
| `routes suggest [run]` | 動的IDの残るHTTP routeから`[[routes]]`候補を作る。`--output`で書き出し先を指定する |
| `brief` | score、異常、benchmark値、主要性能sectionだけの小さいJSONを出力する |
| `series` | 時刻付きmetricをbucket化したJSONで調べる |
| `query` | SQLite上の保存済みmetricを絞り込み、安全な集約JSONで調べる |
| `sql` | 保存済みindexへ読み取り専用のSQLを実行する。`--schema`でtable定義、`--format tsv`で表形式 |
| `states sql` / `states timeline` | アプリの状態ログ（runの生の行）をSQLiteへ載せて読む（runごとに残し、次からは開くだけ）。`timeline`は1つの対象の変化と、attrsでつながる対象の変化を時刻順に並べる |
| `analyze` | PASSしたrunへ仮説の判定と分析を記録する |
| `enrich` | 保存済みbenchmark logへ現在のparserを再適用する |
| `rescore` | adapterが判定を保留した（pending）runを、保存済みのベンチ出力から`[benchmark] rescore_command`で確定する |

`list`、`brief`、`series`、`query`、`sql`、`states`、`change`は機械処理しやすいJSONを返します（`schema_version`は2。表を`columns`と`rows`で出す形になったときに上げました）。どの出力も同じ約束で書きます。

- runを指す値は、そのrunなら`run`、比較元なら`base`で、値は短縮IDです。完全なIDは`list`の`id`だけに出します。
- 表は`total_count`（切る前の件数）と`truncated`を必ず持ちます。briefの各欄、`list`の`runs`、`change`の`changes`・`decisions`も同じです。
- 区間は`window`（`whole`・`initialize`・`load`）という名前で出します。`series`の実際の時刻とbucketは`range`です。
- 比較は、比較元を`_base`、今回を接尾辞なし、差を`_delta`と`_delta_percent`で書きます。briefのスコアの比較も同じです。
- `warnings`はどの出力にもあり、無ければ空です。briefの`next`も同じです。
- 結果が空のときは、理由と次のコマンドを`warnings`に出します（時系列が無くrun集約だけある、絞り込みで消えた、名前が無い）。errorも、どのrunか・どうすればよいかを添えます。
- 行を返すコマンド（`query`・`series`・`sql`）の`--limit`の既定値は100です。
- commitは先頭12桁です。
- 絞り込みの引数（`--metric`・`--metric-prefix`・`--source`・`--node`・`--label`・`--label-contains`）は`query`と`series`で同じ意味です。

同じ形の行が並ぶ表（各出力の行、`brief`の各欄と`hosts`・`clients`、`list`の`runs`、`change`の`changes`・`decisions`など）は、列名を`columns`に1回だけ置き、`rows`の各行を値の並びにします。0件でも`columns`と`rows`の形は変わりません。キーは「対象_統計_単位」（例: `cpu_busy_max_percent`、`cpu_max_cores`、`p95_ms`）で、単位の無い値（件数・接続数・load）は「対象_統計」（例: `load1_max`、`connections_in_use_avg`）、時間あたりの値は`_per_second`で終えます。統計は`avg`・`max`・`min`・`p95`・`total`、メモリはMiBです。`query --base`の各行は、行を識別する値（node、route、digest、metricとlabelsなど）と`presence`に続けて、判断に使う列だけを並べます。主要な値（回数・合計・平均・p95、DBはlockも）は今回の値と差の割合（`calls`、`calls_delta_percent`）、HTTPのエラー数とDBの1回あたりの検査行数は比較元と今回（`errors_base`、`errors`）です。0からの増減や前後の値そのものが判断材料になるためです。両runに1回以上ある行には、合計時間の差を回数の増減による分（`total_ms_delta_by_calls`、HTTPは`total_ms_delta_by_count`）と1回あたりの時間の増減による分（`total_ms_delta_by_avg`）に分けた列も付きます。2つの和が合計時間の差です。合計が増えても1回あたりは速くなった、のような変化を取り違えないためのものです。`--all-columns`を付けると、すべての値の比較元・今回と、主要な値の差（`calls_delta`）も出します。全行で同じ識別値は出力の`common`へ1回だけ出します。比較しない`query`でも、全行で同じnode・source・window・labelなどは`common`へ1回だけ出します。行を返す出力（`query`、`series`、`sql`、`list`、`change list`）は約12KBを超えないよう末尾の行を減らし、そのときは`truncated`と、絞り方を添えた`warnings`で伝えます（全行が要るときは`ISUSCOPE_OUTPUT_BYTES=0`）。`brief`は`--limit`で各欄の件数を抑え、それでも全体が約12KBを超えるときは順位の表（`hosts`などnodeごとの表を除く）をすべて同じ行数まで減らして`warnings`で伝え、`change show`は判断の全履歴を出すので減らしません。`series`はmetricごとの単位と集計方法を`metrics`に1回だけ置き、`coverage`には完了しなかったcollectorだけを並べます。`--group-by sql-shape`の行では`digest`が文の形になり、元の文が2種類以上あるときだけ`digest_examples`に並べます。`brief`のhttpとdatabaseの表は判断に使う主要な列（回数・合計・平均・p95、HTTPはエラー数、DBはlockと1回あたりの検査行数）だけで、分位・最大・行数などの残りは`query`で見ます。時刻はどの出力もJSTのミリ秒まで（例: `2026-10-07T11:10:58.637+09:00`）で、`sql`だけは保存値（UTC）をそのまま返します。`brief`の`hosts`は**nodeごとに1行**（CPUの平均とピーク、最も詰まっていたコア、PSI、load、memory、disk、上位のservice。serviceはCPU・RSS・thread数のピークと、ベンチの間の再起動回数）で、全nodeの状況を最初の1画面で見切るためのものです。負荷の低いnodeもまとめずに全行を出します。`cpu`はidle taskの待機（`swapper`の`native_safe_halt`など）を除いて順位を付けます。`limits`は、ベンチの前に`limits` collectorが読んだ接続・fdの上限値を、nodeと読んだ設定のkey（`config_key`：`net.core.somaxconn`、nginxの`worker_connections`・`worker_rlimit_nofile`・`keepalive_requests`、unitの`LimitNOFILE`と`service`、MySQLの`max_connections`）ごとに`value`で並べ、同じ上限を数える観測値があればベンチ中のピーク（`peak`と`peak_metric`：接続あたりのrequest数、MySQLの接続数）を隣に置きます。`evidence`は上限に当たったときにだけ出る決まった文言のlog行（Too many open files／EMFILE、worker_connections are not enough、Too many connections、possible SYN flooding）の数と例、listen overflowの数です。上限に近いかどうかの判定はしません。upstreamの切断やconnection refusedのような一般的な接続エラーは上限を示さないので`logs`だけに出ます。`evidence`のある行が先に並びます。上限値も観測も無ければ空です。`logs`はベンチの間にアプリ・nginxのerror log・kernelが出したエラーを、数字を含む語を`<N>`にした型ごとに数えた多い順で、briefには件数と最初の1行だけを出します（型は`query --metric log.error_lines`）。`database_io`はMySQLがfileの読み書きと、fsyncなどそれ以外の操作（`misc_wait_ms`、fsyncだけの時間ではない）で待った時間の長い順、`database_memory`はbuffer poolとtableの大きさです。どちらもベンチの前後の差なのでinitializeを含みます。`coverage_issues`は失敗・欠け・一部だけのcollectorを`status`ごとにnodeをまとめて並べ（失敗が先）、nodeに道具が無いなどで取れなかった`unavailable`のcollectorは数（`coverage_notes_hidden`）と、それを並べるSQL（`coverage_notes_more`）だけを出します。`review`は分析・変更の本文を冒頭だけにし、全文を読む命令を`full_text`に添えます。件数で切った欄には、残りを見るコマンドを`more`に添えます（切っていない欄には付きません）。`next`には、比較元があるrunでHTTPとDBの`query --base`を出します。読むだけのコマンドに限り、どの表を見るべきかのような推測は出しません。briefは並行して作業する別の人も読むので、`analyze`は出しません（runを走らせた本人へは`run`の終了時に出ます）。個々のmetricは`query --scope series --window load`や`series`へ進みます。まず`brief`で判断材料だけを確認し、上位件数から漏れた対象やrun集約metricは`query`で絞り込みます。両方で足りない問いは`sql`で直接引きます（`isuscope sql --schema`でtable定義、`isuscope sql "SELECT ..." --format tsv`で表形式。接続は読み取り専用で、書き込みは拒否されます）。runを丸ごと出す`report`と全件比較の`diff`は、SQL digestを含むJSONが1 MBを超えて読むのに向かないため廃止しました。同じ内容は`brief`・`query --base`・`sql`で取得できます。database viewはcollector sourceを保ったままSQL digestを集約し、`--group-by sql-shape`で可変長`IN`をまとめられます。詳しい引数は`isuscope COMMAND --help`で確認できます。

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

`query --base`は同じselectorを両runへ適用し、全件をfull outer joinしてから`--limit`を適用します。metric viewの行は、metric名ごとに値の大きい順に並べてから切ります。片側にしか無い行もadded/removedとして返すため、対象を絞った比較で上位項目の入れ替わりを失いません。SQL shapeは可変長`IN`と複数行`VALUES`をまとめ、長いdigest exampleは短縮します。SQLiteとstructured snapshotの値は変更しません。表示では`brief`の要約値を単位に応じて丸め、`sql`以外の出力は小数3桁（0.001未満は有効数字3桁）までにします。`sql`は保存値をそのまま返します。

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

### ベンチの前後：準備、同じ構成の再測定、期限

`[benchmark]`の3つのcommandは、どれも手元でproject rootをcurrent directoryにして動きます。環境変数はisuscopeを起動したshellから引き継ぎ（`FOO=... isuscope run`のような指定もそのまま届きます）、`ISUSCOPE_RUN_ID`・`ISUSCOPE_RUN_DIR`・`ISUSCOPE_RUN_MODE`・`ISUSCOPE_PROJECT_ROOT`と、`--repeat`を付けたときは`ISUSCOPE_REPEAT_REASON`が加わります。stdoutとstderrは`benchmark-<name>-stdout`などのlogとしてrunに残り、結果は`run.json`の`hooks`に入ります。期限は`hook_timeout_seconds`（既定120秒）です。

- `prepare_command`：毎回のベンチの前、before collectorより前に動きます（アプリの再起動など）。失敗したrunはcollectorもベンチも動かさず、FAILとして残します。
- `identity_command`：配ったものを識別する値をstdoutへ出します。JSON objectならfieldごとに比べ、`run`の終了時に`identity`行へ並べます。
- `cleanup_command`：isuscopeがベンチを止めたとき（下の期限、出力の途絶え、signal）に動きます。別nodeのbenchmarkerを止める処理はここにまとめます。

**同じ構成は理由なしに測り直しません。** before collectorの後で、ベンチを実際に始めた直前のrun（prepareで止まったrunや、ここで止めたrunは含めない）と、配ったもの（`identity_command`の出力。無ければsourceの状態）、nodeのfingerprint、計測の設定（`config.toml`、`routes.toml`、`[tooling] include`のfile、isuscopeの版）、runの種類を比べます。違いが1つも無ければベンチを始めずにFAILとして残すので、deployし忘れや編集の失敗に、ベンチ1回分の時間を使いません。ばらつきの確認や観測の追加で同じ構成を測るときは`--repeat "<理由>"`を付けます。比べた結果は`run.json`の`repeat`（比較元、違った項目、理由）に残り、`run`の終了時に`changed`行（違った項目）か`repeat`行（理由）で出ます。片方にしか無いfingerprintは「同じ」とは扱いません。nodeのfingerprintは`/usr/local/lib/isuscope/fingerprint.sh`が出し、比べたいfile・directoryは`/usr/local/lib/isuscope/fingerprint-paths`へ1行ずつ書きます。

**期限。** `timeout_seconds`はベンチ全体の期限で、adapterへ`ISUSCOPE_BENCHMARK_TIMEOUT_SECONDS`として渡します。adapterがその秒数で自分から止めて結果を出せるよう、isuscope自身が止めるのはさらに30秒後です。`idle_timeout_seconds`は、stdoutにもstderrにも何も出ない時間の上限です（initializeやpollの間に黙る時間より長くします）。isuscopeが止めたrunは`benchmark.stopped_by`に`timeout`・`idle`・`interrupted`のどれかが残り、`run`の終了時に`stopped`行で出ます。手元のsshを止めても別nodeのbenchmarkerが止まるとは限らないので、adapterはそのnode側でも止まるようにし（例：`timeout -k 10 "$ISUSCOPE_BENCHMARK_TIMEOUT_SECONDS" ./bench`）、`cleanup_command`でも止めます。

**initializeの境界。** まずベンチの出力（`initialize_start_marker`・`initialize_finish_marker`の文言か、`isuscope.event`の行）が届いた時刻から取ります。取れなかったrunでは、ベンチの直後に`phase = "boundary"`のcollectorが動きます。雛形の`initialize-boundary`は、`routes.toml`の`[initialize]`（`method`と、query stringを除いて完全一致する`path`）に一致する要求のうち、ベンチの間で最後に応答を返し終えた時刻（access logの`$msec`）を負荷の始まりにします。どこから取ったかは`benchmark.initialize_source`（`benchmark_output`か`access_log`）に残り、`run`の終了時に`initialize`行で出ます。境界の無いrunは、hosts・clients・databaseがinitializeを含むベンチ全体の要約になるので、`brief`の`warnings`で伝えます。

parserは`metric`に加えて`{"type":"message","kind":"failure"|"error","category":"...","text":"..."}`を出せます。`failure`はFAILした理由（最初の10件）、`error`はエラーの実例（categoryごとに最初の5件、最大20 category、各1000文字）で、runの`enrichments[].messages`に保存されます。上限を超えた件数は`omitted_message_count`に残ります。FAIL runは分析不要なので、`list`の`failure_reason`、`brief`の`benchmark_messages`、`run`終了時の`failure`／`error`行がその理由の記録になります。parserが理由を出さなかったFAIL（adapterが何も出さずに終了した場合など）は、isuscopeが記録した`benchmark.error`を理由として表示します。benchmark出力に不正なUTF-8が混ざっても、捕捉とparserはその行だけを読み飛ばして続けます。既存runへは`isuscope enrich`で再適用できます。

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

SQLの回数と時間は、既定ではperformance_schemaのstatement digestから取ります（`mysql-digest`）。ベンチの前にdigest表を空にし、ベンチの間は1秒ごとに累積値を写し、ベンチの後にinitializeと負荷区間の差を出します。slow log（`long_query_time=0`）は全文を書くのでベンチ中のDBの負荷になり、後のslpの集計も数十秒かかるためです。出力はslpと同じ形なので、briefとqueryのdatabase viewは同じように使えます（両方あればdigestを使います）。p95・p99・最大はinitializeを含むベンチ全体の分布です。server側のprepared statement（Rustのsqlxなどがbinary protocolで実行する文）はdigest表に入らないので、`prepared_statements_instances`をSQL文ごとに足して同じ形で出します（識別子は`ps:`で始まり、分位は無く最大だけ。接続が閉じて行が消えても、それまでの回数と本文は残します）。MariaDBやperformance_schemaが無効な環境では`mysql-digest`が`unavailable`になるので、slow logを有効にします。

slow logを有効にした場合は、`mysql-log-delta`がベンチ中の差分をDB node上に用意し、`slp`がその場でinitializeと負荷区間に分けて集計します。区間は各文の開始時刻で決めます。`SET timestamp=`は開始の秒（epoch）しか持たないので、`# Time:`（書き終えた時刻、マイクロ秒）の秒以下から`Query_time`を引いて補い、負荷の始まりの直後の文をinitializeへ入れません。秒以下だけを使うのでtimezoneに依存せず、`# Time:`の無い文は秒で振り分けて`db.slow_log_coarse_time`に数を残します。一括INSERTの`VALUES`と`IN`の並びは`--bundle-values --bundle-where-in`で1つの文にまとめます。文ごとに回数・合計・最大・p95・p99・lock・rowsを`window` label付きで出し、DB全体の5秒ごとの回数と時間（`db.calls`、`db.duration`）も出します。briefのDB欄は負荷区間（`database_window`）の行だけで順位を付けるので、initializeの一括投入が上位を占めません。区間は`isuscope query latest --view database --window load`で選べます。生のslow logは通常のrunでは運ばず、`survey-run`でだけ`mysql-log-raw`が持ち帰ります（DB 1台あたり1 runで数十MBになるため）。slow logがあるのにslpが無いnodeでは、`doctor`と`slp` collectorが失敗します（slow logが無ければ`slp`は`unavailable`で飛ばします）。

`mysql-status`は2秒ごとに`SHOW GLOBAL STATUS`から、実行中thread、行lock待ちの回数と時間、log flush待ち、buffer poolの読み込み、fsyncを出します（`mysql.threads_running`、`mysql.row_lock_waits_per_second`など）。SQLが遅い理由が実行そのものか、競合やI/Oかを分けるための最小限で、Performance Schemaは有効化しません（有効化すると測る対象が変わるため、必要なときだけ別に行います）。

access logに`upstream_addr`と`upstream_connect`・`upstream_header`があると、接続先ごとに`http.upstream_requests`、`http.upstream_retried_requests`、`http.upstream_connect_duration`、`http.upstream_header_duration`、`http.upstream_response_duration`を出し、briefの`upstreams` sectionに要求数・再試行・p95を並べます。同じrouteでも特定のbackendだけ遅い、接続に時間がかかっている、ヘッダーは早いが応答完了が遅い、といった切り分けに使います。upstream別の値は**試行単位**です。nginxはretryした試行を各変数に同じ並びで記録するので、backend Aで1秒待って失敗しBが20 msで返した要求は、Aに1,000 ms、Bに20 msとして数えます（合計を最後の接続先へ付けると、正常なBが遅く見え、原因のAが消えます）。`http.upstream_requests`はその接続先への試行数、`http.upstream_retried_requests`はその接続先で失敗して次へ回された試行の数です。再試行を含む要求全体の時間はroute別の`http.request_duration`で見ます。

`host-sampler`はCPU・memory・loadに加えて、接続とネットワークのcounterも1秒ごとに出します。`host.tcp_passive_opens_per_second`、`host.tcp_established`、`host.tcp_time_wait`、`host.tcp_listen_overflows_per_second`、`host.tcp_listen_drops_per_second`、`host.tcp_syn_cookies_sent_per_second`、`host.tcp_time_wait_overflow_per_second`、`host.tcp_retransmit_segments_per_second`、NICごとの`host.net_rx_packets_per_second`などと、`host.cpu_softirq_percent`です。取りこぼしのcounterが0のままなら、少なくとも受け付けの取りこぼしとしては説明できません（接続の失敗がサーバー側にないことの証明にはなりません）。毎秒値は`/proc/uptime`で測った実際の間隔で割るので、samplerが1秒より遅れても率が水増しされません。毎秒値は小数で出し、取りこぼし系（`host.tcp_listen_overflows`、`host.tcp_listen_drops`、`host.tcp_syn_cookies_sent`、`host.tcp_time_wait_overflow`、`host.tcp_retransmit_segments`）は区間の件数も整数で残します。整数で割っていたころは、1.01秒に1件の取りこぼしが0件/秒になり、「起きていない」と区別できませんでした。読むのは`/proc`の小さなfileだけで、追加のtoolもroot権限も要りません。

ベンチ前後のcollectorは**node単位で並列**に実行し、同じnodeの中でも`sequence`の違うものは並行に動かします。同じ`sequence`の中では設定順を保つので、perf-stopの後にperf-series、log markの後にdeltaとalpという受け渡しは崩れません（標準設定はperf・off-CPU・nginx log・app log・slow log・MySQL I/Oをそれぞれ1つの`sequence`にしています。`sequence`の無いcollectorはnodeごとに1列で順に動きます）。sshdが認証前の接続を落とし始めないよう、1 nodeへ同時に走らせるのは8本までです。ベンチ中のcollectorは全部へ同時に停止を送って並行に回収し、benchmark parserはベンチの出力だけを読むので、その停止とafter collectorを待たずに並行して動かします。localのcollectorは、nodeから持ち帰った成果物を読むため最後にまとめて実行します。各段階（`collectors_before`、`benchmark`、`collectors_during_stop`、`collectors_after`、`benchmark_parsers`など）の開始時刻と所要時間は`run.json`の`stages`に、collectorとparserごとの値は各結果の`started_at`・`duration_ms`に残り、`run`の終了時に`timing`の行へまとめて出します。practice-12ではベンチ後の後処理が中央値106秒（ベンチ本体は92秒）かかっており、その大半はnodeごとのSSHを1本ずつ待っていた時間でした。slow logとaccess logは生のまま運ばず、node上でslp・`isuscope-node`が集計した結果（1 nodeあたり数十KB）だけを持ち帰ります。MySQLのSQLは、performance_schemaのstatement digestから`mysql-digest`がslpと同じ形で集計します。slow log（`long_query_time=0`）は全文を書くのでベンチ中のDBの負荷になり、slpの集計にも1回数十秒かかっていたためです（practice-14の序盤では後処理37.7秒のうち27秒）。slow logがあればslpも動き、両方あればbriefは`mysql-digest`を使います。practice-12ではaccess logの差分が1 runで76万行・272 MBあり、これを毎回転送して手元で解析し直していました。生のlogは`survey-run`でだけ`mysql-log-raw`・`nginx-log-raw`が持ち帰ります。perf.dataは`perf-series`の1回の`perf script`でrun全体と5秒ごとのsymbol別の割合を作ります（perf reportを重ねて走らせません）。perf flame graphはperf.dataを読み直す2つ目のpassになるため、既定では`survey-run`のときだけ作ります。

`[disk]`で、全nodeの空き容量を`doctor`と各ベンチの開始前に`df -Pk`で調べます。既定は`paths = ["/", "/var/log", "/tmp"]`、`node_warn_free_mb = 4096`（警告）、`node_min_free_mb = 1024`（`doctor`は失敗、`run`はベンチを開始しない。0で無効）です。手元の`data_dir`のfilesystemは`local_warn_free_mb = 1024`を下回ると`doctor`が警告します。`[doctor] max_clock_skew_seconds = 2`は、nodeの時計と手元の時計の差がこれを超えると`doctor`が警告する値です（nodeごとのsampleは時刻で突き合わせるため）。警告にはどの設定の値かを添えます。ログはベンチごとに増え、node側のdiskが尽きるとdeployやDBが先に壊れるためです。雛形のaccess log・slow logの`*-log-mark` collectorは、前回までの差分を回収済みのログが1 GiBを超えていれば、ベンチ開始前に空にします（nginx・mysqldは追記モードで書くため、以後の行は先頭から入ります）。

`[lock] path`を指定すると、`run`と`survey-run`はベンチ全体でそのlockを持ちます。after collectorが終わってnodeへの操作が無くなった時点で手放すので、runが手元で保存している間に次のdeployを始められます。deployなど他の変更系操作も`isuscope lock --path <同じpath> -- <command>`で実行すれば、ベンチと重なりません。lockはdirectoryの中の`lock` fileを`flock`で握るもので、kernelが所有processの終了時に外すため、落ちたprocessのlockは残りません。`owner`は誰が握っているかの説明だけで、生きた所有者がいれば終了code 75で止まります。lock fileは消さずに残ります。

`[lock]`が無くても、同じdata directoryで2つの`run`は重なりません。`run`は入口で`runs/.incomplete`のdirectoryを`flock`で握って終わるまで持ち、同時に始めた2本目は、1本目がまだディスク確認やgit snapshotの途中でも開始を拒みます。実行中のrunは`runs/.incomplete/<run-id>.running`も`flock`で握ります。握られていない`.incomplete`のrunだけを、中断されたrunとして`aborted`で確定します。

`[ssh] known_hosts_file`を指定すると、全SSH呼び出しがprojectのknown_hostsを`StrictHostKeyChecking=accept-new`で使います。ベンチ前のSSH collectorがあるnodeで、そのすべてがSSH接続自体の失敗（exit 255）になった場合は、計測のないrunを残さないようベンチを開始せずに失敗させます。

`[context.agent]`を設定すると、run開始時のCodexまたはClaude Codeのsessionと最後のUser inputを厳密に紐付けられます。sessionは`CODEX_SESSION_ID`／`CODEX_THREAD_ID`と`CLAUDE_CODE_SESSION_ID`から解決し、history fileの`- Agent:` headerと一致するものだけを採用します。旧名の`[context.codex]`、`codex-event`マーカー、既存runの`codex_context`も読み込めます。別sessionや通常ターミナルへ推測でfallbackせず、解決できない場合はベンチ開始前に停止します。
会話履歴はcontextとして別途snapshotされるため、設定した`history_dir`は性能sourceのdirty判定とpatchから自動的に除外されます。

### アプリの状態ログ

HTTPとSQLの記録からは、アプリの中の対象（注文、予約、配車など）が**今どの状態で、いつ変わったか**が分かりません。それを知るために計測を足したrunを回すと、ベンチ1回分の時間を使います。状態ログは、アプリが状態の変化を1行ずつ書き、isuscopeが毎回のrunで運ぶ記録です。一度入れれば、状態についての問いは前のrunの記録を引くだけで答えられます。

1行に1つの変化を、JSONで書きます（`/var/tmp/isuscope-state.jsonl`へ追記。pathは`init --state-log`で変えられます）。

```json
{"t":1760259047123456,"seq":81234,"kind":"ride","id":"01J9…","from":"MATCHING","to":"ENROUTE","attrs":{"chair":"01J8…","pickup_distance":512}}
```

- `t`はUnix時刻のマイクロ秒、`seq`はprocess内の通し番号（同じ`t`の順序）。`kind`と状態の名前はアプリが使う値のままで、isuscopeは問題を知りません。
- 作られたときは`"from":null`。書き手が前の状態を知らないとき（状態の列を読まずにUPDATEするときなど）は`from`を書きません。isuscopeが追ってきた状態で補います。
- `attrs`は集計に使う少しの値（関係する対象のID、距離、金額など）で、入れ子にしません。
- 書けずに捨てた行は`{"t":...,"dropped":<数>}`の1行で残します。書き手はリクエストを待たせないよう、あふれたら捨てます。

`state-log-mark`・`state-log-delta`（role `app`）がベンチの前後の差分を運び、runのlogに残します（生のlogなのでGitには入りません）。ファイルがまだ無ければ`unavailable`になるだけで、runは止まりません。`parser = "state-log"`が負荷走行の区間（initializeの終わりが分からなければベンチ全体）について集計します。

| metric | label | 意味 |
|---|---|---|
| `state.dwell.count`・`total_duration`・`p50_duration`・`p95_duration`・`p99_duration`・`max_duration` | `kind`, `state`, `window` | 状態にいた時間（ms）。状態が変わった行から次に変わった行まで。区間の中で出たものだけ |
| `state.dwell.open` | `kind`, `state`, `window` | 区間の終わりでまだその状態にいた対象の数 |
| `state.transitions` | `kind`, `from`, `to`, `window` | 遷移の回数。作られたときの`from`は`-`、前の状態が分からなければ`?` |
| `state.occupancy`（1秒ごと）・`state.occupancy_max`（区間の中の最大。変わるたびに数える） | `kind`, `state`（最大は`window`も） | その状態にいる対象の数（ログに現れた対象だけ） |
| `state.lines`・`state.invalid_lines`・`state.dropped`・`state.inconsistent`・`state.truncated_bytes` | | 読んだ行、形の違う行、書き手が捨てた行、前の状態が合わなかった行、1 runの上限（256MiB）を超えて運ばなかった大きさ |

`brief`の`states`欄は、状態にいた時間の合計が長い順に並べ、欠け（捨てた行、形の違う行、合わなかった行、上限）を`warnings`に出します。前のrunとの比較は`query --base`、時間の推移は`series`で読みます。

```console
isuscope query latest --base BASE_RUN --metric-prefix state.dwell.total_duration --limit 20
isuscope series latest --metric state.occupancy --label kind=ride --label state=MATCHING
```

生の行は`states sql`で読みます。runの状態ログをSQLiteへ載せ（`state_events`表、attrsの値を1つずつ持つ`state_attrs`表、区間の`state_window`表、状態にいた時間の`state_stays`表と、それぞれの索引）、読み取り専用のSQLを当てます。作ったSQLiteはdata directoryの`cache/states/`にrunごとに残し（最近使った3 runまで。Gitには入れません）、同じrunの次の問い合わせはそれを開くだけです（40万行・64 MBのlogで、作るのに約4秒、開くのは一瞬）。元のlog・区間・isuscopeの版が変われば作り直します。

1つのSQLは既定で30秒で止め（`--timeout-seconds`、0で無制限。`states timeline`と`isuscope sql`も同じ）、索引を使う書き方を示します。行ごとに相関サブクエリで数え直すSQLは行数の2乗で重くなり、practice-14-seventhでは2分と5分待って手が止まりました。同じ問いは、attrsで結ぶなら`state_attrs`の索引（`key, value, kind, to_state, t`）から引き、ある状態にいる数の推移は`entered_t`で+1・`left_t`で-1した累積和（`SUM(d) OVER (ORDER BY t)`）で数えると、40万行でも0.1〜0.2秒で返ります。1秒ごとの数は`isuscope series latest --metric state.occupancy`にもあります。attrsの値で分ける集計や、問いに合わせた数え方はここで書きます。`timeline`は1つの対象の変化を時刻順に並べ、`--related`でattrsでつながる対象（対象のattrsにあるID、attrsに対象のIDを持つ行）も混ぜます。`gap_ms`は1つ前の行からの経過です。

```console
isuscope states sql --schema
isuscope states sql latest "SELECT CASE WHEN json_extract(attrs, '$.pickup_distance') > 500 THEN 'far' ELSE 'near' END AS pickup, COUNT(*) AS stays, SUM(duration_ms) AS total_ms FROM state_stays WHERE kind = 'ride' AND state = 'ENROUTE' GROUP BY 1"
# 椅子が空いた通知から、その椅子に次のライドが付くまで（state_attrsの索引で引く）
isuscope states sql latest "SELECT COUNT(*) AS n, AVG(next_t - t) / 1000.0 AS avg_ms FROM (SELECT c.t, (SELECT MIN(m.t) FROM state_attrs m WHERE m.key = 'chair' AND m.value = json_extract(c.attrs, '$.chair') AND m.kind = 'ride' AND m.to_state = 'MATCHED' AND m.t > c.t) AS next_t FROM state_events c WHERE c.kind = 'chair_notification' AND c.to_state = 'SENT' AND json_extract(c.attrs, '$.status') = 'COMPLETED') WHERE next_t IS NOT NULL"
isuscope states timeline latest --kind chair --id 01J8XYZ --around 2026-10-12T09:10:47.5+09:00 --span 200ms --related
```

記録は「今どうなっているか」で、「ここを速くすると何点増えるか」は分かりません。ロジックを変える案は実装の前に記録で前提を確かめ（合わない案は実装しない）、採否はスコアで決めます。

## 観測の考え方

標準雛形はhost sampler、sysstat、指定systemd unitのcgroup sampler、perf、Flame Graph、off-CPU、access log（`isuscope-node`）、SQL（statement digest、またはslow log）、fingerprintをnodeとphase単位で記録します。依存toolや権限がないcollector、または安全に追えないログrotationは、壊れた値を成功扱いせず`unavailable`として残します。時系列は`--window whole|initialize|load`で初期化と負荷走行を分離できます。5秒ごとに集計する値（HTTP、DB、perf、client）は負荷の始まりから区切るので、負荷の最初のbucketが落ちたりinitializeへ混ざったりしません。briefの`hosts`と`clients`は、initializeの終わりが分かるrunでは負荷区間だけで要約し、どちらで要約したかを`hosts_window`（`load`か`whole`）に出します。

alp collectorはrouteごとのcount、status、sum/avg、min/max、p50/p95/p99を保存します。標準collectorを最初から全部入れる理由と、その受け入れ基準は[標準observability](docs/standard-observability.md)にあります。

## License

[MIT](LICENSE)
