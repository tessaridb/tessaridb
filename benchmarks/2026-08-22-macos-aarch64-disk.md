# bgv-db benchmark

- backend: `disk`
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
| write | 2000 | 18513 | 52.3 | 63.6 | 84.0 | 152.1 |

## read-by-id

point reads by record identity — the cheapest access path there is

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| write | 2000 | 20125 | 48.5 | 56.0 | 66.2 | 83.4 |
| read-by-id | 2000 | 114418 | 8.7 | 9.0 | 9.4 | 55.1 |

## filter

the same equality filter over a scan and over an index, so the two are one table

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| write | 2000 | 19833 | 48.9 | 56.3 | 73.8 | 161.8 |
| filter-scan | 100 | 953 | 1040.2 | 1082.6 | 1193.6 | 1312.3 |
| filter-build-index | 1 | 340 | 2937.0 | 2937.0 | 2937.0 | 2937.0 |
| filter-index | 100 | 9501 | 103.5 | 107.6 | 144.7 | 147.7 |

## search

a term search over a full-text index, against the scan of the same condition

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| search-write | 2000 | 18638 | 51.8 | 60.7 | 80.7 | 201.4 |
| search-scan | 100 | 571 | 1736.8 | 1792.7 | 1930.8 | 1996.8 |
| search-build-index | 1 | 83 | 12082.2 | 12082.2 | 12082.2 | 12082.2 |
| search-index | 100 | 60335 | 16.2 | 16.8 | 22.5 | 36.5 |
| search-rank | 100 | 40395 | 24.5 | 25.0 | 28.6 | 36.0 |

## vector-index

the same read served by a graph, with the recall it buys against the exact scan

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| vector-index-build | 1 | 6 | 156419.4 | 156419.4 | 156419.4 | 156419.4 |
| vector-exact-scan | 100 | 256 | 3804.8 | 4116.2 | 4300.9 | 4465.6 |
| vector-graph-walk | 100 | 1196 | 825.1 | 895.9 | 988.2 | 1026.8 |
| recall | **100.0% of the exact ten, over 1000 asked** | | | | | |

## vector

a nearest-neighbour read over a scan — the number an HNSW index has to beat

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| vector-write | 2000 | 16549 | 58.2 | 70.0 | 89.0 | 248.3 |
| vector-nearest-scan | 100 | 258 | 3840.4 | 3977.6 | 4401.5 | 4464.2 |
