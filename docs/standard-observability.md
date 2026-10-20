# 標準観測スタックの設計

## 結論

perf、alp、slp、sysstatは「必要になってから有効化する追加機能」ではなく、`run`と`survey-run`の全runで起動を試みる標準collectorとして設定します。一方、ツール、ログ、service、DB engineが存在することは前提にしません。各collectorは開始時に観測可能性を判定し、存在しなければ終了コード75で終了します。`survey-run`だけが追加するのは、cookieなどの匿名識別子を利用したリクエスト間の行動遷移分析と、最初の走行で必要な場合だけ有効化する通常HTTPのrequest/response body captureです。

`edge`、`app`、`db`はマシンを固定的に分類する型ではなく、collectorの配置先を選ぶための任意のtagです。1台が複数tagを持ってよく、構成変更後のrunではtagを変更して構いません。実際に使用した設定はrunのtooling snapshotへ残るため、過去runの意味は変わりません。標準設定は複数roleの兼務を前提にし、DBの有無もcollector自身が実行時に判定します。

これにより、MySQLからPostgreSQLへ移行したrunでも、古いslp collectorを設定から削除する必要がありません。結果は`complete`、`unavailable`、`failed`の3状態で区別します。

| collector | phase | 常時取得するもの | unavailableの条件 |
|---|---|---|---|
| sysstat | during | CPU内訳、disk IOPS・帯域・queue・latency・utilのベンチ区間sample | `sar`がない |
| service-sampler | during | 指定systemd unitのCPU、memory、disk I/O、PID数 | unit未指定、cgroup v2でない、unitが停止中 |
| process-sampler | during | 指定systemd unitの全process（cgroup v2・v1、見つからなければMainPID）のRSS合計・thread数・process数を/procから1秒ごと。MainPIDが変わったら再起動として数える。RSSはcgroupの`memory.current`（page cacheを含む）とは別の値 | unit未指定、`systemctl`か`/proc`がない |
| perf | before/after | detachしたsystem-wide sampleとhot symbol。perf.dataは`perf-series`の1回の`perf script`で読み、ベンチの区間`[始まり, 終わり)`のsampleだけから、区間全体と5秒ごとのsymbol別の割合を作る（perfはafter phaseまで止まらないので、ベンチの後のsampleは捨てる） | `perf`がない、権限不足、kernelが非対応 |
| alp | after | access logの差分をnode上で集計する。route別のrequest数・status・合計・平均・min/max・p50/p95/p99・error・bytes（差分全体、alp）、route別の5秒ごとのrequest数・error・p50/p95/p99（ベンチ区間、5秒ごとのfileをalp）、ベンチ側の接続の使い方とupstream別の試行数・失敗して次へ回された試行・connect/header/response時間（試行単位、ベンチ区間、awk）。nodeに同じbuildの`isuscope-node`があれば、これらと同じ出力を1回の読み取りで作る（下の「node上の集計binary」）。生のlogは運ばない | access logがない（`isuscope-node`もalpも無い、行はあるのにmethod/uriを1件も読めない場合は`failed`） |
| limits | before | 上限値を読むだけ（somaxconn、nginxのworker_connections・worker_rlimit_nofile・http側のkeepalive_requests・worker数、`service_units`の`LimitNOFILE`、MySQLの`max_connections`）。briefの`limits`が観測と並べて、当たった上限を設定の名前で出す | 読めない値は出さない |
| mysql-digest-start・mysql-digest-sampler・mysql-digest | before・during・after | performance_schemaのstatement digest表をベンチの前に空にし、ベンチの間は1秒ごとに累積値をnodeの一時fileへ写し、ベンチの後に負荷の始まり以前の最後の写しでinitializeと負荷の区間へ分ける。出力はslpと同じ形（文ごとの回数・合計・lock・rows、DB全体の5秒ごとの回数・時間）で、最大・p95・p99はベンチ全体の分布。slow logを書かないのでベンチ中のDBの負荷にならず、後処理も数秒で済む。両方あればbriefはこちらを使う | performance_schemaかstatement digestのconsumerが無効（MariaDBの既定など。そのときはslow logを使う） |
| slp | after | slow logの差分をnode上でinitializeと負荷区間に分け、文ごとの回数・合計・最大・p95・p99・lock・rowsと、DB全体の5秒ごとの回数・時間 | MySQL slow logがない、MySQLが退役済み（slpが無いのは`failed`。Ansibleのobservability roleが入れる） |
| app-log | before/after | `service_units`のjournal、nginxのerror log（access logと同じdirectoryの`error.log`）、kernelのwarning以上を、beforeで記録した位置（journalはcursor、error logは大きさ）より後だけnode上で読む。行の数字を含む語を`<N>`にした型ごとに数え、多い順に10型と1行の実例だけを持ち帰る。appのunitはerror・timeout・失敗などの語を含む行だけを数える。journaldは1 unitあたり既定で30秒に10,000行までしか残さず、捨てた通知は区間が明けて次の行が来たときに出るので、捨てた行数は数えない（briefは1万行以上読んだunitに、件数が少ない可能性を警告する） | journalもerror logも読めない |
| state-log | before/after | アプリが状態の変化を1行ずつ書くfile（既定`/var/tmp/isuscope-state.jsonl`）を、beforeで記録した位置（device・inode・大きさ）より後だけ運ぶ。作り直されたfileは先頭から読み、1 runで256MiBを超えた分は運ばずに大きさを残す。生の行はrunのlogに残し、`isuscope states sql`・`states timeline`がSQLiteへ載せて読む（runごとに残す） | アプリがまだ書いていない（file が無い） |
| network-allowance | before/after | EC2のENAが上限を超えて捨てた・遅らせたpacketの数（`ethtool -S`の`bw_in`・`bw_out`・`pps`・`conntrack`・`linklocal`の`*_allowance_exceeded`）の前後の差と、後の`conntrack_allowance_available`。initializeを含む。briefは増えたcounterを`warnings`に出す | `ethtool`がない、どのdeviceもallowance counterを持たない（ENA以外） |
| mysql-io | before/after | Performance Schemaのfile別（`file_summary_by_instance`）とtable別（`table_io_waits_summary_by_table`）の累積値を前後に1回ずつ読んだ差。あわせてbuffer poolの大きさとtableのdata・indexの大きさ。ベンチ中は何もしない | MySQLがない、Performance Schemaが無効、間でmysqldが再起動した |
| PostgreSQL | after | `pg_stat_statements`のquery別差分 | PostgreSQLがない、extensionが無効 |

権限不足は環境上の意図した制約なら`unavailable`、設定ミスや途中で壊れた場合は`failed`にします。単に`required = false`にするだけでは失敗と不在を区別できないため、標準collectorは明示的に75を返します。

## collectorの形

設定には各候補を常に残し、roleは配置先、実行commandは実在性を判断する責務を持ちます。native JSONやsysstat textは`parser` adapterを指定して共通metricへ変換します。

```toml
[[collectors]]
name = "sysstat"
phase = "during"
transport = "ssh"
command = ["sh", "-c", "command -v sar >/dev/null 2>&1 || exit 75; exec env LC_ALL=C TZ=UTC sar -u -d 1"]
parser = "sysstat"
unavailable_exit_codes = [75]
```

ログ全体を毎回解析するとrun同士を比較できないため、before collectorでoffset・先頭最大64 KiBのSHA-256またはDB統計snapshotを保存し、after collectorで差分だけを処理します。perfはbeforeでSSHからdetachしてPIDを保存し、afterでSIGINT、process終了、非空`perf.data`の順に確認してからreportとseriesを作ります。これによりduring collectorのprocess group終了に巻き込まれて未flushになることを防ぎます。sysstatの値は終了後の1秒ではなく、ベンチ中に出力されたsampleから作ります。

## 共通metric契約

ツール固有の生出力は圧縮ログとして保存し、それとは別に次の共通metricへ変換します。

| source | metric | 必須label |
|---|---|---|
| alp（route別） | `http.requests`, `http.errors`, `http.request_duration_sum`, `http.request_duration_mean`, `http.request_duration_min`, `http.request_duration`, `http.request_duration_max`, `http.response_bytes`（差分全体）と、`http.requests`, `http.errors`, `http.request_duration`の5秒bucket。時間は`$request_time`（alpへ`--apptime-label reqtime`を渡す） | `node`, `method`, `route`; status別requestsは`status_class`、percentileは`quantile` |
| alp（接続・upstream） | `client.connections_in_use`, `client.connections_opened`, `client.request_gap`（5秒bucketのp50/p95/p99）、`client.connection_requests_mean`/`_max`, `client.connections_opened_total`, `http.upstream_requests`, `http.upstream_retried_requests`, `http.upstream_{connect,header,response}_duration`（sum/mean/min/maxとp50/p95/p99）、`http.access_log_bytes`（差分の大きさ、nginx-log-delta）、`http.access_log_skipped`（methodとuriはあるのにalpが飛ばした行の数。壊れたescapeのURIなど。全部飛ばされたらcollectorを失敗にする） | `node`; upstreamは`upstream` |
| mysql-digest（区間別・DB全体） | slp（区間別）と同じ名前と`db.calls`・`db.duration`・`db.lock_duration`、写しの数の`db.digest_snapshots` | slp（区間別）と同じ。`digest`はperformance_schemaの`DIGEST_TEXT`（値は`?`） |
| slp（区間別） | `db.query.calls`, `db.query.total_duration`, `db.query.duration_max`, `db.query.p95_duration`, `db.query.p99_duration`, `db.query.lock_duration`, `db.query.rows_sent`, `db.query.rows_examined` | `node`, `engine`, `digest`, `window`（`initialize`、`load`、区間が分からなければ`whole`）、`digest_id`（1,024 byteを超えて切った文だけ。切る前の全文のSHA-256先頭16桁で、集計と比較の識別に使う） |
| slp（DB全体） | `db.calls`, `db.duration`, `db.lock_duration`（5秒bucket）、`db.slow_log_bytes`（差分の大きさ）、`db.slow_log_unclassified`（`SET timestamp=`が無く区間へ振り分けられなかった文の数。0件なら出さない）、`db.slow_log_coarse_time`（`# Time:`が無く、開始時刻を秒までで振り分けた文の数。0件なら出さない） | `node`, `engine` |
| app-log | `log.error_lines`（型ごとの行数）、`log.lines`（読んだ行数）、`log.error_lines_omitted`（上位10型の外の行数） | `node`, `source`（unit名、`nginx-error`、`kernel`）; `log.error_lines`は`pattern`と`example` |
| state-log | `state.dwell.{count,total_duration,p50_duration,p95_duration,p99_duration,max_duration}`（ms）、`state.dwell.open`、`state.transitions`、`state.occupancy_max`、`state.occupancy`（1秒ごと）、`state.lines`・`state.invalid_lines`・`state.dropped`・`state.inconsistent`、`state.read_bytes`・`state.truncated_bytes` | `node`, `kind`, `state`（遷移は`from`・`to`）、`window`（`load`か`whole`。1秒ごとの値には付かない） |
| mysql-io | `db.file.{read,write}_wait`（ms）、`db.file.misc_wait`（ms、fsync・open・close・statなど読み書き以外の操作の合計。fsyncだけの時間ではない）、`db.file.{read,write}_bytes`、`db.table.{read,write}_time`（ms）、`db.table.rows_{read,written}`、`db.table.size`、`db.memory.buffer_pool`、`db.memory.tables` | `node`, `engine`; fileは`file`（datadirからの相対path）、tableは`table`（`schema.table`） |
| pg_stat_statements | `db.query.calls`, `db.query.total_duration`, `db.query.p95_duration`, `db.query.lock_duration`, `db.query.rows_sent`, `db.query.rows_examined` | `node`, `engine`, `digest` |
| perf-series | `cpu.sample_percent`, `cpu.sample_count`（5秒bucketと取得全体。全体は`perf report --no-children --sort comm,dso,symbol`の自己時間と同じ切り方で、sample数で数える） | `node`, `process`, `symbol`, `binary` |
| perf-series | `cpu.process_percent`（5秒bucketと取得全体） | `node`, `process` |
| sysstat | `host.cpu_busy_percent`, `host.cpu_{user,system,iowait,steal,idle}_percent`, `host.disk_{iops,read_bytes_per_second,write_bytes_per_second,queue_depth,await,util_percent}` | `node`; diskは`device` |
| service-sampler | `service.cpu_cores`, `service.memory_bytes`, `service.io_{read,write}_bytes_per_second`, `service.pids` | `node`, `service` |
| process-sampler | `service.process_rss_bytes`, `service.process_threads`, `service.processes`, `service.process_restarts`（MainPIDが変わった秒に1） | `node`, `service` |
| network-allowance | `net.allowance_exceeded`（packets、前後の差）、`net.conntrack_allowance_available` | `node`, `device`; 前者は`counter`（`bw_out_allowance_exceeded`など） |

`isuscope init`が生成するconfigにはhost-sampler、sysstat、service-sampler、perf、alp、slp、optionalなFlame Graph/off-CPU collectorが含まれます。role指定を省略して設定済みの全nodeを対象とし、`run`と`survey-run`の両方で実行します。service-samplerは`[observability].service_units`に列挙した少数のunitだけを対象にし、cgroupの累積counterをparserでrateへ変換します。sysstat、service-sampler、alp、slp、perf scriptのnative出力はcollectorの`parser` adapterが上記の共通metricへ変換します。perfは`perf record -g`でcall graphを採取し、`stackcollapse-perf.pl`と`flamegraph.pl`があればSVGを生成して完全なSVG documentか検証します。`offcputime-bpfcc`と権限があればbeforeでSSHからdetachし、afterでprocess groupへSIGINTを送り、終了と非空出力を確認してからfolded off-CPU stackを回収します。各非空行はstack/count形式か検証します。ツールやkernel capabilityがない場合とsampleが0件の場合は終了コード75の`unavailable`であり、runをdegradedにしません。各ツールの生出力も圧縮保存され、直近runのSVG/folded stackは`latest/logs`へ直接展開されます。

ALP adapterと行動遷移helperは同じ`routes.toml`を使います。標準設定は各正規表現をALP 1.0.21の`--matching-groups`へ解析前に渡すため、正規化route単位のcount、status class、min/max/sum/avg、p50/p95/p99をALP自身が集計します。adapterはこれらを単位付きmetricへ変換し、`brief`と`query --view http`がrouteごとの表としてtotal時間順に返します。ALPの区切り文字と衝突するcommaをpatternへ含められず、置換後routeを一意に戻すため`replace`の`$1`などのcaptureも使用できません。該当routeは1規則ずつに分割し、固定のcanonical routeへ置換します。制約違反はcollector実行前に設定エラーとして拒否します。

実環境では最初にlog path・formatを確定し、`isuscope brief`でrun全体を確認してから、`isuscope sql`で名前、`isuscope query`でrun集約値、`isuscope series`で時間帯を絞り込みます。

## 時系列

`host-sampler`は追加packageなしで`/proc`からCPU内訳、使用memory、load averageを1秒間隔で記録します。busyは`100 - idle - iowait`と定義し、互換名`host.cpu_percent`も同じ値です。sysstatが利用できる環境では、CPUとdiskの各`sar` sampleもUTCの観測時刻付きで保存し、run全体平均も比較用に残します。既に`sar -d`が出力している値をparserで保持するため、disk詳細の追加によるremote側のsampling処理は増えません。

初期化と負荷走行を混ぜずに見る場合は`whole`、`initialize`、`load`の名前付きwindowを使います。`query --scope series`はwindow内のgauge/rateを平均し、counterを合計します。`series`は同じwindow内をさらにbucket化します。

```bash
isuscope query latest --scope series --window load --metric-prefix service. --group-by node --group-by service
isuscope series latest --window initialize --metric host.cpu_iowait_percent --bucket 1
```

HTTPのroute別5秒bucketは、alp collectorがベンチ区間を5秒ごとのfileに分けてalpで集計したrequest数・error・p50/p95/p99です（`$msec`で振り分ける）。行動遷移helperは`survey-run`で持ち帰った生のaccess logからsession単位の遷移だけを作ります。MySQL slow logはslpがnode上でinitializeと負荷区間に分けて文ごとに集計し、SQL別の値は区間ごとの集約だけです（SQL別の5秒時系列はありません）。5秒bucketはDB全体の`db.calls`・`db.duration`・`db.lock_duration`です。perf scriptはprocess・binary・symbolごとのsample count/shareを5秒bucketへ変換します。5秒bucketはどれも負荷の始まり（initializeの終わり。分からなければベンチの始まり）から5秒ずつ区切り、`timestamp`はbucketの先頭です。ベンチの始まりをまたぐbucketは始まりを先頭にした短いbucketにし、同時接続数のような率はbucketの実際の長さで割ります。node上でも区間は`[始まり, 終わり)`で、終わりちょうどの観測は入れません。epochの5の倍数で区切ると、負荷の始まりをまたぐbucketができ、`--window load`で絞ったときに負荷の最初の数秒（接続が急増しやすいところ）が丸ごと落ちていました。区間は`[始まり, 終わり)`で絞るので、負荷の最初のbucketはinitializeには入りません。`series`の`range.edges`は、区間の端がbucketの区切りかnode上の境界（ベンチの始まりと終わり）なら`exact`、端をまたぐbucketがあれば`approximate`です（initializeの始まりや、`--from/--to`で切った区間）。bucket値には`timestamp`があり、filter可能な`isuscope series`、`isuscope query`、`isuscope sql`から参照できます。

after collectorを開始する前にbenchmarkの開始・終了時刻をrun manifestへcheckpointし、HTTP・MySQL・sysstat parserは区間外のsampleを除外します。external benchmarkでは、portalで開始する直前と終了後にEnterを押した時刻を境界として記録します。metricの`collector` labelで観測元を区別し、表のCPUは追加package不要の`host-sampler`を優先してsysstatとの二重集計を避けます。

### node上の集計binary（isuscope-node）

alp collectorの集計は、awkで差分を5秒ごとのfileへ分け、upstreamと接続の値をsortで並べ、差分全体とfileごとにalpを走らせる形だと、2コアのnodeで62万行に約20秒かかり、行数に比例して伸びました（practice-14-seventhの58 runで計測後の後処理27分のほぼ全部、最大は322万行で87秒）。`isuscope-node`はこの集計を1回の読み取り（alpの表と、接続・upstreamの値を2本のthreadで）に置き換えたbinaryで、同じ62万行を約1.2秒で集計します。

- 出力はawkとalp 1.0.21の組み合わせと同じです。`alp-windows-v1.0.21.out`のfixture、崩れた行（`:`の無い項目、途中の空の項目、壊れた`%`escape、escapeが要る文字、`,`を含むURI、`-`の時間、範囲外のstatus、区間の外の行）を混ぜた2万行、62万行の合成logで、metricと表がすべて一致することを確かめています。alpの読み方（go-ltsv、Goの`url.Parse`と`String`、最小値の上書き、`int((n+1)*p)-1`番目の分位点、小数3桁の丸め、5,000種類を超えるURIでの失敗）も写しています。同時接続数だけは、接続ごとの足し算の順がawkのhashの順と違うため、最後の桁が違い得ます。
- 集計のコードは`src/access_summary.rs`で、`node/`のcrateがそれを取り込んでbuildします。依存はregexだけなので、muslの静的binaryとして手元からcross buildできます（`cargo build --release -p isuscope-node --target x86_64-unknown-linux-musl`。linkerは`rust-lld`）。nodeのOSやglibcに依存しないので、問題の公開前に作れます。
- `isuscope-node --build-id`は`src/access_summary.rs`の内容から作った値です。collectorは手元のisuscopeの値（`{isuscope_node_build}`）と照合し、違えばawkとalpで集計します（stderrに書く）。`isuscope doctor`のpreflightは、buildが違えば失敗、`isuscope-node`が無ければ警告にします。
- 手元では`isuscope __access-summary access-summary --input <差分> ...`で同じ集計を試せます。

parserの回帰テストには、sysstat 12系の24時間・AM/PM両形式、MySQL 8.0 slow log、alp 1.0.21の表形式JSONとalp collectorの出力（mawk・gawkで同一）、slp 0.2.1のTSV fixtureを使用します。公式Ubuntu Docker imageから、Ubuntu 20.04のsysstat 12.2.0、22.04の12.5.2、24.04の12.6.1、およびMySQL 8.0.46の完全な出力も採取して固定しています。fixtureの由来は`tests/fixtures/README.md`に記録します。perfはDockerだけで完了扱いにせず、公式ISUCON13 AMIの3 node実走でstart/stop/report/seriesと一時ファイル消去（reportは現在perf-seriesに統合）を確認済みです。

標準log collectorは開始時のoffsetと先頭最大64 KiBのSHA-256を記録します。終了時は現在のfileと`.1`〜`.5`（各`.gz`も可）からfingerprintが一致する開始時fileを探し、そのoffset以降、中間世代、現在fileを時系列順に連結します。これによりrename、gzip、複数回rotation、copytruncateを同じ方式で扱い、世代欠落やfingerprint不一致は壊れた差分を成功扱いせず`unavailable`にします。
