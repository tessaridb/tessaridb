# TessariDB benchmark

- backend: `memory`
- build: release build
- machine: `macos` / `aarch64`
- records per workload: 2000
- percentiles: nearest-rank over every retained sample, per phase

A baseline is comparable with another taken on the same machine and the same
build profile, and with nothing else. It is read by a person; it is not a gate.

## vector-filtered

a filtered nearest read walked through the graph, with recall against the exact filtered read at three selectivities

| phase | ops | ops/s | p50 µs | p90 µs | p95 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|---|
| filtered-exact 50% | 100 | 57 | 17609.8 | 17866.4 | 18053.9 | 18444.7 | 18453.9 |
| filtered-walk 50% | 100 | 148 | 6683.0 | 7019.5 | 7198.2 | 7774.8 | 10427.8 |
| recall 50% | **98.0% of the exact filtered ten over 1000 asked; 100 of 100 reads served by the walk** | | | | | | |
| filtered-exact 10% | 100 | 62 | 16147.7 | 16471.7 | 16626.9 | 16863.8 | 17060.2 |
| filtered-walk 10% | 100 | 121 | 8110.0 | 9531.8 | 9601.2 | 9889.8 | 10297.5 |
| recall 10% | **99.9% of the exact filtered ten over 1000 asked; 100 of 100 reads served by the walk** | | | | | | |
| filtered-exact 1% | 100 | 63 | 15759.3 | 16071.6 | 16212.1 | 16883.7 | 17335.5 |
| filtered-walk 1% | 100 | 38 | 28027.9 | 29074.7 | 29822.0 | 30161.4 | 30305.5 |
| recall 1% | **100.0% of the exact filtered ten over 1000 asked; 12 of 100 reads served by the walk** | | | | | | |
