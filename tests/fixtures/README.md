# Fixture provenance

- `alp-json-v1.0.21*.json` was captured from `alp v1.0.21` for Linux arm64 with `alp ltsv --format json --output count,method,uri,p95 --percentiles 95`. It includes the header-only output produced for an empty input.
- `slp-tsv-v0.2.1.tsv` matches the four-column output captured from `slp v0.2.1` for Linux arm64 with `slp my --format tsv --noheaders --output count,query,sum-query-time,p95-query-time --percentiles 95`.
- `perf-script-series.txt` represents the supported `perf script --reltime --ns -F comm,time,event,ip,sym,dso` shape with the standard collector's wall-clock marker.

- `isupipe-practice-bottleneck.json` is a minimal, anonymized subset of run `e3f6c73f` from the 2026-08-27 stock ISUCON13 environment. It retains the leading HTTP, MySQL, perf, and host observations plus one shared five-second bucket. This regresses cross-source coverage and `summary-only` / `direct` / `corroborated` strength without retaining cookies, binary literals, request bodies, addresses, source snapshots, or raw logs.
- `sysstat-ubuntu-20.04-sysstat-12.2.0.txt` was captured with Ubuntu 20.04 package `sysstat 12.2.0-2ubuntu0.3`.
- `sysstat-ubuntu-22.04-sysstat-12.5.2.txt` was captured with Ubuntu 22.04 package `sysstat 12.5.2-2ubuntu0.2`.
- `sysstat-ubuntu-24.04-sysstat-12.6.1.txt` was captured with Ubuntu 24.04 package `sysstat 12.6.1-2`.
- `mysql-slow-8.0.46-docker.log` contains the complete slow-log records for a controlled schema/query sequence executed against the official MySQL 8.0.46 Docker image. Image initialization records were excluded because enabling `long_query_time=0` during first boot logs the entire time-zone import.

The sysstat fixtures were generated on 2026-08-27 with the official Ubuntu Docker images using `env LC_ALL=C TZ=UTC sar -u -d 1 2`. Container identifiers were replaced with stable fixture names; metric rows are otherwise preserved. Docker validates output schema and locale handling, not `perf` compatibility with an ISUCON host kernel.
