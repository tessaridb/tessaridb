# TessariDB benchmark

- backend: `memory`
- build: release build
- machine: `macos` / `aarch64`
- records per workload: 2000
- percentiles: nearest-rank over every retained sample, per phase

A baseline is comparable with another taken on the same machine and the same
build profile, and with nothing else. It is read by a person; it is not a gate.

## paging

the same page by offset, by cursor, and by a cursor that cannot seek, at four depths

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| corpus | **100000 records, pages of 20** | | | | | |
| offset at 0 | 20 | 35155 | 10.0 | 13.3 | 368.2 | 368.2 |
| cursor at 0 | 20 | 71900 | 12.5 | 16.2 | 29.4 | 29.4 |
| ordered offset at 0 | 20 | 23 | 43788.9 | 44793.7 | 48285.0 | 48285.0 |
| ordered cursor (walked) at 0 | 20 | 23 | 43543.5 | 44633.9 | 46060.6 | 46060.6 |
| offset at 1000 | 20 | 2881 | 340.5 | 345.2 | 453.6 | 453.6 |
| cursor at 1000 | 20 | 77482 | 12.5 | 13.0 | 18.5 | 18.5 |
| ordered offset at 1000 | 20 | 22 | 44646.1 | 45364.7 | 45432.4 | 45432.4 |
| ordered cursor (walked) at 1000 | 20 | 23 | 42480.8 | 43419.2 | 44116.6 | 44116.6 |
| offset at 10000 | 20 | 279 | 3525.0 | 3718.8 | 4254.7 | 4254.7 |
| cursor at 10000 | 20 | 73960 | 12.5 | 13.5 | 29.1 | 29.1 |
| ordered offset at 10000 | 20 | 21 | 46129.9 | 47228.2 | 59430.1 | 59430.1 |
| ordered cursor (walked) at 10000 | 20 | 23 | 42338.7 | 43414.2 | 43772.8 | 43772.8 |
| offset at 99000 | 20 | 24 | 39984.9 | 41121.5 | 55596.5 | 55596.5 |
| cursor at 99000 | 20 | 58580 | 12.6 | 13.2 | 100.1 | 100.1 |
| ordered offset at 99000 | 20 | 20 | 49703.5 | 50568.2 | 50988.7 | 50988.7 |
| ordered cursor (walked) at 99000 | 20 | 23 | 42420.1 | 43243.0 | 44339.5 | 44339.5 |
