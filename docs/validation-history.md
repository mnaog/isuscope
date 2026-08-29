# 検証履歴

1回のベンチ結果を信用できる状態へ固めるために行った検証の記録です。以下の項目はすべて2026-08-28に完了しています。

## 1. Profile collectorの物理host受け入れ確認 ✅

`perf-flamegraph`と`offcpu`はtool/kernel/sudo preflight、出力形式検証、`unavailable`記録まで実装済みです。公式ISUCON環境では、FlameGraph scriptsとBCCを配置した物理hostで次を確認します。

- Rustの関数名を含むSVG symbolizationと`unknown` frameの割合
- off-CPU collectorをflush可能なsignalで終了したときの出力確定
- sampleが0件だった場合の終了状態とreport表示
- collector有効・無効時のベンチスコアとCPU負荷の差
- 正常、空sample、依存不足の実出力をfixtureとして固定

公式ISUCON13 EC2の3 app nodeで全成果物を取得し、正常・空sample・依存不足をfixture化しました。結果、symbolization、unknown率、スコア/CPU差は[`docs/profile-acceptance-isucon13.md`](docs/profile-acceptance-isucon13.md)に記録しています。off-CPUは強制終了ではflushされないため、独立process groupへ`SIGINT`を送り正常終了させる方式に確定しました。

## 2. `doctor`のprofile preflight強化 ✅

ベンチ開始前に、local/SSH先で`perf`、`stackcollapse-perf.pl`、`flamegraph.pl`、`offcputime-bpfcc`、passwordless `sudo`、kernel/BPF要件を診断します。wrapperの`sh`だけが見つかる状態を成功とせず、collectorごとに`ready`または具体的な`unavailable`理由を示します。

local/SSHのnested tool、sudo、system-wide perf、kernel/BPF probeを診断し、公式環境では22/22 checkが成功しました。readyと依存不足のE2E testも追加済みです。

## 3. ALP実出力fixtureの固定 ✅

ALP 1.0.21の実コマンドから、通常、空ログ、複数status、欠損fieldを含むJSONを採取してfixtureにします。可能なら現行ALPでも同じケースを採取し、HTTP metric contractの互換性テストに使用します。

公式ISUCON13 AMI上のALP 1.0.21から4ケースを採取し、HTTP metric contract testに固定しました。

## 4. Collector coverageのnode/source単位化 ✅

reportのcoverageをcategory全体だけで集約せず、`category × node × collector`単位で`complete`、`unavailable`、`failed`、`missing`を返します。一部nodeの成功によって別nodeの欠損が隠れないようにします。

report schema v4で`section × node × collector × phase`へ単位化し、別nodeの`failed`や期待metricの`missing`が成功nodeに隠れないtestを追加しました。raw metricの任意埋め込みを廃止し、固定compact contractにした現行schemaはv6です。

## 5. Run diff ✅

1〜4を完了した後、変更前後のrunについてHTTP route、SQL、CPU stack、host/process peak、collector欠損を同じmetric contractで比較できるdiffを追加します。CLI JSON、ローカルUI、AI向け出力は同じdiff modelを利用します。人間向けHTMLはUIだけが描画します。

`RunDiagnostics`で全件を正規化し、2 runをstable keyでJOINしてから差分上位20件へcompact化する`RunDiff` schema v1を追加しました。`isuscope diff BASE CANDIDATE`と`/api/diff`は同じJSON、`/diff`は同じmodelの人間向けHTMLを返します。片方のReportで上位20件外だった項目も比較から落ちません。

現行CPU contractは`perf report`由来のsymbol単位なので、v1もCPU symbol差分です。実stack差分を追加する場合は、SVGを逆解析せずcollectorからfolded stackを正本として保存する新しいmetric/artifact contractを先に定義します。
