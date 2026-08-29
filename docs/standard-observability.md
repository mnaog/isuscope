# 標準観測スタックの設計

## 結論

perf、alp、slp、sysstatは「必要になってから有効化する追加機能」ではなく、`run`と`survey-run`の全runで起動を試みる標準collectorとして設定します。一方、ツール、ログ、service、DB engineが存在することは前提にしません。各collectorは開始時に観測可能性を判定し、存在しなければ終了コード75で終了します。`survey-run`だけが追加するのは、cookieなどの匿名識別子を利用したリクエスト間の行動遷移分析と、最初の走行で必要な場合だけ有効化する通常HTTPのrequest/response body captureです。

`edge`、`app`、`db`はマシンを固定的に分類する型ではなく、collectorの配置先を選ぶための任意のtagです。1台が複数tagを持ってよく、構成変更後のrunではtagを変更して構いません。実際に使用した設定はrunのtooling snapshotへ残るため、過去runの意味は変わりません。標準設定は複数roleの兼務を前提にし、DBの有無もcollector自身が実行時に判定します。

これにより、MySQLからPostgreSQLへ移行したrunでも、古いslp collectorを設定から削除する必要がありません。結果は`complete`、`unavailable`、`failed`の3状態で区別します。

| collector | phase | 常時取得するもの | unavailableの条件 |
|---|---|---|---|
| sysstat | during | CPU内訳、disk IOPS・帯域・queue・latency・utilのベンチ区間sample | `sar`がない |
| service-sampler | during | 指定systemd unitのCPU、memory、disk I/O、PID数 | unit未指定、cgroup v2でない、unitが停止中 |
| perf | before/after | detachしたsystem-wide sampleとhot symbol | `perf`がない、権限不足、kernelが非対応 |
| alp | after | route別request数、p50/p95/p99、error、bytes | access logがない、alpがない |
| slp | after | digest別query数、合計時間、p95、rows | MySQL slow logがない、slpがない、MySQLが退役済み |
| PostgreSQL | after | `pg_stat_statements`のquery別差分 | PostgreSQLがない、extensionが無効 |

権限不足は環境上の意図した制約なら`unavailable`、設定ミスや途中で壊れた場合は`failed`にします。単に`required = false`にするだけでは失敗と不在を区別できないため、標準collectorは明示的に75を返します。

## collectorの形

設定には各候補を常に残し、roleは配置先、実行commandは実在性を判断する責務を持ちます。native JSONやsysstat textは`parser` adapterを指定して共通metricへ変換します。

```toml
[[collectors]]
name = "mysql-slow-query"
phase = "after"
transport = "ssh"
roles = ["db"]
modes = ["run", "survey-run"]
command = ["slp", "my", "--file", "/tmp/isuscope-{run_id}.mysql.log", "--format", "tsv", "--noheaders", "--output", "count,query,sum-query-time,p95-query-time", "--percentiles", "95"]
parser = "slp-tsv"
unavailable_exit_codes = [75]
required = false
```

ログ全体を毎回解析するとrun同士を比較できないため、before collectorでoffset・先頭最大64 KiBのSHA-256またはDB統計snapshotを保存し、after collectorで差分だけを処理します。perfはbeforeでSSHからdetachしてPIDを保存し、afterでSIGINT、process終了、非空`perf.data`の順に確認してからreportとseriesを作ります。これによりduring collectorのprocess group終了に巻き込まれて未flushになることを防ぎます。sysstatの値は終了後の1秒ではなく、ベンチ中に出力されたsampleから作ります。

## 共通metric契約

ツール固有の生出力は圧縮ログとして保存し、それとは別に次の共通metricへ変換します。

| source | metric | 必須label |
|---|---|---|
| alp | `http.requests`, `http.errors`, `http.request_duration_sum`, `http.request_duration_mean`, `http.request_duration_min`, `http.request_duration`, `http.request_duration_max`, `http.response_bytes` | `node`, `method`, `route`; status別requestsは`status_class`、percentileは`quantile` |
| slp/pg_stat_statements | `db.query.calls`, `db.query.total_duration`, `db.query.p95_duration`, `db.query.lock_duration`, `db.query.rows_sent`, `db.query.rows_examined` | `node`, `engine`, `digest` |
| perf | `cpu.sample_percent`, `cpu.sample_count` | `node`, `process`, `symbol`, `binary` |
| sysstat | `host.cpu_busy_percent`, `host.cpu_{user,system,iowait,steal,idle}_percent`, `host.disk_{iops,read_bytes_per_second,write_bytes_per_second,queue_depth,await,util_percent}` | `node`; diskは`device` |
| service-sampler | `service.cpu_cores`, `service.memory_bytes`, `service.io_{read,write}_bytes_per_second`, `service.pids` | `node`, `service` |

`isuscope init`が生成するconfigにはhost-sampler、sysstat、service-sampler、perf、alp、slp、optionalなFlame Graph/off-CPU collectorが含まれます。role指定を省略して設定済みの全nodeを対象とし、`run`と`survey-run`の両方で実行します。service-samplerは`[observability].service_units`に列挙した少数のunitだけを対象にし、cgroupの累積counterをparserでrateへ変換します。sysstat、service-sampler、alp、slpのnative出力はcollectorの`parser` adapterが上記の共通metricへ変換します。perfは`perf record -g`でcall graphを採取し、`stackcollapse-perf.pl`と`flamegraph.pl`があればSVGを生成して完全なSVG documentか検証します。`offcputime-bpfcc`と権限があればbeforeでSSHからdetachし、afterでprocess groupへSIGINTを送り、終了と非空出力を確認してからfolded off-CPU stackを回収します。各非空行はstack/count形式か検証します。ツールやkernel capabilityがない場合とsampleが0件の場合は終了コード75の`unavailable`であり、runをdegradedにしません。各ツールの生出力も圧縮保存され、直近runのSVG/folded stackは`latest/logs`へ直接展開されます。

ALP adapterと行動遷移helperは同じ`routes.toml`を使います。標準設定は各正規表現をALP 1.0.21の`--matching-groups`へ解析前に渡すため、正規化route単位のcount、status class、min/max/sum/avg、p50/p95/p99をALP自身が集計します。adapterはこれらを単位付きmetricへ変換し、`report`はrouteごとのHTTP表としてtotal時間順に返します。ALPの区切り文字と衝突するcommaをpatternへ含められず、置換後routeを一意に戻すため`replace`の`$1`などのcaptureも使用できません。該当routeは1規則ずつに分割し、固定のcanonical routeへ置換します。制約違反はcollector実行前に設定エラーとして拒否します。

実環境では最初にlog path・formatを確定し、`isuscope report`の統合JSONでrun全体を確認してから、`isuscope metrics`で名前、`isuscope query`でrun集約値、`isuscope series`で時間帯を絞り込みます。

## 時系列

`host-sampler`は追加packageなしで`/proc`からCPU内訳、使用memory、load averageを1秒間隔で記録します。busyは`100 - idle - iowait`と定義し、互換名`host.cpu_percent`も同じ値です。sysstatが利用できる環境では、CPUとdiskの各`sar` sampleもUTCの観測時刻付きで保存し、run全体平均も比較用に残します。既に`sar -d`が出力している値をparserで保持するため、disk詳細の追加によるremote側のsampling処理は増えません。

初期化と負荷走行を混ぜずに見る場合は`whole`、`initialize`、`load`の名前付きwindowを使います。`query --scope series`はwindow内のgauge/rateを平均し、counterを合計します。`series`は同じwindow内をさらにbucket化します。

```bash
isuscope query latest --scope series --window load --metric-prefix service. --group-by node --group-by service
isuscope series latest --window initialize --metric host.cpu_iowait_percent --bucket 1
```

行動遷移helperは正規化済みHTTP routeを5秒bucketへまとめ、request数とrequest/upstream時間のp50/p95/p99を時系列metricとして出力します。MySQL slow logはSQL literalを`?`へ正規化したdigestごとに、run集約のquery数・合計時間・p95と5秒bucketのquery数・合計時間を出します。perf scriptはprocess・binary・symbolごとのsample count/shareを5秒bucketへ変換します。bucket値には`timestamp`があり、`isuscope metrics`、filter可能な`isuscope series`またはSQLiteから参照できます。

after collectorを開始する前にbenchmarkの開始・終了時刻をrun manifestへcheckpointし、HTTP・MySQL・sysstat parserは区間外のsampleを除外します。external benchmarkでは、portalで開始する直前と終了後にEnterを押した時刻を境界として記録します。metricの`collector` labelで観測元を区別し、表のCPUは追加package不要の`host-sampler`を優先してsysstatとの二重集計を避けます。

parserの回帰テストには、sysstat 12系の24時間・AM/PM両形式、MySQL 8.0 slow log、alp 1.0.21の表形式JSON、slp 0.2.1のTSV fixtureを使用します。公式Ubuntu Docker imageから、Ubuntu 20.04のsysstat 12.2.0、22.04の12.5.2、24.04の12.6.1、およびMySQL 8.0.46の完全な出力も採取して固定しています。fixtureの由来は`tests/fixtures/README.md`に記録します。perfはDockerだけで完了扱いにせず、公式ISUCON13 AMIの3 node実走でstart/stop/report/seriesと一時ファイル消去を確認済みです。

標準log collectorは開始時のoffsetと先頭最大64 KiBのSHA-256を記録します。終了時は現在のfileと`.1`〜`.5`（各`.gz`も可）からfingerprintが一致する開始時fileを探し、そのoffset以降、中間世代、現在fileを時系列順に連結します。これによりrename、gzip、複数回rotation、copytruncateを同じ方式で扱い、世代欠落やfingerprint不一致は壊れた差分を成功扱いせず`unavailable`にします。
