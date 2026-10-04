# TessariDB benchmark — an approximate vector read, node by node (G058 C2, Q-906)

- backend: disk (`Db::open`, default configuration)
- build: release; **before** = `e3746d9` (whole graph decoded per read), **after** = this change (nodes read as
  the walk reaches them) — the two builds differ by the graph's node source alone
- machine: `macos` / `aarch64`
- command: `cargo run --release -p tessari-bench --example vector_walk -- <empty dir>` on each build
- data: 20 000 records × `vector<32>`, index `VECTOR cosine` (16 neighbours, exploration 64), built through the
  language in 500-record transactions, fixed seed
- read: `SELECT id FROM notes ORDER BY vector::cosine(e, q) LIMIT 10 APPROXIMATE`, 200 queries, each once cold and
  5 times warm; nearest-rank over the 1 000 warm runs

| build | warm p50 | warm p99 | build time | answers (fingerprint of all 200) |
|---|---|---|---|---|
| before `e3746d9` | 27 473 µs | 30 113 µs | 10.8 s | `baadd5768e50a450` |
| after | 1 700 µs | 2 675 µs | 12.2 s | `baadd5768e50a450` |

The answers are the same records for every query (one fingerprint over all 200), so recall is unchanged: the walk
visits the same nodes in the same order and only stops decoding the ones it never visits. Q-906's ~6 ms floor was
measured in memory; on disk the whole-graph decode was 27 ms and is gone. The build is a single run each and
within the spread a disk build shows; it is not claimed to have moved.
