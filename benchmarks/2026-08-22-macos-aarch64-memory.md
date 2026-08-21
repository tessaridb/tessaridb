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
| write | 2000 | 118006 | 8.2 | 8.8 | 12.4 | 73.6 |

## read-by-id

point reads by record identity — the cheapest access path there is

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| write | 2000 | 139849 | 7.1 | 7.2 | 7.7 | 17.7 |
| read-by-id | 2000 | 322314 | 3.0 | 3.1 | 3.3 | 67.8 |

## filter

the same equality filter over a scan and over an index, so the two are one table

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| write | 2000 | 140908 | 7.1 | 7.2 | 7.6 | 12.6 |
| filter-scan | 100 | 1116 | 879.7 | 945.0 | 1070.8 | 1138.5 |
| filter-build-index | 1 | 582 | 1719.2 | 1719.2 | 1719.2 | 1719.2 |
| filter-index | 100 | 18450 | 51.5 | 58.1 | 83.4 | 171.2 |

## search

a term search over a full-text index, against the scan of the same condition

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| search-write | 2000 | 111110 | 8.7 | 9.3 | 13.1 | 87.9 |
| search-scan | 100 | 613 | 1600.0 | 1714.9 | 1901.8 | 2043.4 |
| search-build-index | 1 | 133 | 7532.6 | 7532.6 | 7532.6 | 7532.6 |
| search-index | 100 | 134968 | 7.2 | 7.5 | 9.5 | 20.7 |
| search-rank | 100 | 84487 | 11.6 | 12.0 | 14.9 | 21.1 |

## vector

a nearest-neighbour read over a scan — the number an HNSW index has to beat

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| vector-write | 2000 | 73235 | 13.5 | 13.8 | 15.6 | 30.0 |
| vector-nearest-scan | 100 | 79 | 12582.2 | 13025.2 | 13232.5 | 13341.8 |
