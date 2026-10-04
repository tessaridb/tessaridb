# TessariDB benchmark — what one event costs the write it runs for (G058 C4, Q-912)

- backend: memory (`MemoryBackend`); build: release; machine: `macos` / `aarch64`
- command: `cargo run --release -p tessari-bench --example event_writes`
- work: 5 000 `CREATE <table>:n` into a fresh store per round, 7 rounds, median µs per write. `plain` has no
  event; `orders` has `DEFINE EVENT noted ON orders FOR CREATE THEN { CREATE audit = { … } }`, whose body
  inserts under a generated identity. Every evented round leaves 5 000 audit records.

| build | plain | one event |
|---|---|---|
| `a2449d8` (before) | 12.4 µs | 398.6 µs |
| + a commit reads a record's versions only where two writers are admitted | 13.0 µs | 30.4 µs |
| + event bodies and conditions parsed once per text | 13.2 / 13.3 µs | 28.6 / 28.8 µs |

The first row is not the event: a generated identity is counted in one record that every insert rewrites, and
each commit read all of that record's versions to look for a second survivor — quadratic in the inserts
(1 000 inserts: 91.5 µs a write with a generated id against 12.4 µs naming one). Only a namespace that admits two
writers can hold a second survivor, so elsewhere the versions are no longer read (`counted_reads` holds the
commit's reads equal after 10 and after 1 000 rewrites). The parse cache is the smaller, separate gain the G055 W7
measurement pointed at; per-statement authorization is kept as it was.
