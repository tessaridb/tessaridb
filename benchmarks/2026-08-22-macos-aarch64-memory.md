# bgv-db benchmark

- backend: `memory`
- build: release build
- machine: `macos` / `aarch64`
- records per workload: 2000
- percentiles: nearest-rank over every retained sample, per phase

A baseline is comparable with another taken on the same machine and the same
build profile, and with nothing else. It is read by a person; it is not a gate.

## write

point writes of a small record, one statement each

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| write | 2000 | 74775 | 13.4 | 16.7 | 18.5 | 64.7 |

## read-by-id

point reads by record identity — the cheapest access path there is

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| write | 2000 | 89083 | 9.2 | 10.2 | 67.4 | 180.3 |
| read-by-id | 2000 | 282943 | 3.5 | 3.7 | 4.5 | 64.0 |

## filter

the same equality filter over a scan and over an index, so the two are one table

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| write | 2000 | 134030 | 7.4 | 7.7 | 7.9 | 15.0 |
| filter-scan | 100 | 1166 | 848.8 | 886.0 | 937.1 | 1179.4 |
| filter-build-index | 1 | 611 | 1637.1 | 1637.1 | 1637.1 | 1637.1 |
| filter-index | 100 | 21147 | 46.2 | 49.8 | 58.0 | 62.0 |

## search

a term search over a full-text index, against the scan of the same condition

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| search-write | 2000 | 118094 | 8.3 | 8.8 | 9.3 | 37.8 |
| search-scan | 100 | 631 | 1569.3 | 1618.5 | 1736.6 | 1808.0 |
| search-build-index | 1 | 134 | 7470.5 | 7470.5 | 7470.5 | 7470.5 |
| search-index | 100 | 134048 | 7.0 | 7.2 | 23.4 | 27.8 |
| search-rank | 100 | 83534 | 11.7 | 12.1 | 16.4 | 23.7 |

## vector

a nearest-neighbour read over a scan — the number an HNSW index has to beat

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| vector-write | 2000 | 73336 | 13.4 | 14.1 | 17.6 | 42.5 |
| vector-nearest-scan | 100 | 266 | 3683.3 | 3987.7 | 4383.9 | 4487.6 |
