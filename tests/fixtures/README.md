# Fixture provenance

- `isupipe-practice-bottleneck.json` is a minimal, anonymized subset of run `d7555a6b` from the 2026-08-26 ISUCON13 practice. It retains only the leading HTTP observations and host CPU values needed to regress candidate ordering. Cookies, request bodies, addresses, source snapshots, and raw logs are excluded.
- `sysstat-ubuntu-20.04-sysstat-12.2.0.txt` was captured with Ubuntu 20.04 package `sysstat 12.2.0-2ubuntu0.3`.
- `sysstat-ubuntu-22.04-sysstat-12.5.2.txt` was captured with Ubuntu 22.04 package `sysstat 12.5.2-2ubuntu0.2`.
- `sysstat-ubuntu-24.04-sysstat-12.6.1.txt` was captured with Ubuntu 24.04 package `sysstat 12.6.1-2`.
- `mysql-slow-8.0.46-docker.log` contains the complete slow-log records for a controlled schema/query sequence executed against the official MySQL 8.0.46 Docker image. Image initialization records were excluded because enabling `long_query_time=0` during first boot logs the entire time-zone import.

The sysstat fixtures were generated on 2026-08-27 with the official Ubuntu Docker images using `env LC_ALL=C TZ=UTC sar -u -d 1 2`. Container identifiers were replaced with stable fixture names; metric rows are otherwise preserved. Docker validates output schema and locale handling, not `perf` compatibility with an ISUCON host kernel.
