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
| write | 2000 | 18647 | 51.0 | 62.8 | 88.2 | 153.8 |

## read-by-id

point reads by record identity — the cheapest access path there is

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| write | 2000 | 20099 | 48.5 | 56.8 | 65.4 | 76.9 |
| read-by-id | 2000 | 114503 | 8.6 | 9.0 | 9.3 | 67.5 |

## filter

the same equality filter over a scan and over an index, so the two are one table

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| write | 2000 | 20051 | 48.6 | 57.1 | 73.0 | 140.1 |
| filter-scan | 100 | 951 | 1043.3 | 1068.0 | 1214.3 | 1415.2 |
| filter-build-index | 1 | 339 | 2947.6 | 2947.6 | 2947.6 | 2947.6 |
| filter-index | 100 | 9655 | 102.0 | 107.0 | 128.3 | 132.7 |

## search

a term search over a full-text index, against the scan of the same condition

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| search-write | 2000 | 19576 | 50.1 | 57.2 | 69.2 | 157.8 |
| search-scan | 100 | 564 | 1760.1 | 1796.8 | 1867.6 | 2003.2 |
| search-build-index | 1 | 81 | 12344.9 | 12344.9 | 12344.9 | 12344.9 |
| search-index | 100 | 59605 | 16.2 | 16.8 | 22.0 | 49.5 |
| search-rank | 100 | 40552 | 24.2 | 24.9 | 35.9 | 38.3 |

## vector

a nearest-neighbour read over a scan — the number an HNSW index has to beat

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| vector-write | 2000 | 16988 | 57.2 | 66.9 | 78.2 | 173.4 |
| vector-nearest-scan | 100 | 78 | 12751.1 | 13176.2 | 13700.9 | 13850.2 |
