# 標準観測スタックの設計

## 結論

perf、alp、slp、sysstatは「必要になってから有効化する追加機能」ではなく、`run`と`discovery-run`の全runで起動を試みる標準collectorとして設定します。一方、ツール、ログ、service、DB engineが存在することは前提にしません。各collectorは開始時に観測可能性を判定し、存在しなければ終了コード75で終了します。`discovery-run`だけが追加するのは、cookieなどの匿名識別子を利用したリクエスト間の行動遷移分析です。

`edge`、`app`、`db`はマシンを固定的に分類する型ではなく、collectorの配置先を選ぶための任意のtagです。1台が複数tagを持ってよく、構成変更後のrunではtagを変更して構いません。実際に使用した設定はrunのtooling snapshotへ残るため、過去runの意味は変わりません。標準設定は複数roleの兼務を前提にし、DBの有無もcollector自身が実行時に判定します。

これにより、MySQLからPostgreSQLへ移行したrunでも、古いslp collectorを設定から削除する必要がありません。結果は`complete`、`unavailable`、`failed`の3状態で区別します。

| collector | phase | 常時取得するもの | unavailableの条件 |
|---|---|---|---|
| sysstat | during | CPU、diskのベンチ区間sample | `sar`がない |
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
modes = ["run", "discovery-run"]
command = ["slp", "my", "--file", "/tmp/isuscope-{run_id}.mysql.log", "--format", "tsv", "--noheaders", "--output", "count,query,sum-query-time,p95-query-time", "--percentiles", "95"]
parser = "slp-tsv"
unavailable_exit_codes = [75]
required = false
```

ログ全体を毎回解析するとrun同士を比較できないため、before collectorでoffset・先頭最大64 KiBのSHA-256またはDB統計snapshotを保存し、after collectorで差分だけを処理します。perfはbeforeでSSHからdetachしてPIDを保存し、afterでSIGINT、process終了、非空`perf.data`の順に確認してからreportとseriesを作ります。これによりduring collectorのprocess group終了に巻き込まれて未flushになることを防ぎます。sysstatの値は終了後の1秒ではなく、ベンチ中に出力されたsampleから作ります。

## bottleneckへ渡すmetric契約

ツール固有の生出力は圧縮ログとして保存し、それとは別に次の共通metricへ変換します。

| source | metric | 必須label |
|---|---|---|
| alp | `http.requests`, `http.request_duration` | `node`, `method`, `route`; durationは`quantile` |
| slp/pg_stat_statements | `db.query.calls`, `db.query.total_duration`, `db.query.p95_duration` | `node`, `engine`, `digest` |
| perf | `cpu.sample_percent`, `cpu.sample_count` | `node`, `process`, `symbol`, `binary` |
| sysstat | `host.cpu_percent`, `host.disk_util_percent`, `host.disk_await` | `node`; diskは`device` |

候補は単一の数式に混ぜません。HTTP、DB、CPU、host saturationの各カテゴリで候補を作り、根拠となる値とsourceを表示します。HTTPの`requests × p95`はendpoint内の順位には使えますが、CPU sampleやdisk awaitと直接比較できるスコアではありません。最大5件の表示では、まず観測できた各カテゴリの首位を1件ずつ残し、残枠をカテゴリ内の正規化値が高い候補で埋めます。表示番号はカテゴリ横断の改善優先順位を意味しません。

候補生成には`timestamp`のないrun集約だけを使い、5秒seriesを加算しません。同じ対象の時系列があれば`strength=direct`、さらに同じnode・bucketでCPU/disk高負荷があれば`corroborated`、なければ`summary-only`です。これは調査根拠の強さであって、相関から原因を断定するものではありません。coverageには関連collectorのnode、status、errorを併記し、metricがない理由を「未設定」「unavailable」「failed」から切り分けます。

`isuscope init`が生成するconfigにはsysstat、perf、alp、slp collectorが含まれます。role指定を省略して設定済みの全nodeを対象とし、`run`と`discovery-run`の両方で実行します。sysstat、alp、slpのnative出力はcollectorの`parser` adapterが上記の共通metricへ変換します。perfはrun集約のreportと5秒ごとのsymbol seriesの両方を生成します。alpとslpはログpathとformatが環境依存なので、生成された安全な既定値を実環境へ合わせます。各ツールの生出力も圧縮保存されます。

ALP adapterと行動遷移helperは同じ`routes.toml`を使います。標準設定は各正規表現をALP 1.0.21の`--matching-groups`へ解析前に渡すため、正規化route単位の正確なp95をALP自身が計算します。ALPの区切り文字と衝突するcommaをpatternへ含められず、置換後routeを一意に戻すため`replace`の`$1`などのcaptureも使用できません。該当routeは1規則ずつに分割し、固定のcanonical routeへ置換します。制約違反はcollector実行前に設定エラーとして拒否します。

`isuscope bottleneck`は上記の共通metricが存在するカテゴリを横断して候補を生成します。実環境では最初にlog path・formatを確定し、`isuscope metrics`でmetric/label一覧、`isuscope series`で該当時間帯、`bottleneck`で次の調査候補という順に確認します。

## 時系列

`host-sampler`は追加packageなしで`/proc`からCPU使用率、使用memory、load averageを1秒間隔で記録します。sysstatが利用できる環境では、CPUとdiskの各`sar` sampleもUTCの観測時刻付きで保存し、従来のrun全体平均もbottleneck判定用に残します。

行動遷移helperは正規化済みHTTP routeを5秒bucketへまとめ、request数とrequest/upstream時間のp50/p95/p99を時系列metricとして出力します。MySQL slow logはSQL literalを`?`へ正規化したdigestごとに、run集約のquery数・合計時間・p95と5秒bucketのquery数・合計時間を出します。perf scriptはprocess・binary・symbolごとのsample count/shareを5秒bucketへ変換します。bucket値には`timestamp`があり、`isuscope metrics`、filter可能な`isuscope series`またはSQLiteから参照できます。

after collectorを開始する前にbenchmarkの開始・終了時刻をrun manifestへcheckpointし、HTTP・MySQL・sysstat parserは区間外のsampleを除外します。external benchmarkでは、portalで開始する直前と終了後にEnterを押した時刻を境界として記録します。metricの`collector` labelで観測元を区別し、表のCPUは追加package不要の`host-sampler`を優先してsysstatとの二重集計を避けます。

parserの回帰テストには、sysstat 12系の24時間・AM/PM両形式、MySQL 8.0 slow log、alp 1.0.21の表形式JSON、slp 0.2.1のTSV fixtureを使用します。公式Ubuntu Docker imageから、Ubuntu 20.04のsysstat 12.2.0、22.04の12.5.2、24.04の12.6.1、およびMySQL 8.0.46の完全な出力も採取して固定しています。fixtureの由来は`tests/fixtures/README.md`に記録します。perfはDockerだけで完了扱いにせず、公式ISUCON13 AMIの3 node実走でstart/stop/report/seriesと一時ファイル消去を確認済みです。

標準log collectorは開始時のoffsetと先頭最大64 KiBのSHA-256を記録します。終了時は現在のfileと`.1`〜`.5`（各`.gz`も可）からfingerprintが一致する開始時fileを探し、そのoffset以降、中間世代、現在fileを時系列順に連結します。これによりrename、gzip、複数回rotation、copytruncateを同じ方式で扱い、世代欠落やfingerprint不一致は壊れた差分を成功扱いせず`unavailable`にします。
