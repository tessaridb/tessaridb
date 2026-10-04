# TessariDB benchmark — a fuzzy read ranked by what it reached (G058 C3, Q-909)

- backend: `memory`
- build: release build, `0.25.0-beta` + this change (`dev`, uncommitted tree at the measurement)
- machine: `macos` / `aarch64`
- corpus: the documentation site's `content/`, fingerprint `61672c3fdf0d1b10` — 76 pages, 740 fragments
- judgments: `benchmarks/judgments/docs.tsv` — 81 queries (54 word, 12 prefix, 15 fuzzy), graded 0–3
- command: `cargo run -p tessari-bench --release --example relevance -- <content> benchmarks/judgments/docs.tsv --repeat 3`
- mode: field index (one `SEARCH` index on a field concatenating title, heading and body)
- percentiles: nearest-rank; warm is every run after the first of each query, cold the first

Before, a fuzzy read through a field index was answered unranked: `search::score`
scored the typed spelling, which a misspelling's record does not hold, so every
record scored `0` and the harness did not order by it. After, a score over a
field the statement reads `MATCHES FUZZY` blends the terms the read reached
under one document frequency, each occurrence weighed `1 / (1 + edits)`, and
the harness orders fuzzy reads by it like the other two kinds. Two things moved
together — the engine and the harness's `ORDER BY` — because the second is
meaningless without the first: on the old engine every score is `0` and the
order is store order either way.

| run | all NDCG@10 | all MRR@10 | word | prefix | fuzzy NDCG@10 | fuzzy MRR@10 | warm p50 | warm p99 |
|---|---|---|---|---|---|---|---|---|
| before | 0.7218 | 0.7178 | 0.8287 | 0.8268 | 0.2527 | 0.1540 | 0.151 ms | 9.835 ms |
| after | 0.8210 | 0.8292 | 0.8287 | 0.8268 | 0.7885 | 0.7556 | 0.147 ms | 19.476 ms |

Word and prefix are identical to the digit — the control: neither path changed.
The p99 doubles because a ranked fuzzy read now reads each reached term's
statistics and scores every candidate; the p50 is unmoved. Per fuzzy query, after:
`transcation` 0.359 · `replciation` 0.983 · `geomtery` 1.000 · `snapshto` 0.631 · `vecotrs` 0.964 · `failvoer` 1.000 · `pasword` 0.599 · `vaulst` 0.631 · `unsael` 1.000 · `consoel` 1.000 · `backpu` 0.629 · `trasnactoin` 0.339 · `subscirptoin` 0.731 · `replciatoin` 0.961 · `anlayzer` 1.000.
