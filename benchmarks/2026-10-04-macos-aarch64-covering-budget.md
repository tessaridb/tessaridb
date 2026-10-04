# TessariDB benchmark — how finely a query box is covered (G058 C1)

- backend: disk (`LsmBackend`, default configuration), one store, warm
- build: release build, `0.25.0-beta` + G058 tree (`dev`, uncommitted at the measurement)
- machine: `macos` / `aarch64`
- command: `cargo run --release -p tessari-bench --example covering -- <empty dir>`
- corpus: 43 640 records from a fixed seed — 30 000 points clustered around three cities (σ ≈ 0.04° × 0.03°),
  10 000 spread over a continent, 3 000 buildings (≈ 50 m squares), 600 roads (1–6 km), 40 regions (0.5–2°)
- queries: 120 boxes per size centred on stored records — street (±0.004°), district (±0.05°), country (±1.5°);
  each run once cold and 5 times warm; the read is the whole thing — the index's region read (filter) and the
  exact `intersects` on every candidate (refine)
- columns: `entries` index entries read and `candidates` records the boxes could not rule out, summed over the 120
  queries' first run; `results` records that truly meet the query (the control — it must not move); latency
  nearest-rank over the 600 warm runs

```
street   budget   4 | entries   427574 | candidates     4871 | results    4593 | p50 0.311 ms | p99 6.801 ms
street   budget   8 | entries    80248 | candidates     4871 | results    4593 | p50 0.220 ms | p99 2.922 ms
street   budget  16 | entries    33833 | candidates     4871 | results    4593 | p50 0.294 ms | p99 0.592 ms
street   budget  32 | entries    32934 | candidates     4871 | results    4593 | p50 0.675 ms | p99 1.053 ms
street   budget  64 | entries    33061 | candidates     4871 | results    4593 | p50 0.732 ms | p99 1.062 ms
street   budget 128 | entries    57199 | candidates     4871 | results    4593 | p50 1.662 ms | p99 2.100 ms
street   budget 256 | entries   112268 | candidates     4871 | results    4593 | p50 3.688 ms | p99 5.067 ms
district budget   4 | entries  1737162 | candidates   520862 | results  520771 | p50 13.924 ms | p99 18.020 ms
district budget   8 | entries  1579181 | candidates   520862 | results  520771 | p50 13.616 ms | p99 17.832 ms
district budget  16 | entries  1453345 | candidates   520862 | results  520771 | p50 12.661 ms | p99 17.574 ms
district budget  32 | entries  1309831 | candidates   520862 | results  520771 | p50 12.357 ms | p99 17.885 ms
district budget  64 | entries  1203869 | candidates   520862 | results  520771 | p50 12.239 ms | p99 17.802 ms
district budget 128 | entries  1139907 | candidates   520862 | results  520771 | p50 12.535 ms | p99 19.301 ms
district budget 256 | entries  1107871 | candidates   520862 | results  520771 | p50 13.990 ms | p99 21.768 ms
country  budget   4 | entries  2488632 | candidates  1052935 | results 1052935 | p50 21.991 ms | p99 24.813 ms
country  budget   8 | entries  2032231 | candidates  1052935 | results 1052935 | p50 21.877 ms | p99 23.056 ms
country  budget  16 | entries  1903030 | candidates  1052935 | results 1052935 | p50 21.879 ms | p99 23.072 ms
country  budget  32 | entries  1871184 | candidates  1052935 | results 1052935 | p50 21.939 ms | p99 23.396 ms
country  budget  64 | entries  1860372 | candidates  1052935 | results 1052935 | p50 22.330 ms | p99 23.491 ms
country  budget 128 | entries  1846165 | candidates  1052935 | results 1052935 | p50 22.969 ms | p99 24.125 ms
country  budget 256 | entries  1845591 | candidates  1052935 | results 1052935 | p50 24.648 ms | p99 26.245 ms
```

**Reading.** The budget moves only how many entries the read touches and how many range scans it issues — the
candidate count is the same at every budget, because a record is box-tested once however many cells reach it, and
the results do not move (the control). A street query pays for a coarse covering (budget 4 reads 13× the entries of
16) and for a fine one (each cell is a scan plus a lookup per level above it: 128 costs 5.7× the p50 of 16). A
district or country query is dominated by the candidates it must refine, so the budget moves it by a few percent.
**Sixteen** has the lowest street p99 and sits within 3 % of the best p50 at the other two sizes, so the constant
`SPATIAL_QUERY_CELLS` stays at 16 — now measured rather than chosen by symmetry with the record budget.
