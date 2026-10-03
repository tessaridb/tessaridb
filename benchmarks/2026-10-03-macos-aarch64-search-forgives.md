# TessariDB benchmark — search that forgives (G055 W5)

- backend: `memory`
- build: release build
- machine: `macos` / `aarch64`
- corpus: the documentation site's `content/`, fingerprint `6f1dc272a8307c32` — 72 pages, 700 fragments
- judgments: `benchmarks/judgments/docs.tsv` — 81 queries (54 word, 12 prefix, 15 fuzzy), graded 0–3
- command: `cargo run -p tessari-bench --release --example relevance -- <content> benchmarks/judgments/docs.tsv --repeat 10 [--engine weighted]`
- percentiles: nearest-rank; warm is every run after the first of each query, cold the first

A baseline is comparable with another taken on the same machine, build profile
and corpus fingerprint, and with nothing else. It is read by a person; it is
not a gate. The relevance side is the gate: every number below is NDCG@10 and
MRR@10 over the same judged pages.

"Field index" is one `SEARCH` index on a field concatenating title, heading and
body; a fuzzy query there is answered in store order (the harness does not rank
it, and `search::score` scores the typed words). "`DEFINE SEARCH` weighted" is a
search over title (weight 2), heading (3) and body.

## Before — `dev` at `87bad9d`

| mode | all NDCG@10 | all MRR@10 | word | prefix | fuzzy | warm p50 | warm p99 |
|---|---|---|---|---|---|---|---|
| field index | 0.6820 | 0.6788 | 0.8203 | 0.7801 | 0.1057 | 0.107 ms | 7.341 ms |
| `DEFINE SEARCH` weighted | 0.8220 | 0.8349 | 0.8880 | 0.8677 | 0.5479 | 3.221 ms | 26.381 ms |

Five fuzzy queries answered nothing through `DEFINE SEARCH`: `failvoer`,
`trasnactoin`, `subscirptoin`, `replciatoin` (stem-changing typos measured against
stems) and `anlayzer` (a typo in the third letter, inside the old three-letter
non-fuzzy prefix).

## After — this wave

| mode | all NDCG@10 | all MRR@10 | word | prefix | fuzzy | warm p50 | warm p99 |
|---|---|---|---|---|---|---|---|
| field index | 0.7103 | 0.6996 | 0.8203 | 0.7801 | 0.2586 | 0.127 ms | 10.059 ms |
| `DEFINE SEARCH` weighted | 0.8826 | 0.9012 | 0.8880 | 0.8677 | 0.8749 | 0.093 ms | 9.503 ms |

Word and prefix relevance are identical to the bit in both modes — the
postings-scored `FROM SEARCH` ranks every record with the same score the text
path gives, which the suite asserts record for record. Every fuzzy query now
answers; through `DEFINE SEARCH` four of the five that answered nothing now
reach NDCG ≥ 0.83.

## Latency by kind, warm, `--repeat 10`

Run on each build with the judgments filtered to one kind (single variable: the
build).

| mode · kind | before p50 | before p99 | after p50 | after p99 |
|---|---|---|---|---|
| field index · word | 0.090 ms | 0.823 ms | 0.089 ms | 0.807 ms |
| field index · prefix | 3.844 ms | 7.494 ms | 3.967 ms | 7.851 ms |
| field index · fuzzy | 3.323 ms | 6.124 ms | 4.920 ms | 10.411 ms |
| `DEFINE SEARCH` · word | 2.180 ms | 26.332 ms | 0.071 ms | 0.611 ms |
| `DEFINE SEARCH` · prefix | 3.888 ms | 7.415 ms | 0.101 ms | 0.192 ms |
| `DEFINE SEARCH` · fuzzy | 3.752 ms | 17.303 ms | 3.882 ms | 9.615 ms |

- `DEFINE SEARCH` word and prefix: ranked from the postings (shape
  `search from postings`), reading only the answered records — 31× and 38×.
  The whole set's warm median is now 0.73× the field index's (C5's stated factor
  was ≤ 3×).
- Fuzzy, both modes: a surface reaches a stem, and the stem's postings nominate
  every record holding any spelling of it, each re-tested. Through the field
  index that is +48 % at the median for queries that previously answered nothing
  or less; through `DEFINE SEARCH` the per-read analysis memo (each token
  analysed once per read) brings it level at the median and halves the tail.
  The field index's whole-set p50 (+19 %) and p99 (+37 %) move only because of
  its fuzzy queries; recorded as the accepted price of answering them (Q-909).
