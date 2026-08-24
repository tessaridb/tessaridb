# Changelog

Versions before 1.0 do not promise compatibility with each other. The query
language, the wire format and the on-disk format may each change in any release,
and **there is no migration between versions** — a store written by one version
is not guaranteed to open under the next. Treat every upgrade as a fresh store
until this file says otherwise.

The three numbers a pre-release carries are never reused by the release that
follows it: `0.0.1-alpha` is followed by `0.0.2` or higher, never by a bare
`0.0.1`. That is what lets a backup written by a pre-release be told apart from
one written by a final release, because the ordered version a node stores and
compares carries no pre-release suffix.

## 0.0.1-alpha — 2026-08-25

The first version with a number on it. Everything before this was `0.0.0`, which
is to say unversioned.

It is **an alpha in the ordinary sense**: it runs, it is tested, and it is not
finished. Use it for prototypes, evaluation and development. Do not put data you
cannot lose behind it.

### What is here

One transactional record store with several access models over the same
records — documents, tables, edges, key–value spaces, buckets of bytes,
full-text, vectors, time windows and geometry — in one language, under one
snapshot. Two backends: in memory, and on disk on a log-structured merge-tree
engine.

Four ways in: an embedded library, the `tessaridb` command line, HTTP with
WebSocket subscriptions, and a framed binary wire protocol whose specification
and conformance corpus are published separately.

Around them: snapshot-isolation transactions, change subscriptions, namespaces
and databases with users and grants, health and readiness endpoints, metrics,
graceful drain, and log-as-backup with replay-as-restore.

433 conformance cases define the language and run in the build.

### What is not here

Stated as plainly as the list above, because an alpha that is vague about its
absences is worse than one that is missing more.

- **No spatial index.** Geometry can be stored, compared with seven predicates,
  measured geodesically and written as a literal — but every geometry question is
  a scan, and there is no nearest-first read. Seven is also the whole list:
  `geo::touches` is not written, because it needs an algorithm the other seven
  are not built from.
- **No sharding, no replication, no cluster membership.** Peers can be declared
  and read back; nothing replicates between them. The language has words for
  these; the engine does not have the machinery.
- **Not published to crates.io.** Every crate carries `publish = false`. Build it
  from source.
- **No migration between versions**, as above.

### Known rough edges

- Two tests in the suite have failed under heavy parallel load on a busy machine
  and pass reliably otherwise. Both causes were identified and addressed — a
  commit retry loop that re-raced with no backoff, and a test precondition that
  sampled a transient — but neither failure could be reproduced on demand, so
  neither fix is proven. If the build is flaky for you, that is a real report and
  worth sending.

### Notes for anyone who tried an earlier commit

- `tessaridb --version` exists, and prints the full version including the
  pre-release suffix.
- `tessaridb --help` now prints to **standard output** and exits **zero**. It
  previously printed to standard error and exited non-zero, which meant
  `tessaridb --help | grep serve` came back empty. A misspelled flag is still a
  refusal on standard error with a non-zero status.
- `INFO FOR NODE` and `$node` carry a **`build`** field beside `version`.
  `version` is the ordered three numbers a node stores and an upgrade compares;
  `build` is the exact string this binary was compiled as.
- Building now documents its prerequisites: a C++ toolchain and `libclang`, both
  needed by the on-disk backend.
