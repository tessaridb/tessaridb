<div align="center">

<img src="assets/logo/tessaridb-mark-256.png" alt="" width="112" height="112">

# TessariDB

**The stack around the model, in one store.**

Eleven engines. One transaction. One binary. A real-time multi-model database,
written in Rust, for AI applications and the products built around them.

[![status](https://img.shields.io/badge/status-in%20development-D98E33?style=flat-square)](#status)
[![version](https://img.shields.io/badge/version-0.29.0--beta-6B5FD1?style=flat-square)](#status)
[![licence](https://img.shields.io/badge/licence-BUSL--1.1-6B5FD1?style=flat-square)](LICENSE)
[![rust](https://img.shields.io/badge/rust-1.98%2B-6B5FD1?style=flat-square)](Cargo.toml)
[![conformance](https://img.shields.io/badge/conformance-1586%20cases-6B5FD1?style=flat-square)](crates/tessari-conformance/tests/corpus)

[tessaridb.com](https://tessaridb.com) · [docs](https://docs.tessaridb.com) ·
[protocol](https://github.com/tessaridb/tessaridb-protocol) ·
[clients](#clients)

</div>

> [!NOTE]
> **TessariDB is a beta — `0.29.0-beta`.** It is released and tested, published as
> a container image (`tessaridb/tessaridb:0.29.0-beta`; the image tracks the
> larger releases), and the licence makes production use free, including inside
> a commercial company.
> What a beta does not promise yet is permanence of the language and the wire:
> before 1.0 the query language and the wire format may still change, and several
> engines are still partial. The **on-disk format is held**: a store written by
> `0.22.0-beta` or any release after it opens under a newer one and reads back the
> same, and a store from a newer format is refused rather than opened. So pin a
> released version, and keep a backup you have restored.
> [**Status**](#status) says what runs today, engine by engine — it is a report,
> not a roadmap. The [**changelog**](CHANGELOG.md) says what each version is and
> what it is missing.

---

A project built around a model almost never needs one data system. It needs
search, a database for everything else and a queue; then a cache, a lock, an
event log, files, a map of where things are, the relations between them, and
somewhere for the secrets it must not leak. Each of those is usually one more
service to stand up, upgrade, back up, secure and keep in step with the others —
and there is no transaction across them, so the question is never *whether* they
drift, only when somebody notices. Most projects never load any one of them hard
enough to need the specialist; all of them pay for running the set.

TessariDB takes the other route: **one transactional record store, eleven access
models over the same records.** Documents, tables, edges, keys, files, terms,
vectors, time windows, geometry, queues, topics and sealed fields are not
separate systems bolted together — they are different ways of reading the same
committed bytes, under one snapshot, in one language.

| A project would run | Here |
|---|---|
| a database | records — declared or schemaless, joined, in transactions |
| a search engine | full-text — analysed, scored, prefix and fuzzy |
| a vector index, and a step that re-ranks the two | vectors, exact unless you ask otherwise, and `ORDER BY FUSE` by rank |
| a cache, a lock, a rate limit | a space — keys that expire, `INCR`, `SET … IF ABSENT`, a limit that evicts |
| a job queue | `DEFINE QUEUE` — claims that lapse when a worker dies |
| an event log | `DEFINE TOPIC` — each reader's position kept in the store |
| a geo database | shapes, radius reads, nearest-first, cells for a map |
| a graph database | edges as records, traversed |
| object storage | buckets of files, in the same transaction as their records |
| somewhere for secrets | a vault — fields sealed before they are stored |
| a time-series store, an audit trail | series with a retention floor, `VERSION`, the change feed |

**Not the fastest at any one of those jobs; enough at all of them.** A system
built for one job will beat a general store at it, and nothing here claims
otherwise — there is no published benchmark. What this removes is the cost of
running a dozen systems and the seams between them. It grows with the project on
the same binary — in memory, on disk, with followers, automatic failover, tables
split by key range and two writers on one range — and a project that outgrows one
part of it can move that part out later: the data is one log behind a published
protocol and five clients. The vault seals declared fields; it is not a general
secrets service with leases and rotation.

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

## Why this fits AI applications

Chunks are the easy part. An agent's working set is not one shape. Inside a single turn it wants the
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
  Words and meaning are ranked together with `ORDER BY FUSE`, by where each
  record came in each order — never by adding a relevance score to a distance.
- **State for the tools the model calls.** A session that times out, a rate-limit
  window, a lock that frees itself and a cache of answers that cost money to
  produce are keys in a space — `EXPIRE`, `INCR`, `SET … IF ABSENT`, `MAX n` —
  in the same transaction as the records they describe.
- **Events without a broker beside the store.** A topic keeps every message in
  commit order at dense positions, and a reader's position moves in the reader's
  own transaction — so an effect it writes into this store happens once per
  message. An effect outside the store (an email, a call) is at least once.
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

**Twelve rows, eleven engines.** A row is a corpus rather than an engine: three
of them — Documents, Relational and References — are three faces of the record
store rather than three stores, and queues and topics share one row. The
[engines page](https://docs.tessaridb.com/overview/engines) lists the eleven.

| Engine | What it gives you | Cases | State |
|---|---|---|:--|
| **Documents** | schemaless or schemafull records, nested objects and arrays, typed fields with defaults, a document filtered by the sub-document it contains — through a containment index that changes the cost and never the answer — JSON text read into a document and written back out | 71 + 85 + 16 | ✅ runs |
| **Relational** | declared tables and fields, unique and multi-field indexes, joins whose answer an index may not change, `INSERT` of several records in one statement at identities the store produces | 63 + 27 + 51 + 21 | ✅ runs |
| **Graph** | edge tables, `RELATE`, properties on the edge, multi-hop traversal in both directions, an edge table that names the pair it joins and refuses every other, a declared graph that holds its own records with no table declared beside it and takes them with it when dropped, tables you already have joining it with `IN`, `DEFINE EDGE` writing adjacency beside the node so a hop is a range read, an edge removed by the pair it joins, `DEPTH n` bounding a repeated hop, and `PATH TO … DEPTH n [WEIGHT f]` answering the shortest or cheapest path within that bound | 76 | ✅ runs |
| **Key–value** | `SPACE`s — one key, one whole value, a per-key expiry (`EXPIRE`, `TTL`, `PERSIST`), atomic `INCR` and conditional `SET … IF`, and a seeking key walk by range or prefix with `AFTER`/`LIMIT` paging, and a key limit (`MAX n`) that evicts the least recently modified or refuses, on memory and on disk | 33 | ✅ runs |
| **Objects & files** | `BUCKET`s — bytes addressed by path, byte-range reads, writes at an offset, metadata that is an ordinary record, and a declared ceiling on the largest file the bucket takes | 38 | ✅ runs |
| **Full-text** | per-field analyzers, whole-term search, prefix search a reader is served by while still typing — a starred word ranked as it is typed and a phrase ending in one — fuzzy search that survives a typo or a swapped pair of letters, measured against the word the text held rather than its stem, lowercase · ASCII folding · Porter2 stemming and Snowball Russian, German, French and Spanish, a quoted phrase with declared slop that widens the window without relaxing the order, `OR` and `NOT` inside a query, per-field weighting written as arithmetic, a `did you mean` suggestion that is a field beside the records and never a substitution into the query, and highlighting that marks the token the read actually reached rather than the characters that were typed, `search::explain` saying what each word of a score contributed, ranked pages that resume below their anchor rather than re-reading the table, and `POSITIONS` / `OFFSETS` / `NO SCORE` on the index as costs that never change an answer, `MATCHES INFIX` from a suffix structure over the dictionary rather than n-grams, and `DEFINE SEARCH` — several fields of several tables ranked as one collection by BM25F, per-field weights and operators, query-time synonyms and stop words, snippets, ranked type-ahead and facets, scored from its postings alone when they can decide the query, Chinese and Japanese searchable one ideograph to a token, and a space of plain text searched through its `value` | 126 | ✅ runs |
| **Vector** | cosine, Euclidean and dot distance, kNN ordering, a graph index that declares whether it answered exactly, and a field that declares how wide its vectors are so a write of any other width is refused where it happens, and a vector store declared as one so the width, the index and the requirement cannot come apart, and a read that says what it will spend on the walk, and a recall the store reports only once something has measured it, and a filtered nearest read walked through the graph with the whole condition tested on every record it answers with, and an index that keeps one byte per component and rescores on the full vectors | 56 | ✅ runs |
| **Vault** | a store whose declared fields can be `SECRET`, sealed under a per-record key before the record is encoded so the index, the change feed, the replication log and a backup all carry ciphertext, read only by `REVEAL` naming one record, sealed and unsealed by a passphrase that reaches memory and never disk, and dropped by destroying the key rather than the rows, with an opaque recipient set the engine carries and never reads, and a strictness a vault cannot be talked out of because a field nobody declared is a field nothing seals, with a trail every `REVEAL` writes to and `INFO FOR AUDIT` reads back, and edited field by field so that rotating a secret keeps the recipients it was shared with, an unseal that lasts a period and a passphrase that can change — the store's, or the vault's own so the store's passphrase opens nothing in it | 67 | ✅ runs |
| **Time-series** | `DEFINE SERIES` — a table with a declared retention, past which a record is not answered with while its bytes are still there and its removal is a separate act, epoch-anchored windows every process agrees on, aggregates per window, and retention as a statement over any table that reports what it removed; ordered by event time with `TIME`, windows filled over a stated range, the newest record per key, `ASOF JOIN`, counter folds, rollups kept by the writes, aged records removed as one range, and batches of events appended over HTTP | 27 | ✅ runs |
| **References** | `FETCH` — follow a reference, an array of them, or a nested route, without a join | 13 | ✅ runs |
| **Queues & topics** | `DEFINE QUEUE` — work handed out one holder at a time under a hold that lapses, first-in-first-out by identity, by a declared priority, or on the record you name, held back until a declared instant, an attempt ceiling whose dead letter is a predicate rather than a second table, a claim that is an ordinary write so it replicates, recovers and needs no lease manager, and a claimant a session declares so it can hand back everything it holds and nobody else's, by name or one record at a time, on a strict table or a loose one, and work a holder may compare-and-set without losing the hold — declared strict or lenient and in a graph or in none, so a queue is an end of a link like any other table, and a hold that no write can drop by saying nothing about it; and `DEFINE TOPIC` — an append-only order whose messages hold dense positions decided at commit, whose named readers keep their place in the store and move it in their own transaction, whose retention — by age or by bytes kept — tells a reader how much it missed, and which a topic declared `PUBLIC` lets a caller nobody signed in append to at a declared rate; and `DEFINE GROUP` — workers sharing a topic, each message held in flight until it is acknowledged, handed out again on a negative acknowledgement or a passed deadline, and dead-lettered past its deliveries; and `DEFINE TOPIC CONSUMER` — a topic read into a table through a group, each message applied exactly once in the transaction that acknowledges it | 71 + 37 | ✅ runs |
| **Geospatial** | a geometry type on an exact integer grid, eight predicates over whole shapes, geodesic distance and area, shapes written as literals, a spatial index seven of the eight predicates and a radius read go through, a nearest-first read over positions and areas and under a `WHERE`, distance between any two shapes to their nearest points, counting by cell, a geo store declared as one so the field, the index and the requirement cannot come apart, a measured query covering, and a measured refinement ratio saying what that index's candidates cost | 74 | ✅ runs |

Underneath all of them, one substrate with two backends: **in memory**, and
**on disk** on a log-structured merge-tree engine. Everything above the
key–value layer is written against a single trait, which is what makes the pair
possible rather than aspirational.

## And around them

| | |
|---|---|
| **Transactions** | snapshot isolation on the commit log, `BEGIN` · `COMMIT` · `CANCEL` |
| **Events** | `DEFINE EVENT … ON t [FOR CREATE, UPDATE, DELETE] [WHEN …] THEN …` — statements run after each write of a record, in the writer's transaction and as the writer, seeing `$before` and `$after`; a refusal in the body refuses the write, a chain is bounded at 16, and work after the commit is a topic appended in the same commit |
| **Materialized views** | `DEFINE VIEW … MATERIALIZED` — a read's answer kept as records, brought current from its source's change feed in the writer's order and always equal to the read at the version it states; `INFO FOR TABLE` says how far behind it is |
| **A planner that estimates** | `ANALYZE TABLE` — per-index statistics (common values, distinct counts, equi-depth buckets) a serving node also keeps fresh itself; two indexes ranked by the records each produces, an index that returns most of a table losing to it, and `EXPLAIN` saying what it estimated and from what |
| **Real-time** | change subscriptions as a first-class feature — over the wire and over a WebSocket |
| **Stream ingestion** | `DEFINE KAFKA CONSUMER` — one statement says what to read, where it lands and under which group, and the node runs it; at-least-once, never exactly-once |
| **Multi-tenant** | namespaces and databases, users, roles, `GRANT` and `REVOKE` per database |
| **Four ways in** | embedded library · `tessaridb` CLI · HTTP + WebSocket · a framed binary wire protocol |
| **Operable** | health and readiness endpoints, Prometheus metrics, graceful drain, log-as-backup with replay-as-restore |
| **Specified** | the wire and value protocol is [published](https://github.com/tessaridb/tessaridb-protocol) with a shared conformance corpus, so a client in any language is written from the spec and not from our source |

### Two writers on one range

A namespace says how many nodes may write it. Declare `MULTI MASTER` and two
nodes may each accept writes to the same record — which gives up single-copy
semantics: two versions can exist of which **neither is newer**, and no clock
settles it, because the nodes' clocks are not comparable and the order versions
arrive somewhere is the order that node heard about them.

The engine does not guess. A write onto a record in that state is **refused, and
names both versions and a node whose write one of them has not seen** — enough to
read both and write what they mean. Refusing is the point: once a write lands on
top of a contested record, nothing afterwards can tell a record that was
reconciled from one that was silently ranked.

A table may choose otherwise. `LAST WRITER WINS` takes the write instead of
refusing it, and the cost is stated rather than implied: the versions it did not
see are discarded — still on disk, gone from every answer — so the engine counts
each one and the count is readable. `INFO FOR VERSIONS OF person:1` returns every
surviving version and the node that wrote it.

## Status

**Stage: active development · `0.29.0-beta` · not published to crates.io.** What
follows is what runs today, not a roadmap.
<!-- absent: published-to-crates-io -->

- ✅ **Runs:** the embedded library, the `tessaridb` command line, the HTTP and
  WebSocket surface, the binary wire protocol (v1.0, with a published spec and
  conformance corpus), single-node serving with roles, endpoints and graceful
  drain, backup and restore.
- ✅ **Clusters:** a node joins a running cluster with one flag and declares no
  membership of its own; leadership is elected by a majority of the coordinating
  members and held on a renewable lease, so a leader that has lost the cluster
  **refuses writes before the cluster may replace it**; a namespace declares how
  many copies are kept and how many nodes may write it; a read may name how stale
  an answer it will accept, and that bound decides which nodes may answer rather
  than labelling the answer it gets; a read may instead require the **leader** to
  answer it, which no staleness bound can express because a follower at zero lag
  is level rather than authoritative; **how long the cluster waits before
  replacing a leader is a replicated policy** an operator writes once with
  `DEFINE FAILOVER` rather than a file each node holds its own copy of; a write
  is acknowledged once a majority holds it (`ACKNOWLEDGE MAJORITY`, the default
  wherever there is more than one copy); a write for a range another node leads
  is redirected there, or carried over the peer link for a caller that cannot
  follow; a transaction across two leaders commits whole when it asks to; and a
  namespace declared `MULTI MASTER` admits writes on more than one node, where a
  write concurrent with the stored version is **refused and named** unless the
  table declares `LAST WRITER WINS`, in which case what it discards is counted.
  See [Clustering](https://docs.tessaridb.com/cluster/what-a-cluster-is).
- ✅ **A cluster that trusts nothing in the clear:** peers speak mutual TLS with a
  certificate issued for each node's id, renewed from its files without a restart
  and revoked with `REVOKE CERTIFICATE` — a revocation or an expiry ends even a
  stream already open; clients are served over TLS 1.3 when the node is given a
  certificate, in the clear otherwise and said so at start, and
  `--require-client-tls` makes a node refuse to start without one; a request sent to a node that
  cannot answer it is carried over the peer link as a signed assertion of who
  asked, never a password; a joining node is approved by its id, its certificate
  fingerprint or a one-time join token; and the store can be encrypted at rest,
  its backups sealed under the same key. The built-in console builds such a
  cluster from empty to serving.
- ✅ **The storage and backup layer settled around that.** The log is kept **per
  range** rather than per store, so a key carries its home and the sequence and
  the leadership that wrote it are properties of a range instead of the whole
  machine; a store written by an older build is rewritten once at open. A record's
  **version is its own counter**, no longer doubling as the log position a replica
  resumes from and the applied-position guard — three jobs one number could only
  hold while a single leader made every timeline the same timeline. A backup file
  therefore carries **one section per log**, and a backup or restore bounded by a
  single sequence (`--from`, `--upto`) **refuses a multi-log store** rather than
  presenting part of it as the whole. One correctness fix belongs here too: index
  maintenance selected definitions **by table id alone**, and ids are handed out
  store-wide — so a catalog write could select an unrelated user table's indexes
  and write into that user's keyspace. It is selected by the whole tenancy now. The
  log can now be **bounded** — `DEFINE NODE RETAIN <n> RECORDS` keeps the newest
  *n* per log on this machine and prunes the rest, **default off**, with a reader
  below the horizon refused by name rather than served a short answer.
- ✅ **Geospatial** can store a shape, answer eight predicates over whole
  shapes, measure geodesic distance and area, and be written as a literal in a
  script. `DEFINE INDEX … SPATIAL` writes and maintains a **spatial index** —
  the cells covering each geometry, with the record's bounding box in each entry
  — and **seven of the eight predicates read through it**: the query shape is
  covered by cells of its own (sixteen, a budget measured on a skewed corpus),
  the entries under and above them are read, the stored boxes reject what they
  can, and the exact predicate decides the rest. `geo::disjoint` is the
  complement of a region and stays an exact scan by design. The same index
  answers **the nearest few** — `ORDER BY geo::distance(at, …) LIMIT k`, over
  points or areas and under a `WHERE` — by walking cells and records
  cheapest-first and measuring each record with the statement's own expression,
  so the order is the scan's to the last digit. `geo::distance` measures between
  **any two shapes**, to their nearest points; a **radius read**
  (`geo::distance(at, …) < r`) is served through the query's box widened by `r`;
  and `geo::cell(at, n)` answers the index's own cell as a polygon to group by.
- 🚧 **Partial:** a read that runs whole on every shard's node. A table can
  be split by the identities of its records (`SPLIT AT`), and each shard is
  logged, replicated and — where a member row places it (`LEADS`) — elected and
  led on its own node, so writes to two shards are taken by two nodes. Shards
  split and merge while the table serves, by hand (`ALTER TABLE … SPLIT AT`,
  `MERGE SHARD`) or by the cluster within stated bounds (`SPLIT AUTOMATICALLY`),
  and the store's leader can even out who leads what (`BALANCE LEADERSHIPS`). A
  transaction writing ranges two nodes lead commits whole when it asks to
  (`COMMIT ACROSS LEADERS`). A table partitioned by a field (`PARTITION BY
  region`) keeps each region's records in its shard, and a read naming the
  region touches only that shard. A node holding only some shards answers a
  read of the rest from those shards' leaders — a `WHERE`, a `LIMIT` with or
  without an `ORDER BY`, `count`/`sum`/`mean`/`min`/`max` and
  `variance`/`stddev` (floats included, summed exactly) are worked out there,
  under the caller's visibility, and a join side or a `FETCH` into a shard it
  lacks is gathered the same way, narrowed to the keys the near side holds.
  What still fetches the records is a fold that does not merge — `median`,
  `collect` and the counter folds.
  <!-- absent: holistic-folds-on-the-leaders -->
- ✅ **The on-disk format holds across versions:** a store written by
  `0.22.0-beta` or any release since opens under this build and reads back what the
  build that wrote it answered — tested against a store each of those releases
  wrote, its indexes included — and an older layout is rewritten at open where it
  has to be. A store from a newer format, or one holding data with no format
  stamp, is refused by name and left untouched. The format is written down
  ([`docs/key-grammar.md`](docs/key-grammar.md),
  [`docs/value-system.md`](docs/value-system.md)) and a test fails when the code
  and the documents disagree, or when the format changes without its version
  moving. Going back to an older build is not promised. A write the engine could
  not make durable stops the store until it is reopened and recovers from its log
  ([`docs/storage-contract.md`](docs/storage-contract.md)).
- 🔄 **Not promised yet:** before 1.0 the query language and the wire format may
  still change.

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

**A store that leads more than one range holds more than one log, and a backup
file carries one section per log.** A whole backup and a whole restore are
unaffected. A *bounded* one is not: `--from` and `--upto` name a single sequence,
and a sequence means nothing across several logs — so those forms **refuse** a
multi-log store instead of quietly backing up one range and calling it the
store.

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

- **AI applications and the products around them**, whose working set spans
  documents, a knowledge graph, embeddings, exact text, attached files, tool
  state, background work and events — and which need all of that to commit
  together or not at all.
- **Anyone about to stand up their third datastore.** If the design document
  says "a relational store for records, a vector index for recall, a search
  cluster for text, a cache, a broker and object storage for blobs", that is six
  operational surfaces, six backup stories, six auth models and zero
  transactions across them.
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
| **Multi-model** | Documents, graph edges, relational tables, keys, files, vectors, full-text, time windows, geometry, queues, topics and a vault — over one record store, not bolted together |
| **One query language** | **TessariQL** — a single surface for every model, including graph traversal and vector search ([the language reference](docs/tessariql.md)) |
| **Pluggable storage** | Everything above the key–value layer is written against one trait, and two backends prove it: in memory, and durable on a log-structured merge-tree engine |
| **Transactional** | Real transactions with a declared isolation level, not best-effort batching |
| **Real-time** | Change subscriptions as a first-class feature, not polling |
| **Honest about cost** | An index changes what a read costs and never what it answers — and where that cannot hold, as with an approximate vector search, the read says so in its own result |
| **Specified, not just implemented** | The wire and value protocol is published with a shared conformance corpus, so a client is written from the spec rather than from our source |
| **Deployable three ways** | Embedded library · single self-hosted node · multi-node cluster with replication, elected leadership and a read that may name how stale an answer it accepts |
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

Cluster membership, leadership and replication have no crate of their own: they
live in `tessari-wire` and `tessari-storage`, beside the wire and the log they
are written in terms of, and so does splitting a table into shards. The list
above is what exists, not a plan.

## Building

**What you need beyond Rust.** The on-disk backend links a log-structured
merge-tree engine that is **compiled from C++ source**, and its bindings are
generated at build time by loading `libclang`. So a first build needs a C++
toolchain and libclang present, and it takes several minutes — after which they
are cached and rebuilds are ordinary.

| | |
|---|---|
| Rust | 1.98 or newer (`rust-version` in `Cargo.toml`); the toolchain file asks for `stable` |
| macOS | `xcode-select --install` — the Command Line Tools carry both |
| Debian · Ubuntu | `apt install build-essential clang libclang-dev` |
| Fedora · RHEL | `dnf install gcc-c++ clang clang-devel` |

```sh
cargo build --workspace
cargo test --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

`cargo test --workspace --tests` builds 60 test targets (`cargo metadata
--no-deps` counts them without compiling). Two of them bind fixed ports and must
not run beside a second copy of themselves.

## Branches

- `main` — stable branch. Releases and tags come from here.
- `dev` — integration branch. Feature branches merge here first.

## Clients

The [protocol](https://github.com/tessaridb/tessaridb-protocol) is a repository
of its own: the wire and HTTP specification plus a conformance corpus that every
client is tested against. It is Apache-2.0, and a client written from it depends
on nothing in this repository.

Five clients are written from it, each at `0.2.0`:

- **Rust** — [tessaridb-sdk-rust](https://github.com/tessaridb/tessaridb-sdk-rust)
- **Python** — [tessaridb-sdk-python](https://github.com/tessaridb/tessaridb-sdk-python)
- **TypeScript** — [tessaridb-sdk-js](https://github.com/tessaridb/tessaridb-sdk-js)
- **Go** — [tessaridb-sdk-go](https://github.com/tessaridb/tessaridb-sdk-go)
- **Kotlin** — [tessaridb-sdk-kotlin](https://github.com/tessaridb/tessaridb-sdk-kotlin)

Another language: write one from the spec. That is what it is for.

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
**2030-10-04** — or four years after any given version is first published,
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
security reports to [security@tessaridb.com](mailto:security@tessaridb.com) ([SECURITY.md](SECURITY.md)).

Copyright (c) 2026 boogvar.
