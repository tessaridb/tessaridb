# bgv-db

A multi-model database written in Rust.

`bgv-db` stores documents, graphs, relational tables, vectors, full-text and
time-series data in one engine, behind one query language — **bgvQL** — over a
pluggable key–value substrate. It runs as an embedded library, as a single self-hosted
node, or as a cluster that distributes both data and engine roles across nodes.

> **Status: pre-alpha.** Embedded use works today; the node, the network
> interfaces and the cluster do not exist yet. Interfaces, the query language
> and the on-disk format are all unstable and will change without notice.

## Opening one

```rust
use bgv_db::Db;

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

session.run("CREATE users:1 = { email: 'ada@example.com', city: 'Paris' };")?;

let found = session.run(
    "SELECT email, string::upper(city) AS city
       FROM users
      WHERE email LIKE 'ada%' AND city = 'Paris';",
)?;
```

The two ways of opening differ in where the bytes live and in nothing else.

## Talking to one over HTTP

```rust
use std::sync::Arc;
use bgv_db::Db;
use bgv_db_http::Node;

let db = Arc::new(Db::open("./data")?);
let node = Node::bind(db, "127.0.0.1:8080")?;
node.serve();
```

```sh
curl -s localhost:8080/health
# {"status":"ok","committed":12}

curl -s localhost:8080/script --data-binary '
  USE NAMESPACE prod DATABASE orders;
  SELECT email FROM users WHERE city = "Paris" LIMIT 2;'
# {"results":[{"kind":"done"},{"kind":"records","path":"index","records":[…]}]}
```

Two routes, and no REST resource tree over tables: that would be a second query
language expressed in URLs, and it could say less than the one above. **The
language is the API.** Each request is its own session and each answer carries
the access path that served it, so a scan is visible rather than folklore.

A store with **no users declared is open** and runs anything, which is what keeps
an empty one usable. The first `DEFINE USER` closes it, and from then on a
request signs in with `Authorization: Basic`:

```bash
curl -s localhost:8080/script -u root:'a long one' --data-binary '
  USE NAMESPACE prod DATABASE orders;
  SELECT email FROM users LIMIT 2;'
```

A closed store answers `401` to a request it cannot place and `403` to one whose
role forbids the statement — "I do not know you" and "I know you and no" are
different answers, and a client that cannot tell them apart retries a signin that
will never help.

> **Basic over plaintext is plaintext.** The password is in a header anything on
> the path can read, and this node terminates no TLS. Bind it to a loopback
> address or put a reverse proxy in front of it. Note too that a store cannot be
> re-opened from outside by dropping its last user — a lost owner password is a
> restore from backup, not a recovery.


Following what changes is a cursor over the same log that carries replication,
so it needs no setup and loses nothing by being slow:

```rust
use bgv_db::{Db, Sequence, Watch};

let mut watching = Db::subscribe(Sequence::ZERO, Watch::default());
for change in db.poll(&mut watching, 128)? {
    println!("{:?} {:?}", change.id, change.kind);
}
```

## Talking to one over the wire

```rust
use std::sync::Arc;
use bgv_db::Db;
use bgv_db_wire::{Client, Node};

let node = Node::bind(Arc::new(Db::open("./data")?), "127.0.0.1:9080")?;
std::thread::spawn(move || node.serve());

let mut client = Client::connect("127.0.0.1:9080")?;
let answers = client.run("SELECT * FROM users LIMIT 2;", None)?;
```

The same statements as the HTTP route, and the same sessions — what differs is
what a value is when it arrives. **JSON has six types and this store has
fifteen.** The HTTP endpoint pays that deliberately, because a browser is owed
JSON, and it quotes a decimal so it is not silently a double; a client reading
that back has to *guess* whether `"12.34"` is a decimal and `"2s"` a duration.
Here a value travels through the codec the store writes records with, so fifteen
types go out and fifteen come back and neither end decides anything.

Frames are `<kind> <length> <body>`, and a declared length above 16 MiB is
refused **before anything is allocated** — a length from a stranger is not a
promise, and allocating on one is the oldest denial of service there is. An
unknown frame kind closes the connection rather than being skipped, because a
protocol that ignores what it does not understand is one where a version mismatch
looks like silence. A *refusal* closes nothing: a client that mistyped a
statement has not stopped being a client.

It takes **no dependency at all** — `std::net`, the frames, and the encoding
crate — and it is synchronous, a thread per connection. That is the decision
rather than the shortfall: a commit is a compare-and-set, so an async server over
it would be `spawn_blocking` at every call, a thread pool wearing a runtime's
clothes. The cost is a thread per *connection*, which will matter when idle
subscribers outnumber what a thread each is worth, and that is the trigger for
revisiting it.

A connection holds **one session**, so `USE NAMESPACE prod;` is still in force in
the next statement — which is what a connection means, and what the thread it
costs is buying. Two connections are two sessions and share nothing but the
store.

An answer carrying records also carries the **names of the tables its references
point at**. A reference holds a table id and the name lives in the catalog, which
is on the server; without them a client renders `<record 3:7>`, and the point of
this protocol is that a client decides nothing.

> **There is no TLS here either.** Credentials travel as they were given. This
> belongs on a trusted network or behind something that terminates TLS, and says
> so rather than leaving it to be assumed.

Frame kinds above 3 are reserved for **subscription push**, which is why this is
framed rather than request-and-reply: a server that pushes has to be able to send
what the client did not ask for. It is not built yet.

## From a terminal

```
cargo install --path crates/bgv-db-cli    # installs `bgv`

bgv                                    an in-memory store, and a prompt
bgv ./data                             a store on disk, and a prompt
bgv ./data -e 'SELECT * FROM users;'   one script, then exit
bgv ./data -f setup.bgvql              a file
echo 'SELECT * FROM users;' | bgv ./data

bgv ./data --serve 127.0.0.1:9080      be a node
bgv --at 127.0.0.1:9080                a prompt against one
```

```
bgv> CREATE users:1 = { name: 'ada', joined: datetime '2026-01-15T09:30:00Z' };
ok
bgv> SELECT * FROM users;
1: { joined: datetime '2026-01-15T09:30:00Z', name: 'ada' }
(1 record(s), via scan)
```

Answers print in **bgvQL's own syntax**, so what comes out can be pasted back in.
JSON is what the HTTP endpoint speaks, and it had to decide how fifteen types
become six; a terminal is owed no such compromise.

A refusal at a prompt prints its message and the next statement runs; in a script
it stops, because carrying on past a failed step is how a half-applied migration
happens. Either way the exit code says what happened.

A path opens the store **in this process**; `--at` talks to a running node over
the wire protocol above. Both produce the same answers to the same renderer, so
what is printed does not depend on which was used — a test runs every answer
shape through both and compares the output character for character, which is the
claim worth testing rather than asserting. It is `--at` and not `--url` because
this protocol has no scheme, and calling an address a URL would promise one.

To sign in, `--user <name>`; the password comes from `BGV_PASSWORD` and never
from an argument, which the process table publishes and the shell history keeps.
`--backup`, `--restore`, `--health` and `--serve` work on a store this process
opened, so asking for one over an address is refused rather than quietly run
against a different store.

**There is no line editing or history** — both mean a dependency, and a terminal
library is a large surface to take for a convenience, so `.help` says so rather
than leaving it to be found by pressing up.

## Is it well

```
bgv ./data --health          # exits non-zero when it is not
curl -s localhost:8000/health
```

An engine does its compaction, its flushing and its write-ahead work on its own
threads, and a failure there surfaces at **no call a caller makes**: the store
keeps answering reads while the thing that keeps them has stopped. It is the one
failure this store cannot notice by being used, so something has to ask.

An unwell store answers `GET /health` with **503** and the complaint. That is
deliberately not a new alerting system — a 503 is taken out of rotation by every
load balancer and paged on by every monitor, so the alert is the one that already
exists rather than a second one written here and exercised never.

## Backing up

A backup of this store is its **log**, because the records, the indexes, the
catalog, the search statistics and the vector graph are all derived from it by a
pure function (ADR-0001). So a restore is a replay, through the same code a
replica runs.

```
bgv ./data --backup ./monday.bgvlog
bgv ./restored --restore ./monday.bgvlog
```

**A store's `*.log` files are not logs to tidy away — they are its newest data.**
Removing the live one discards every write since the last flush, silently: the
store opens, answers, reports no failure, and is simply an earlier store. A
write-ahead file the engine has already recorded is refused when it goes missing,
but the live one cannot be, because once it is gone there is nothing left to
notice with. Copy the whole directory, or use `--backup`.

A restore refuses a store that already holds something — merging a backup into a
populated store is not a restore, and the sequences would collide with a
different meaning. A file that has been cut short restores what it holds and says
so on the error stream, because a backup interrupted at record nine thousand is
still nine thousand records and refusing it outright would throw away what
somebody is holding in a bad week.

**What this makes testable is worth more than the feature.** If a restored store
differed from the original anywhere, something here would not be derived from the
log — so the acceptance test restores a store that has exercised every engine and
compares the two keyspace by keyspace, byte for byte.

Timed on two thousand records: 0.8 ms to write, 13 ms to replay.

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
| **One query language** | **bgvQL** — a single surface for every model, including graph traversal and vector search ([the milestone-1 subset](docs/bgvql.md)) |
| **Pluggable storage** | Everything above the key–value layer is written against one trait, and two backends prove it: in-memory, and durable on a log-structured merge-tree engine |
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
  bgv-db-types        leaf   the value system, record ids, newtypes
  bgv-db-constants    leaf   tunables, each with unit and rationale
  bgv-db-kv                  key-value contract, atomic conditional batches, in-memory backend
  bgv-db-lsm                 persistent backend, durability levels, engine options
  bgv-db-encoding            key grammar and value codec over the KV layer
  bgv-db-storage             records, transactions, indexes (snapshot isolation on the log)
  bgv-db-ql                  bgvQL: lexer, parser, AST
  bgv-db-session             running a script: catalog, planning, execution, permissions
  bgv-db                     the embedded front door — open, run, follow the changes
  bgv-db-http                the HTTP surface
  bgv-db-wire                the wire protocol
  bgv-db-backup              log as backup, replay as restore
  bgv-db-conformance         the executable definition of bgvQL: corpora and runner
  bgv-db-cli          bin    `bgv` — a prompt and a script runner
  bgv-db-bench        bin    workload harness, exact percentiles, recorded baselines
```

Cluster membership, sharding and replication have no crate yet; they will be
derived from the feature matrix when the node grows past one. The list above is
what exists, not a plan.

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
