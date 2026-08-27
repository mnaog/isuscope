# 標準観測スタックの設計

## 結論

perf、alp、slp、sysstatは「必要になってから有効化する追加機能」ではなく、`run`と`discovery-run`の全runで起動を試みる標準collectorとして設定します。一方、ツール、ログ、service、DB engineが存在することは前提にしません。各collectorは開始時に観測可能性を判定し、存在しなければ終了コード75で終了します。`discovery-run`だけが追加するのは、cookieなどの匿名識別子を利用したリクエスト間の行動遷移分析です。

`edge`、`app`、`db`はマシンを固定的に分類する型ではなく、collectorの配置先を選ぶための任意のtagです。1台が複数tagを持ってよく、構成変更後のrunではtagを変更して構いません。実際に使用した設定はrunのtooling snapshotへ残るため、過去runの意味は変わりません。標準設定は複数roleの兼務を前提にし、DBの有無もcollector自身が実行時に判定します。

これにより、MySQLからPostgreSQLへ移行したrunでも、古いslp collectorを設定から削除する必要がありません。結果は`complete`、`unavailable`、`failed`の3状態で区別します。

| collector | phase | 常時取得するもの | unavailableの条件 |
|---|---|---|---|
| sysstat | during | CPU、diskのベンチ区間sample | `sar`がない |
| perf | during | system-wide sampleとhot symbol | `perf`がない、権限不足、kernelが非対応 |
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
command = ["slp", "--format", "json", "/tmp/isuscope-{run_id}.slp.log"]
parser = "slp-json"
unavailable_exit_codes = [75]
required = false
```

ログ全体を毎回解析するとrun同士を比較できないため、before collectorでinode・offsetまたはDB統計snapshotを保存し、after collectorで差分だけを処理します。perfとsysstatはduring collectorとしてベンチと同時に起動し、isuscopeがベンチ終了時に停止します。sysstatの値は終了後の1秒ではなく、ベンチ中に出力されたsampleから作ります。

## bottleneckへ渡すmetric契約

ツール固有の生出力は圧縮ログとして保存し、それとは別に次の共通metricへ変換します。

| source | metric | 必須label |
|---|---|---|
| alp | `http.requests`, `http.request_duration` | `node`, `method`, `route`; durationは`quantile` |
| slp/pg_stat_statements | `db.query.calls`, `db.query.total_duration` | `node`, `engine`, `digest` |
| perf | `cpu.sample_percent` | `node`, `symbol`, `binary` |
| sysstat | `host.cpu_percent`, `host.disk_util_percent`, `host.disk_await` | `node`; diskは`device` |

候補は単一の数式に混ぜません。HTTP、DB、CPU、host saturationの各カテゴリで候補を作り、根拠となる値とsourceを表示します。HTTPの`requests × p95`はendpoint内の順位には使えますが、CPU sampleやdisk awaitと直接比較できるスコアではありません。最大5件の表示では、まず観測できた各カテゴリの首位を1件ずつ残し、残枠をカテゴリ内の正規化値が高い候補で埋めます。表示番号はカテゴリ横断の改善優先順位を意味しません。未観測カテゴリも`unavailable`として明示します。改善によって支配的な待ち時間がHTTP、DB、CPU、diskの間を移るため、この結果は原因の断定ではなく、runごとに更新される次の調査対象です。

`isuscope init`が生成するconfigにはsysstat、perf、alp、slp collectorが含まれます。role指定を省略して設定済みの全nodeを対象とし、`run`と`discovery-run`の両方で実行します。sysstat、alp、slpのnative出力はcollectorの`parser` adapterが上記の共通metricへ変換し、perf reportはJSON Linesを出力します。alpとslpはログpathとformatが環境依存なので、生成された安全な既定値を実環境へ合わせます。各ツールの生出力も圧縮保存されます。

ALP adapterと行動遷移helperは同じ`routes.toml`を使います。ALPが正規化前のURIを複数recordへ分けて返した場合、adapterはrequest数を合計し、再計算できないp95は候補を過小評価しないよう最大値を採用します。

`isuscope bottleneck`は上記の共通metricが存在するカテゴリを横断して候補を生成します。残る拡張順は以下です。

1. alp/slpの問題固有log formatとpathをSETUP時に確定する。
2. 各カテゴリのcoverageが実際のcollector状態と一致することを検証する。
3. 複数sourceが同じ待ち時間を裏付ける場合に、その関連をevidenceへ追加する。
4. 同じtooling fingerprintを持つrun間で候補の増減を比較する。

この順序なら、推測だけの総合点を先に作らず、観測データと欠測状態を正しく蓄積してからランキングを拡張できます。
