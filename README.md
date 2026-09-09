<div align="center">

<img src="assets/logo/tessaridb-mark-256.png" alt="" width="112" height="112">

# TessariDB

**Ten engines. One transactional store. One language.**

A real-time multi-model database, written in Rust, built for AI agents and the
products around them.

[![status](https://img.shields.io/badge/status-in%20development-D98E33?style=flat-square)](#status)
[![version](https://img.shields.io/badge/version-0.0.6--beta-6B5FD1?style=flat-square)](#status)
[![licence](https://img.shields.io/badge/licence-BUSL--1.1-6B5FD1?style=flat-square)](LICENSE)
[![rust](https://img.shields.io/badge/rust-1.85%2B-6B5FD1?style=flat-square)](Cargo.toml)
[![conformance](https://img.shields.io/badge/conformance-1296%20cases-6B5FD1?style=flat-square)](crates/tessari-conformance/tests/corpus)

[tessaridb.com](https://tessaridb.com) · [docs](https://docs.tessaridb.com) ·
[protocol](https://github.com/TessariDB/TessariDB-protocol) ·
[Rust SDK](https://github.com/TessariDB/TessariDB-sdk-rust)

</div>

> [!NOTE]
> **TessariDB is a beta — `0.0.6-beta`.** It is released, tested and published as
> a container image, and the licence makes production use free, including inside
> a commercial company.
> What a beta does not promise yet is permanence of shape: before 1.0 the query
> language, the wire format and the on-disk format may still change, there is no
> migration between versions, and several engines are still partial. So pin a
> released version and expect to re-ingest across one.
> [**Status**](#status) says what runs today, engine by engine — it is a report,
> not a roadmap. The [**changelog**](CHANGELOG.md) says what each version is and
> what it is missing.

---

Most systems that need more than one data model end up running more than one
database — a relational store, a vector index, a search cluster, a graph engine,
an object bucket — and then spend their whole complexity budget keeping those
copies agreeing with each other. There is no transaction across them, so the
question is never *whether* they drift, only when somebody notices.

TessariDB takes the other route: **one transactional record store, several
access models over the same records.** Documents, tables, edges, keys, files,
terms, vectors, time windows and geometry are not separate systems bolted
together — they are different ways of reading the same committed bytes, under
one snapshot, in one language.

```sql
DEFINE ANALYZER english FILTERS lowercase, ascii, stemmer;
DEFINE FIELD    body      ON notes TYPE string ANALYZER english;
DEFINE INDEX    by_body   ON notes FIELDS body SEARCH;
DEFINE INDEX    by_vector ON notes FIELDS embedding VECTOR cosine;
DEFINE TABLE    cites EDGE;

SELECT * FROM notes
 WHERE body MATCHES 'lovelace'
 ORDER BY vector::cosine(embedding, $query)
 LIMIT 10;
```

One statement, one snapshot: a full-text term, a vector neighbourhood and the
records themselves. No fan-out, no reconciliation, no second client library.

## Why this fits AI agents

An agent's working set is not one shape. Inside a single turn it wants the
document it is editing, the graph of what depends on that document, the vector
neighbourhood of a memory, the exact phrase somebody typed three weeks ago, the
file that was attached, and the last handful of events. The usual answer is five
services and a consistency problem that lands in the agent's own code.

- **One commit, or none.** When an agent writes a memory, its embedding and its
  edges, either all three land or none do. Split across a database, a vector
  index and a graph store there is no such guarantee — and a retrieval that
  silently misses the embedding does not fail, it returns a plausible wrong
  answer, which is the worst failure an agent can have.
- **One retrieval, not three round trips.** Hybrid recall — a term, a
  neighbourhood, a hop, a filter, a time window — is one statement evaluated
  against one snapshot, not three queries stitched together in application code.
- **One surface to expose as a tool.** An agent tool is `run(script)` and a
  result, rather than six client libraries with six auth models. A
  [typed query builder](crates/tessari-query) is there for when you would rather
  a model did not write raw text.
- **Errors a model can act on.** The language refuses what it does not
  understand *by name* — `NameTaken`, `UnexpectedToken`, `NoSuchFunction` with
  the function and the argument position — instead of a stack trace an agent can
  only paste back to you.
- **Approximation is declared, never assumed.** A vector read says whether it
  was exact or approximate. An agent that cannot tell the difference will report
  the wrong neighbour with complete confidence.
- **Strict where it matters, loose where it does not.** Schemafull and
  schemaless are per table, so an agent's scratch memory and your ledger can
  live in the same database without either compromising.
- **It tells you what changed.** Change subscriptions are first-class, so an
  agent reacts to the store instead of polling it.

## The engines

Every row is proven by an executable corpus — a script the build runs and
compares against expected answers, case by case. The counts are those cases.

| Engine | What it gives you | Cases | State |
|---|---|---|:--|
| **Documents** | schemaless or schemafull records, nested objects and arrays, typed fields with defaults | 38 + 78 + 16 | ✅ runs |
| **Relational** | declared tables and fields, unique and multi-field indexes, joins whose answer an index may not change, `INSERT` of several records in one statement at identities the store produces | 62 + 27 + 51 + 21 | ✅ runs |
| **Graph** | edge tables, `RELATE`, properties on the edge, multi-hop traversal in both directions, an edge table that names the pair it joins and refuses every other, a declared graph that holds its own records with no table declared beside it and takes them with it when dropped, tables you already have joining it with `IN`, `DEFINE EDGE` writing adjacency beside the node so a hop is a range read, an edge removed by the pair it joins, and `DEPTH n` bounding a repeated hop | 71 | ✅ runs |
| **Key–value** | `SPACE`s — one key, one whole value, ordered range scans with inclusive or exclusive bounds | 12 | ✅ runs |
| **Objects & files** | `BUCKET`s — bytes addressed by path, byte-range reads, writes at an offset, metadata that is an ordinary record, and a declared ceiling on the largest file the bucket takes | 35 | ✅ runs |
| **Full-text** | per-field analyzers, whole-term search, prefix search a reader is served by while still typing, fuzzy search that survives a typo, lowercase · ASCII folding · Porter2 stemming, a quoted phrase with declared slop that widens the window without relaxing the order, `OR` and `NOT` inside a query, per-field weighting written as arithmetic, a `did you mean` suggestion that is a field beside the records and never a substitution into the query, and highlighting that marks the token the read actually reached rather than the characters that were typed | 100 | ✅ runs |
| **Vector** | cosine, Euclidean and dot distance, kNN ordering, a graph index that declares whether it answered exactly, and a field that declares how wide its vectors are so a write of any other width is refused where it happens, and a vector store declared as one so the width, the index and the requirement cannot come apart, and a read that says what it will spend on the walk, and a recall the store reports only once something has measured it | 50 | ✅ runs |
| **Vault** | a store whose declared fields can be `SECRET`, sealed under a per-record key before the record is encoded so the index, the change feed, the replication log and a backup all carry ciphertext, read only by `REVEAL` naming one record, sealed and unsealed by a passphrase that reaches memory and never disk, and dropped by destroying the key rather than the rows, with an opaque recipient set the engine carries and never reads, and a strictness a vault cannot be talked out of because a field nobody declared is a field nothing seals, with a trail every `REVEAL` writes to and `INFO FOR AUDIT` reads back, and edited field by field so that rotating a secret keeps the recipients it was shared with | 54 | ✅ runs |
| **Time-series** | `DEFINE SERIES` — a table with a declared retention, past which a record is not answered with while its bytes are still there and its removal is a separate act, epoch-anchored windows every process agrees on, aggregates per window, and retention as a statement over any table that reports what it removed | 21 | ✅ runs |
| **References** | `FETCH` — follow a reference, an array of them, or a nested route, without a join | 12 | ✅ runs |
| **Queues** | `DEFINE QUEUE` — work handed out one holder at a time under a hold that lapses, first-in-first-out by identity, an attempt ceiling whose dead letter is a predicate rather than a second table, and a claim that is an ordinary write so it replicates, recovers and needs no lease manager | 24 | ✅ runs |
| **Geospatial** | a geometry type on an exact integer grid, eight predicates over whole shapes, geodesic distance and area, shapes written as literals, a spatial index seven of the eight predicates read through, a nearest-first read over positions, a geo store declared as one so the field, the index and the requirement cannot come apart, and a measured refinement ratio saying what that index's candidates cost | 67 | 🚧 partial — the nearest few is over positions rather than whole shapes |

Underneath all of them, one substrate with two backends: **in memory**, and
**on disk** on a log-structured merge-tree engine. Everything above the
key–value layer is written against a single trait, which is what makes the pair
possible rather than aspirational.

## And around them

| | |
|---|---|
| **Transactions** | snapshot isolation on the commit log, `BEGIN` · `COMMIT` · `CANCEL` |
| **Real-time** | change subscriptions as a first-class feature — over the wire and over a WebSocket |
| **Stream ingestion** | `DEFINE CONSUMER` — one statement says what to read, where it lands and under which group, and the node runs it; at-least-once, never exactly-once |
| **Multi-tenant** | namespaces and databases, users, roles, `GRANT` and `REVOKE` per database |
| **Four ways in** | embedded library · `tessaridb` CLI · HTTP + WebSocket · a framed binary wire protocol |
| **Operable** | health and readiness endpoints, Prometheus metrics, graceful drain, log-as-backup with replay-as-restore |
| **Specified** | the wire and value protocol is [published](https://github.com/TessariDB/TessariDB-protocol) with a shared conformance corpus, so a client in any language is written from the spec and not from our source |

## Status

**Stage: active development · `0.0.6-beta` · not published to crates.io.** What
follows is what runs today, not a roadmap.
<!-- absent: published-to-crates-io -->

- ✅ **Runs:** the embedded library, the `tessaridb` command line, the HTTP and
  WebSocket surface, the binary wire protocol (v1.0, with a published spec and
  conformance corpus), single-node serving with roles, endpoints and graceful
  drain, backup and restore.
- 🚧 **Partial:** geospatial can store a shape, answer eight predicates over
  whole shapes, measure geodesic distance and area, and be written as a literal
  in a script. `DEFINE INDEX … SPATIAL` writes and maintains a **spatial index**
  — the cells covering each geometry, with the record's bounding box in each
  entry — and **seven of the eight predicates now read through it**: the query
  shape is covered by cells of its own, the entries under and above them are
  read, the stored boxes reject what they can, and the exact predicate decides
  the rest. `geo::disjoint` is the complement of a region and stays an exact
  scan by design. The same index answers **the nearest few** —
  `ORDER BY geo::distance(at, …) LIMIT k` walks cells cheapest-first, keyed by a
  distance nothing inside the cell can beat, and stops when the best cell left is
  further than the worst answer held; that is exact rather than approximate, so
  it asks nothing of the statement. What is missing is a distance to a shape
  larger than a position (which is why the nearest few is over positions), a
  nearest-first read under a `WHERE`, and any measured tuning of how finely a
  query is covered.
  <!-- absent: distance-to-a-shape-larger-than-a-position -->
  <!-- absent: nearest-first-under-a-where -->
  <!-- absent: measured-covering-budget -->
  Peers are declared and read back, but nothing replicates between them.
- ⛔ **Not there:** sharding, replication, and cluster membership. The language
  has words for them; the engine does not have the machinery yet.
  <!-- absent: sharding-replication-cluster-membership -->
- 🔄 **Not promised yet:** before 1.0 the query language, the wire format and the
  on-disk format may still change, and there is no migration between versions.
  <!-- absent: migration-between-versions -->

Pin a released version rather than tracking `dev`, which moves. And keep a backup
you have actually restored: the log *is* the backup, and `--verify` reads one
back without needing anywhere to put it.

## Opening one

```rust
use tessaridb::Db;

let db = Db::open("./data")?;          // or Db::in_memory()
let mut session = db.session();

session.run(
    "DEFINE NAMESPACE prod;
     USE NAMESPACE prod;
     DEFINE DATABASE orders;
     USE DATABASE orders;
     DEFINE TABLE users SCHEMAFULL;
     DEFINE FIELD email ON users TYPE string REQUIRED;
     DEFINE FIELD joined ON users TYPE datetime DEFAULT time::now();
     DEFINE INDEX by_email ON users FIELDS email UNIQUE;",
)?;

session.run("CREATE users = { email: 'ada@example.com', city: 'Paris' };")?;

let found = session.run(
    "SELECT email, string::upper(city) AS city
       FROM users
      WHERE email LIKE 'ada%' AND city = 'Paris';",
)?;
```

The two ways of opening differ in where the bytes live and in nothing else.

A caller who has a value does not write it into the script. `$name` stands
wherever a literal stands, and the value travels beside the script:

```rust
use tessaridb::{Parameters, Value};

let mut given = Parameters::new();
given.insert("city".to_owned(), Value::String("Paris".to_owned()));

let found = session.run_with("SELECT * FROM users WHERE city = $city;", &given)?;
```

A parameter is legal exactly where a literal is and nowhere a name is, and it is
replaced *after* the script is parsed — so whatever a caller supplies, it cannot
be read as grammar.

Every way in carries them: `Client::run_with` over the wire, where the values
travel in the store's own codec, and `tessaridb --param who='ada' -e '…'` at the
console, where a value is written as TessariQL and parsed on its own.

## Backing up

A backup of this store is its **log**, because the records, the indexes, the
catalog, the search statistics and the vector graph are all derived from it by a
pure function (ADR-0001). So a restore is a replay, through the same code a
replica runs.

```
tessaridb ./data --backup ./monday.tessarilog
tessaridb ./restored --restore ./monday.tessarilog

tessaridb --verify ./monday.tessarilog                       # changes nothing, needs no store
tessaridb ./data --backup ./tuesday.tessarilog --from 4001   # only what happened since
tessaridb ./restored --restore ./monday.tessarilog --upto 3000
```

`--verify` reads a backup and says what it holds, applying none of it and opening
no store, so a schedule can run it. `--from` writes an **incremental** whose
header names what it continues from, and a restore onto a store standing
somewhere else is refused rather than silently producing a store no log explains.

**A store's `*.log` files are not logs to tidy away — they are its newest data.**
Removing the live one discards every write since the last flush, silently: the
store opens, answers, reports no failure, and is simply an earlier store.

**What this makes testable is worth more than the feature.** If a restored store
differed from the original anywhere, something would not be derived from the log
— so the acceptance test restores a store that has exercised every engine and
compares the two keyspace by keyspace, byte for byte. One key is excluded on
purpose: a node's own identity is not derived from the log, so a restore onto a
fresh machine produces a *different* node rather than a second process answering
to the first one's id.

Timed on 2 004 records, on the machine and build the [benchmarks](benchmarks)
record: **1.1 ms to write, 9.8 ms to replay**. A timing with no machine and no
date beside it is not a measurement, which is why those are here.

[Operations, in full](https://docs.tessaridb.com/operations/backup-and-restore)

## Using one

This README covers what TessariDB is, what runs, and how to open one. Everything
about *operating* a node lives in the documentation, which is generated from the
engine and stays current in a way a second copy here would not.

| | |
|---|---|
| A first store, step by step | [docs.tessaridb.com/start](https://docs.tessaridb.com/start) |
| The command line and the prompt | [/reference/command-line](https://docs.tessaridb.com/reference/command-line) |
| In a container | [/start/in-a-container](https://docs.tessaridb.com/start/in-a-container) |
| HTTP, WebSocket and the file routes | [/clients/http](https://docs.tessaridb.com/clients/http) |
| The binary wire protocol | [/clients/protocol](https://docs.tessaridb.com/clients/protocol) |
| Serving, health, drain and metrics | [/operations/serving](https://docs.tessaridb.com/operations/serving) |
| The language, statement by statement | [/query-language](https://docs.tessaridb.com/query-language) |

Two copies of one explanation drift, and the reader finds the stale one. That is
why the manual is in one place and this file points at it.

## Who it is for

- **AI agents and AI products**, whose working set spans documents, a knowledge
  graph, embeddings, exact text and attached files — and which need all of that
  to commit together or not at all.
- **Anyone about to stand up their third datastore.** If the design document
  says "Postgres for records, a vector index for recall, a search cluster for
  text, object storage for blobs", that is four operational surfaces, four
  backup stories, four auth models and zero transactions across them.
- **Real-time products**, where something has to know what changed the moment it
  changes, without a polling loop pretending to be a subscription.
- **Embedded and edge use**, where the same engine that runs as a node also
  links straight into the binary with no server at all.

The first consumer is an agent-memory and project-governance layer that needs
documents, a knowledge graph, vector retrieval and full-text search over the
same data, with live subscriptions and real concurrent access. TessariDB is not
built only for that — it is a general-purpose database that happens to have a
demanding first user.

## Design goals

| Goal | What it means here |
|---|---|
| **Multi-model** | Documents, graph edges, relational tables, keys, files, vectors, full-text, time windows, geometry and a vault — over one record store, not bolted together |
| **One query language** | **TessariQL** — a single surface for every model, including graph traversal and vector search ([the language reference](docs/tessariql.md)) |
| **Pluggable storage** | Everything above the key–value layer is written against one trait, and two backends prove it: in memory, and durable on a log-structured merge-tree engine |
| **Transactional** | Real transactions with a declared isolation level, not best-effort batching |
| **Real-time** | Change subscriptions as a first-class feature, not polling |
| **Honest about cost** | An index changes what a read costs and never what it answers — and where that cannot hold, as with an approximate vector search, the read says so in its own result |
| **Specified, not just implemented** | The wire and value protocol is published with a shared conformance corpus, so a client is written from the spec rather than from our source |
| **Deployable three ways** | Embedded library · single self-hosted node · multi-node cluster with sharding and replication |
| **Multiple interfaces** | CLI, HTTP + WebSocket, and a socket protocol for connection, control and maintenance |

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
  tessari-types        leaf   the value system, record ids, newtypes
  tessari-constants    leaf   tunables, each with unit and rationale
  tessari-geo          leaf   geometry on a fixed integer grid: exact predicates, boxes
  tessari-kv                  key-value contract, atomic conditional batches, in-memory backend
  tessari-lsm                 persistent backend, durability levels, engine options
  tessari-encoding            key grammar and value codec over the KV layer
  tessari-storage             records, transactions, indexes (snapshot isolation on the log)
  tessari-ql                  TessariQL: lexer, parser, AST
  tessari-query               a typed query builder that builds the syntax, never the text
  tessari-session             running a script: catalog, planning, execution, permissions
  tessari-vault               sealed values: the key hierarchy and the authenticated envelope
  tessaridb                     the embedded front door — open, run, follow the changes
  tessari-http                the HTTP and WebSocket surface
  tessari-wire                the wire protocol
  tessari-serve               stopping a serving process in the order the stages require
  tessari-ingest              running the declared stream consumers: source, shaping, runner
  tessari-backup              log as backup, replay as restore
  tessari-conformance         the executable definition of TessariQL: corpora and runner
  tessari-cli          bin    `tessaridb` — a prompt and a script runner
  tessari-bench        bin    workload harness, exact percentiles, recorded baselines
```

Cluster membership, sharding and replication have no crate yet; they will be
derived from the feature matrix when the node grows past one. The list above is
what exists, not a plan.

## Building

**What you need beyond Rust.** The on-disk backend links a log-structured
merge-tree engine that is **compiled from C++ source**, and its bindings are
generated at build time by loading `libclang`. So a first build needs a C++
toolchain and libclang present, and it takes several minutes — after which they
are cached and rebuilds are ordinary.

| | |
|---|---|
| Rust | 1.85 or newer (`rust-version` in `Cargo.toml`); the toolchain file asks for `stable` |
| macOS | `xcode-select --install` — the Command Line Tools carry both |
| Debian · Ubuntu | `apt install build-essential clang libclang-dev` |
| Fedora · RHEL | `dnf install gcc-c++ clang clang-devel` |

```sh
cargo build --workspace
cargo test --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

`cargo test --workspace` builds around 216 test binaries. Two of them bind fixed
ports and must not run beside a second copy of themselves.

## Branches

- `main` — stable branch. Releases and tags come from here.
- `dev` — integration branch. Feature branches merge here first.

## Clients

The [protocol](https://github.com/TessariDB/TessariDB-protocol) is a repository
of its own: the wire and HTTP specification plus a conformance corpus that every
client is tested against. It is Apache-2.0, and a client written from it depends
on nothing in this repository.

- **Rust** — [TessariDB-sdk-rust](https://github.com/TessariDB/TessariDB-sdk-rust)
- Other languages: write one from the spec. That is what it is for.

## Provenance

TessariDB is an independent implementation. Its architecture is informed by the
published literature on multi-model storage, LSM and B-tree engines, query
planning, vector and full-text indexing, and distributed transactions — ordinary
engineering practice. No code, grammar file, test fixture, schema or identifier
in this repository is copied or mechanically translated from another database's
source, and no third-party database is vendored, linked, or derived from here.

## Licence

TessariDB is **source-available** under the
[Business Source License 1.1](LICENSE). The source is public, and on
**2030-09-07** — or four years after any given version is first published,
whichever comes first — that version becomes **Apache-2.0** permanently.

**Free, with no agreement and no charge**, for any use — including production,
including inside a commercial organisation, and including inside a product you
sell.

**One restriction.** You may not provide TessariDB to third parties as a
**database service**: a product, service or platform in which TessariDB, or a
derivative of it, gives database functionality to people other than your own
employees and contractors, where those people can create, manage or control
namespaces, databases, tables or schemas. That needs a commercial licence or
written permission. Write to
**[licensing@tessaridb.com](mailto:licensing@tessaridb.com)** or see
[tessaridb.com/licensing](https://tessaridb.com/licensing); we are
straightforward to deal with.

The client SDKs and the protocol specification are **Apache-2.0** on purpose:
whatever licence the server carries, nothing should constrain the applications
that talk to it, or anyone who wants to write a client in another language.

Contact: [hello@tessaridb.com](mailto:hello@tessaridb.com) ·
security reports to [security@tessaridb.com](mailto:security@tessaridb.com).

Copyright (c) 2026 boogvar.
