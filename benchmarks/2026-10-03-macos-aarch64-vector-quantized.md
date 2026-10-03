# TessariDB benchmark

- backend: `memory`
- build: release build
- machine: `macos` / `aarch64`
- records per workload: 2000
- percentiles: nearest-rank over every retained sample, per phase

A baseline is comparable with another taken on the same machine and the same
build profile, and with nothing else. It is read by a person; it is not a gate.

## vector-quantized

a quantized vector store against a full-precision one — bytes per vector, build, walk and recall after rescoring

| phase | ops | ops/s | p50 µs | p90 µs | p95 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|---|
| full write + build | 1 | 0 | 4153505.9 | 4153505.9 | 4153505.9 | 4153505.9 | 4153505.9 |
| full footprint | **vector_bytes 260, node_bytes 326, nodes 20000** | | | | | | |
| full exact | 100 | 54 | 18518.4 | 18783.8 | 18932.5 | 19507.9 | 20892.8 |
| full walk | 100 | 153 | 6463.6 | 6891.3 | 7049.5 | 7292.2 | 7853.0 |
| full recall | **98.4% of the exact ten over 1000 asked** | | | | | | |
| coded write + build | 1 | 0 | 4145370.9 | 4145370.9 | 4145370.9 | 4145370.9 | 4145370.9 |
| coded footprint | **vector_bytes 52, node_bytes 118, nodes 20000** | | | | | | |
| coded exact | 100 | 54 | 18359.8 | 18648.9 | 18762.7 | 20295.6 | 20767.3 |
| coded walk | 100 | 163 | 6108.5 | 6408.7 | 6461.2 | 6811.4 | 6911.8 |
| coded recall | **95.6% of the exact ten over 1000 asked** | | | | | | |
