# Fixture provenance

- `alp-json-v1.0.21*.json` was captured on 2026-08-28 from the Linux amd64 `alp v1.0.21` installed on the official ISUCON13 AMI. Controlled LTSV was passed to `alp ltsv --file /dev/stdin --format json --output count,1xx,2xx,3xx,4xx,5xx,method,uri,min,max,sum,avg,p50,p95,p99 --percentiles 50,95,99 --nosave-pos`. The fixtures preserve normal aggregation, the header-only empty output, mixed 2xx/3xx/4xx/5xx counts, and actual behavior when `method`, `uri`, or `apptime` is absent.
- `slp-tsv-v0.2.1.tsv` matches the four-column output captured from `slp v0.2.1` for Linux arm64 with `slp my --format tsv --noheaders --output count,query,sum-query-time,p95-query-time --percentiles 95`.
- `perf-script-series.txt` represents the supported `perf script --reltime --ns -F comm,time,event,ip,sym,dso` shape with the standard collector's wall-clock marker.
- `perf-flamegraph-isucon13-normalized.svg` was generated on 2026-08-28 by `perf 6.2.16`, `stackcollapse-perf.pl`, and `flamegraph.pl` on an official ISUCON13 EC2 host. Interactive JavaScript, presentation rectangles, and text were removed while retaining the complete SVG envelope and every captured frame title.
- `offcpu-isucon13.folded` is the complete one-second output from `offcputime-bpfcc -f 1` on the same host. `profile-acceptance-isucon13.json` fixes the observed normal, zero-sample, and dependency-missing statuses used by the artifact contract test.

- `sysstat-ubuntu-20.04-sysstat-12.2.0.txt` was captured with Ubuntu 20.04 package `sysstat 12.2.0-2ubuntu0.3`.
- `sysstat-ubuntu-22.04-sysstat-12.5.2.txt` was captured with Ubuntu 22.04 package `sysstat 12.5.2-2ubuntu0.2`.
- `sysstat-ubuntu-24.04-sysstat-12.6.1.txt` was captured with Ubuntu 24.04 package `sysstat 12.6.1-2`.
- `mysql-slow-8.0.46-docker.log` contains the complete slow-log records for a controlled schema/query sequence executed against the official MySQL 8.0.46 Docker image. Image initialization records were excluded because enabling `long_query_time=0` during first boot logs the entire time-zone import.

The sysstat fixtures were generated on 2026-08-27 with the official Ubuntu Docker images using `env LC_ALL=C TZ=UTC sar -u -d 1 2`. Container identifiers were replaced with stable fixture names; metric rows are otherwise preserved. Docker validates output schema and locale handling, not `perf` compatibility with an ISUCON host kernel.
