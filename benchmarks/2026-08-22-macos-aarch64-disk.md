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
| write | 2000 | 17777 | 54.1 | 65.0 | 112.7 | 204.8 |

## read-by-id

point reads by record identity — the cheapest access path there is

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| write | 2000 | 19304 | 50.5 | 60.5 | 73.3 | 164.6 |
| read-by-id | 2000 | 117605 | 8.4 | 8.8 | 10.7 | 58.3 |

## filter

the same equality filter over a scan and over an index, so the two are one table

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| write | 2000 | 19571 | 49.5 | 58.2 | 70.6 | 94.1 |
| filter-scan | 100 | 930 | 1048.8 | 1167.2 | 1426.0 | 1436.7 |
| filter-build-index | 1 | 384 | 2602.8 | 2602.8 | 2602.8 | 2602.8 |
| filter-index | 100 | 9355 | 104.2 | 109.9 | 146.2 | 208.4 |
| range-scan | 100 | 979 | 1012.5 | 1061.3 | 1193.8 | 1196.9 |
| range-build-index | 1 | 351 | 2845.1 | 2845.1 | 2845.1 | 2845.1 |
| range-index | 100 | 3622 | 273.0 | 289.7 | 354.8 | 400.7 |

## search

a term search over a full-text index, against the scan of the same condition

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| search-write | 2000 | 18363 | 53.3 | 60.6 | 75.5 | 133.8 |
| search-scan | 100 | 572 | 1730.9 | 1774.2 | 2007.3 | 2224.4 |
| search-build-index | 1 | 83 | 11977.5 | 11977.5 | 11977.5 | 11977.5 |
| search-index | 100 | 60441 | 16.1 | 17.0 | 23.5 | 35.7 |
| search-rank | 100 | 40060 | 24.7 | 25.9 | 33.1 | 33.2 |

## vector-index

the same read served by a graph, with the recall it buys against the exact scan

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| vector-index-build | 1 | 6 | 176462.1 | 176462.1 | 176462.1 | 176462.1 |
| vector-exact-scan | 100 | 243 | 3981.8 | 4301.5 | 6647.6 | 7052.2 |
| vector-graph-walk | 100 | 1138 | 856.8 | 928.7 | 1537.2 | 1881.6 |
| recall | **100.0% of the exact ten, over 1000 asked** | | | | | |

## vector

a nearest-neighbour read over a scan — the number an HNSW index has to beat

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| vector-write | 2000 | 13723 | 66.1 | 81.6 | 162.7 | 1846.3 |
| vector-nearest-scan | 100 | 252 | 3835.9 | 4288.8 | 4691.2 | 6148.9 |
