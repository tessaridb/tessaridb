# TessariDB benchmark — a grouped materialized view kept a group at a time (G058 C4, Q-908)

- backend: disk (`LsmBackend`, default configuration; a synced commit on macOS is a full flush, ~5 ms)
- build: release; **before** = this change with a grouped read classified as a whole-read view (its whole read
  recomputed per batch, as `a2449d8` does), **after** = this change — the two builds differ by that one
  classification
- machine: `macos` / `aarch64`
- command: `cargo run --release -p tessari-bench --example view_groups -- <empty dir>` on each build
- data: 50 000 records `{ g: n % 500, v: n·7919 % 1000 }` in 1 000-record transactions; view
  `SELECT g, count(*) AS c, sum(v) AS s, max(v) AS hi FROM t GROUP BY g`
- work: 100 batches, each one `UPDATE` of one record then `maintain_views`; nearest-rank over the 100 passes

| build | maintain p50 | maintain p99 | view defined in | equals its read |
|---|---|---|---|---|
| before (whole read per batch) | 51 478 µs | 61 686 µs | 51.3 ms | true |
| after, run 1 | 5 358 µs | 8 130 µs | 630.6 ms | true |
| after, run 2 | 5 057 µs | 10 791 µs | 628.4 ms | true |

After the change a batch recomputes the one or two groups its change touched, and its cost is about one synced
commit. The membership map is the price: two system rows per source record, written when the view is declared
(51 → 630 ms here) and kept on every change after. Both builds end with the view equal to its read.
