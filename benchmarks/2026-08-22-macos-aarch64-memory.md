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
| write | 2000 | 126915 | 7.6 | 8.2 | 12.3 | 68.0 |

## read-by-id

point reads by record identity — the cheapest access path there is

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| write | 2000 | 137939 | 7.1 | 7.4 | 9.9 | 67.6 |
| read-by-id | 2000 | 318628 | 3.0 | 3.2 | 6.0 | 55.5 |

## filter

the same equality filter over a scan and over an index, so the two are one table

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| write | 2000 | 136383 | 7.0 | 9.3 | 10.0 | 91.1 |
| filter-scan | 100 | 1148 | 854.0 | 920.1 | 998.6 | 1111.0 |
| filter-build-index | 1 | 583 | 1715.0 | 1715.0 | 1715.0 | 1715.0 |
| filter-index | 100 | 20929 | 47.1 | 49.3 | 52.7 | 63.3 |
| range-scan | 100 | 1179 | 849.3 | 869.8 | 905.2 | 919.4 |
| range-build-index | 1 | 611 | 1635.8 | 1635.8 | 1635.8 | 1635.8 |
| range-index | 100 | 9495 | 104.8 | 108.6 | 117.0 | 132.2 |

## search

a term search over a full-text index, against the scan of the same condition

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| search-write | 2000 | 118223 | 8.4 | 8.7 | 9.3 | 20.2 |
| search-scan | 100 | 629 | 1571.2 | 1654.3 | 1844.5 | 1886.4 |
| search-build-index | 1 | 136 | 7368.2 | 7368.2 | 7368.2 | 7368.2 |
| search-index | 100 | 134680 | 7.1 | 7.4 | 10.6 | 28.8 |
| search-rank | 100 | 83417 | 11.8 | 11.9 | 16.5 | 26.1 |

## vector-index

the same read served by a graph, with the recall it buys against the exact scan

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| vector-index-build | 1 | 6 | 154786.8 | 154786.8 | 154786.8 | 154786.8 |
| vector-exact-scan | 100 | 268 | 3620.8 | 3945.6 | 4384.2 | 7119.4 |
| vector-graph-walk | 100 | 1579 | 614.1 | 666.6 | 779.1 | 1685.0 |
| recall | **100.0% of the exact ten, over 1000 asked** | | | | | |

## vector

a nearest-neighbour read over a scan — the number an HNSW index has to beat

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| vector-write | 2000 | 70177 | 13.4 | 16.2 | 17.8 | 111.4 |
| vector-nearest-scan | 100 | 264 | 3663.7 | 4109.7 | 4605.5 | 4721.4 |
