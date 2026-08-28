# TODO

UIの追加改善と本格的なdogfoodingより先に、1回のベンチ結果を信用できる状態へ固めます。

## 1. Profile collectorの物理host受け入れ確認

`perf-flamegraph`と`offcpu`はtool/kernel/sudo preflight、出力形式検証、`unavailable`記録まで実装済みです。公式ISUCON環境では、FlameGraph scriptsとBCCを配置した物理hostで次を確認します。

- Rustの関数名を含むSVG symbolizationと`unknown` frameの割合
- off-CPU collectorがSIGTERMで終了したときのflush
- sampleが0件だった場合の終了状態とreport表示
- collector有効・無効時のベンチスコアとCPU負荷の差
- 正常、空sample、依存不足の実出力をfixtureとして固定

これは機能追加ではなく、環境依存collectorの受け入れ確認です。

## 2. `doctor`のprofile preflight強化

ベンチ開始前に、local/SSH先で`perf`、`stackcollapse-perf.pl`、`flamegraph.pl`、`offcputime-bpfcc`、passwordless `sudo`、kernel/BPF要件を診断します。wrapperの`sh`だけが見つかる状態を成功とせず、collectorごとに`ready`または具体的な`unavailable`理由を示します。

## 3. ALP実出力fixtureの固定

ALP 1.0.21の実コマンドから、通常、空ログ、複数status、欠損fieldを含むJSONを採取してfixtureにします。可能なら現行ALPでも同じケースを採取し、HTTP metric contractの互換性テストに使用します。

## 4. Collector coverageのnode/source単位化

reportのcoverageをcategory全体だけで集約せず、`category × node × collector`単位で`complete`、`unavailable`、`failed`、`missing`を返します。一部nodeの成功によって別nodeの欠損が隠れないようにします。

## 5. Run diff

1〜4を完了した後、変更前後のrunについてHTTP route、SQL、CPU stack、host/process peak、collector欠損を同じmetric contractで比較できるdiffを追加します。CLI JSON、静的HTML、ローカルUI、AI向け出力は同じdiff modelを利用します。
