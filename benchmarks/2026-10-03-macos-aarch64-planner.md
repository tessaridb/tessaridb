# TessariDB benchmark — the planner that estimates (G055 W3)

- backend: `memory`
- build: release build
- machine: `macos` / `aarch64`
- records: 50 000 in `people` (indexed on `band`, `skew`, `n`) and the same 50 000 in `plain` (no index)
- percentiles: nearest-rank over every retained sample, per phase

A baseline is comparable with another taken on the same machine and the same
build profile, and with nothing else. It is read by a person; it is not a gate.

Every index-arm answer is compared record by record with the mirror's before it
is timed, so a faster answer that was a different answer fails the run.

## Before — `dev` at `72821fb`, the same workload

| phase | ops | p50 µs |
|---|---|---|
| eq-limit-index (`band = 3 LIMIT 10`) | 200 | 7694.2 |
| eq-limit-scan (mirror) | 200 | 209.5 |
| eq-index (`band = 3`, 5 000 records) | 20 | 7743.1 |
| eq-scan (mirror) | 20 | 17172.8 |
| guard-without-statistics (`n > 0`) | 20 | 17462.3 |

No `ANALYZE` in that build; `EXPLAIN` carried no estimate.

## After
| phase | ops | ops/s | p50 µs | p90 µs | p95 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|---|
| planner-write | 50000 | 38546 | 25.6 | 27.0 | 27.6 | 30.9 | 845.4 |
| eq-limit-index | 200 | 3616 | 251.8 | 277.5 | 286.1 | 326.3 | 4069.5 |
| eq-limit-scan | 200 | 6344 | 155.5 | 163.9 | 168.0 | 186.8 | 251.2 |
| eq-limit served by | **index arm: index | mirror: scan** | | | | | | |
| eq-index | 20 | 179 | 5588.7 | 5740.1 | 5749.3 | 5792.9 | 5792.9 |
| eq-scan | 20 | 59 | 16902.8 | 17206.5 | 17210.0 | 17289.4 | 17289.4 |
| eq served by | **index arm: index | mirror: scan** | | | | | | |
| guard-without-statistics | 20 | 57 | 17294.0 | 18454.2 | 18526.3 | 19851.5 | 19851.5 |
| guard-with-statistics | 20 | 62 | 15996.4 | 16301.6 | 16350.7 | 16537.4 | 16537.4 |
| guard-with-statistics served by | **scan** | | | | | | |
| eq-limit-index-with-statistics | 200 | 29587 | 32.0 | 35.2 | 37.4 | 50.1 | 151.5 |
| estimate skew = 0 | **access "index", estimate 20030 by "probe" | actual 20030 | actual/estimate 1.00x** | | | | | | |
| estimate skew = 17 | **access "index", estimate 31 by "statistics" | actual 31 | actual/estimate 1.00x** | | | | | | |
| estimate band = 3 | **access "index", estimate 5000 by "statistics" | actual 5000 | actual/estimate 1.00x** | | | | | | |
| estimate n > 49000 | **access "index", estimate 1275 by "statistics" | actual 999 | actual/estimate 0.78x** | | | | | | |
| estimate n > 40000 | **access "index", estimate 9948 by "statistics" | actual 9999 | actual/estimate 1.01x** | | | | | | |
| estimate n > 10000 | **access "scan", no estimate; actual 39999** | | | | | | |

## What moved and why

- `eq-limit-index` 7694 → 252 µs: the equality walks its entries in record order
  and stops at the bound instead of naming all 5 000 and reading each back. The
  remaining 252 µs is the scan guard's count of those entries; with statistics
  (`eq-limit-index-with-statistics`) the estimate decides and it is 32 µs.
- `eq-index` 7743 → 5589 µs: the records are fetched a ramping batch at a time
  instead of one confirming read each.
- `guard-without-statistics` vs `guard-with-statistics`, one table, one
  `ANALYZE` apart: 17.3 → 16.0 ms — the count to half the table is gone and the
  read costs the scan it takes (`eq-scan` on the mirror, 16.9 ms).
- Estimate error, actual / estimate: 1.00 on a common value, a rare value and a
  tenth of the table; 0.78 and 1.01 on ranges of 2 % and 20 % (inside one
  bucket of 1/64); `skew = 0` (40 %) falls in the band where the guard counts, so
  its plan reports the probe's exact count; `n > 10000` (80 %) loses to the table
  on the estimate alone.

## vector-filtered, same session (20 000 × 32-d)

| condition admits | walk p50 before | walk p50 after | exact p50 | served by the walk |
|---|---|---|---|---|
| half | 6.7 ms | 7.2 ms | 17.9 ms | 100 of 100 |
| a tenth | 8.1 ms | 8.7 ms | 16.3 ms | 100 of 100 |
| a hundredth | 28.0 ms | 22.9 ms | 16.0 ms | 9 of 100 (was 12) |

The half and tenth rows moved within the run-to-run spread of the exact arm
(17.6 → 17.9, 16.1 → 16.3 ms). At a hundredth the walk now gives up when its
admissions project past the ceiling; what is left above the exact read is the
walk decoding the whole graph before its first step.
