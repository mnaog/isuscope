# ISUCON13 profile collector acceptance

Accepted on 2026-08-28 against the official ISUCON13 AMI (`ami-006d211cb716fe8a0`) in `ap-northeast-1`. The three application nodes were `c5.large` instances running Ubuntu 22.04.3, Linux `6.2.0-1016-aws`, and `perf 6.2.16`.

## Tooling and preflight

- FlameGraph used `stackcollapse-perf.pl` and `flamegraph.pl` from `brendangregg/FlameGraph` commit `41fee1f99f9276008b7cd112fca19dc3ea84ac32`.
- Off-CPU profiling used Ubuntu's `bpfcc-tools 0.18.0+ds-2`. Its `finish_task_switch` probe was made compatible with this kernel's `finish_task_switch.isra.0` symbol by attaching with `event_re=r"^finish_task_switch"`.
- `isuscope doctor` passed all 22 checks with no warning or failure. This included nested executable checks, passwordless sudo, a system-wide perf probe, and a one-second BPF probe on every application node.

`offcputime-bpfcc` flushes folded stacks on `SIGINT`, not on a plain forced termination. A `nohup` child inherits ignored `SIGINT`, so the collector now launches it in a new session from Python without `nohup`, sends `SIGINT` to that process group, waits for exit, and requires a non-empty output before reporting `complete`.

## Successful physical-host run

Run `01a04847-d5dd-75e0-89ca-e73907403683` (`07403683`) completed with all six profile artifacts present:

| Node | CPU flamegraph | Off-CPU folded stacks | Rust symbols | Unknown frame titles |
| --- | ---: | ---: | ---: | ---: |
| app1 | complete | 3,000 | 51 / 2,049 | 155 / 2,049 (7.56%) |
| app2 | complete | 3,035 | 31 / 2,886 | 151 / 2,886 (5.23%) |
| app3 | complete | 2,583 | 47 / 1,713 | 47 / 1,713 (2.74%) |

The Rust-symbol count includes frame titles matching `isupipe`, `isuports`, `core::`, `tokio::`, or `axum`; observed application frames include `isupipe::MemorySessionStore::get`, `isupipe::HotState::stream`, and `isupipe::StreamHotState::refresh_rendered_events`. Unknown rate is intentionally defined as unknown frame titles divided by all frame titles, rather than adding nested sample widths.

The canonical compressed artifacts are retained below `isuscope-data/practice-13/runs/01a04847-d5dd-75e0-89ca-e73907403683/logs/`. Their SHA-256 values are:

| Artifact | app1 | app2 | app3 |
| --- | --- | --- | --- |
| `perf-flamegraph` | `1c33c46214c7f3d5873a2b00f688545cad8a648d3cbfd74c22659a64e22c2c20` | `fbe0b286ae486f20047f6ebec25ccceee958f27748cd6ef181d1c076db78aa39` | `bac15b539cff16576d273cb9994e04568f7025846a1d50890b45c5d28538d6be` |
| `offcpu` | `92b5add09b321c5166ce44acaccea31e92721d44f0e19a729c899396329f5e0b` | `f6619bf821bc5d09a769d191d24f49803137d9a0aaa2d265804441995e703871` | `2007c66411a45fd8ed2b879060d6d75e060181dcfdd21986a82b29755ff49ff9` |

## Overhead comparison

The profile-disabled baseline was run `de185815` with score `1,076,728`. The accepted profile-enabled run scored `1,049,312`, a difference of `-27,416` (`-2.546%`). Average host CPU across the three nodes changed from `31.095%` to `32.611%`, or `+1.516` percentage points.

| Node | Baseline CPU average | Profile-enabled CPU average | Difference |
| --- | ---: | ---: | ---: |
| app1 | 21.329% | 26.279% | +4.950 pp |
| app2 | 45.015% | 46.786% | +1.771 pp |
| app3 | 26.942% | 24.767% | -2.175 pp |

This is a single-run acceptance comparison, not a statistically stable performance estimate.

## Empty and missing-dependency behavior

- A real `offcputime-bpfcc -f -p 999999 1` probe exited zero with a zero-byte folded output. The collector's non-empty check maps this to exit 75 and report status `unavailable`.
- An empty `perf script` conversion produced a zero-byte folded output. Flamegraph generation stops at the same non-empty check and reports `unavailable`.
- With `PATH=/usr/bin:/bin`, both `stackcollapse-perf.pl` and `offcputime-bpfcc` were absent and the dependency checks reported `unavailable`.

The normalized normal outputs and these observed statuses are fixed in `tests/fixtures/perf-flamegraph-isucon13-normalized.svg`, `tests/fixtures/offcpu-isucon13.folded`, and `tests/fixtures/profile-acceptance-isucon13.json`.
