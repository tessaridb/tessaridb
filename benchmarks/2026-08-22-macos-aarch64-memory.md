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
| write | 2000 | 79182 | 11.8 | 16.6 | 19.4 | 141.2 |

## read-by-id

point reads by record identity — the cheapest access path there is

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| write | 2000 | 109754 | 9.2 | 10.0 | 10.2 | 15.3 |
| read-by-id | 2000 | 286214 | 3.5 | 3.6 | 3.7 | 37.5 |

## filter

the same equality filter over a scan and over an index, so the two are one table

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| write | 2000 | 132669 | 7.5 | 7.7 | 7.9 | 12.0 |
| filter-scan | 100 | 1153 | 857.9 | 892.4 | 1017.0 | 1219.2 |
| filter-build-index | 1 | 573 | 1745.7 | 1745.7 | 1745.7 | 1745.7 |
| filter-index | 100 | 21017 | 46.8 | 48.8 | 61.7 | 62.4 |

## search

a term search over a full-text index, against the scan of the same condition

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| search-write | 2000 | 117861 | 8.4 | 8.7 | 9.3 | 14.8 |
| search-scan | 100 | 620 | 1593.4 | 1687.9 | 1817.0 | 1868.3 |
| search-build-index | 1 | 127 | 7870.4 | 7870.4 | 7870.4 | 7870.4 |
| search-index | 100 | 123884 | 7.3 | 7.8 | 24.6 | 48.1 |
| search-rank | 100 | 80080 | 12.2 | 12.5 | 21.4 | 26.0 |

## vector-index

the same read served by a graph, with the recall it buys against the exact scan

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| vector-index-build | 1 | 6 | 162623.0 | 162623.0 | 162623.0 | 162623.0 |
| vector-exact-scan | 100 | 270 | 3649.3 | 3863.9 | 4167.1 | 4361.7 |
| vector-graph-walk | 100 | 1605 | 616.5 | 661.1 | 745.9 | 751.4 |
| recall | **100.0% of the exact ten, over 1000 asked** | | | | | |

## vector

a nearest-neighbour read over a scan — the number an HNSW index has to beat

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| vector-write | 2000 | 75416 | 13.2 | 13.5 | 14.3 | 47.9 |
| vector-nearest-scan | 100 | 271 | 3654.7 | 3824.5 | 4010.4 | 4043.8 |
