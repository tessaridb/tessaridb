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
| write | 2000 | 17833 | 54.6 | 63.9 | 83.7 | 102.9 |

## read-by-id

point reads by record identity — the cheapest access path there is

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| write | 2000 | 19767 | 49.2 | 57.8 | 70.9 | 146.1 |
| read-by-id | 2000 | 119835 | 8.2 | 8.7 | 9.0 | 51.9 |

## filter

the same equality filter over a scan and over an index, so the two are one table

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| write | 2000 | 19669 | 49.6 | 56.6 | 72.2 | 217.0 |
| filter-scan | 100 | 951 | 1035.8 | 1094.2 | 1215.8 | 1413.3 |
| filter-build-index | 1 | 356 | 2810.8 | 2810.8 | 2810.8 | 2810.8 |
| filter-index | 100 | 9917 | 99.5 | 102.5 | 135.7 | 139.4 |

## search

a term search over a full-text index, against the scan of the same condition

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| search-write | 2000 | 19512 | 50.3 | 56.7 | 67.6 | 122.8 |
| search-scan | 100 | 570 | 1730.9 | 1836.8 | 1998.1 | 2110.9 |
| search-build-index | 1 | 83 | 12011.0 | 12011.0 | 12011.0 | 12011.0 |
| search-index | 100 | 62559 | 15.6 | 16.2 | 18.8 | 41.9 |
| search-rank | 100 | 41357 | 23.9 | 24.8 | 27.9 | 33.1 |

## vector

a nearest-neighbour read over a scan — the number an HNSW index has to beat

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| vector-write | 2000 | 17063 | 56.5 | 67.0 | 77.5 | 148.6 |
| vector-nearest-scan | 100 | 257 | 3815.4 | 4151.0 | 4421.3 | 4470.5 |
