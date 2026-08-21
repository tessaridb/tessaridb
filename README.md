# bgv-db

A multi-model database written in Rust.

`bgv-db` stores documents, graphs, relational tables, vectors, full-text and
time-series data in one engine, behind one query language — **bgvQL** — over a
pluggable key–value substrate. It runs as an embedded library, as a single self-hosted
node, or as a cluster that distributes both data and engine roles across nodes.

> **Status: pre-alpha.** Nothing here is usable yet. The repository currently
> holds the workspace skeleton and the design record. Interfaces, the query
> language and the on-disk format are all unstable and will change without
> notice.

## Why

Most systems that need more than one data model end up running more than one
database — a relational store, a vector index, a search cluster, a graph engine —
and then spend their complexity budget keeping those copies consistent with each
other. `bgv-db` takes the other route: one transactional store, several access
models over the same records.

The first consumer is [`bgv-ai-memory`](https://github.com/BoogVAr/bgv-ai-memory),
an agent-memory and project-governance layer that needs documents, a knowledge
graph, vector retrieval and full-text search over the same data, with live
subscriptions and real concurrent access. `bgv-db` is not built only for that —
it is a general-purpose database that happens to have a demanding first user.

## Design goals

| Goal | What it means here |
|---|---|
| **Multi-model** | Documents, graph edges, relational tables and columns, vectors, full-text, time-series — over one record store, not bolted together |
| **One query language** | **bgvQL** — a single surface for every model, including graph traversal and vector search |
| **Pluggable storage** | Everything above the key–value layer is written against a trait. In-memory and RocksDB first |
| **Transactional** | Real transactions with a declared isolation level, not best-effort batching |
| **Real-time** | Change subscriptions as a first-class feature, not polling |
| **Deployable three ways** | Embedded library · single self-hosted node · multi-node cluster with sharding and replication |
| **Multiple interfaces** | CLI, REST, and a socket protocol for connection, control and maintenance |

## Non-goals

- Wire-protocol or storage-format compatibility with any existing database.
- Being fastest at a single model. A specialised engine will beat a general one
  on its own axis; the trade is made deliberately.
- Backwards compatibility before 1.0.

## Architecture

The workspace is layered, with dependencies flowing inward — binaries depend on
services, services on the core, the core on leaf type crates. No crate depends
outward.

```
crates/
  bgv-db-types        leaf   value types, record ids, newtypes
  bgv-db-constants    leaf   tunables, each with unit and rationale
  bgv-db-kv                  key-value abstraction; memory and RocksDB backends
  bgv-db-encoding            key grammar and value codec over the KV layer
  bgv-db-storage             records, indexes, transactions over the KV layer
  bgv-db-ql                  bgvQL: lexer, parser, AST
  bgv-db-planner             logical and physical planning
  bgv-db-exec                execution engine and per-model operators
  bgv-db-index               index kinds: btree, full-text, vector, graph
  bgv-db-core                sessions, auth, permissions, schema catalog
  bgv-db-cluster             node roles, membership, sharding, replication
  bgv-db-rpc                 wire protocol
  bgv-db-api                 REST surface
  bgv-db-cli          bin    command-line client
  bgv-db-node         bin    server / cluster node
```

Only the leaf crates exist today. The rest of the layout is provisional and is
re-derived from the feature matrix before each crate is created — the shape above
records intent, not a commitment.

## Building

```sh
cargo build --workspace
cargo test --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

## Branches

- `main` — stable branch. Releases and tags come from here.
- `dev` — integration branch. Feature branches merge here first.

## Provenance

`bgv-db` is an independent implementation. Its architecture is informed by the
published literature on multi-model storage, LSM and B-tree engines, query
planning, vector and full-text indexing, and distributed transactions — ordinary
engineering practice. No code, grammar file, test fixture, schema or identifier
in this repository is copied or mechanically translated from another database's
source, and no third-party database is vendored, linked, or derived from here.

## Licence

**bgv-db ships under its own commercial licence.** It is proprietary software —
not open source, and not source-available under any public licence. Access to
this repository does not grant a licence to use it.

Any use, hosting, redistribution or derivative work requires a written
commercial licence from the copyright holder. Terms, scope and pricing are set
per agreement. See [LICENSE](LICENSE), and contact the copyright holder to
obtain one.

Copyright (c) boogvar. All rights reserved.
