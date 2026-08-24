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

A caller who has a value does not write it into the script. `$name` stands
wherever a literal stands, and the value travels beside the script:

```rust
use bgv_db::{Parameters, Value};

let mut given = Parameters::new();
given.insert("city".to_owned(), Value::String("Paris".to_owned()));

let found = session.run_with("SELECT * FROM users WHERE city = $city;", &given)?;
```

A parameter is legal exactly where a literal is and nowhere a name is, and it is
replaced *after* the script is parsed — so whatever a caller supplies, it cannot
be read as grammar.

Every way in carries them: `Client::run_with` over the wire, where the values
travel in the store's own codec, and `bgv --param who='ada' -e '…'` at the
console, where a value is written as bgvQL and parsed on its own.

## Files

A bucket is a table whose records are files, so everything the language already
does works on them — listing is a query, linking is a record reference, relating
is an edge, a grant on the bucket governs the bytes and the metadata together,
and a backup carries files because a backup carries the log.

```rust
session.run("DEFINE BUCKET media;")?;
session.run("PUT media:'/logo.png' = 0x89504e47;")?;
session.run("READ media:'/logo.png';")?;
session.run("CREATE users:1 = { name: 'ada', avatar: media:'/logo.png' };")?;
```

A caller who would rather speak HTTP can:

```text
PUT    /files/{namespace}/{database}/{bucket}/{path}   the body is the file
GET    /files/{namespace}/{database}/{bucket}/{path}   the body is the file
DELETE /files/{namespace}/{database}/{bucket}/{path}
GET    /files/{namespace}/{database}/{bucket}          what the bucket holds
```

These run the same statements through the same session, so an identity, a grant
and a refusal behave identically whichever way a caller comes in. The path
travels as a **value**, so a file may be called anything at all without any of it
becoming part of a statement.

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

A caller with a **value** to supply sends the script and the values together,
in a body the `Content-Type` marks as JSON. The value is written in bgvQL rather
than in JSON's own types — one value syntax, the one `--param` already uses, and
the one an answer prints back:

```json
{
  "script": "USE NAMESPACE prod DATABASE orders; SELECT email FROM users WHERE city = $city;",
  "parameters": { "city": "'Paris'" }
}
```

So `"3"` is the integer three and `"'3'"` is the text, and `dec 12.34` stays a
decimal instead of becoming a double on the way in. **A supplied value can never
be read as grammar**: binding happens after the script is parsed and before the
first statement runs, so `'; DROP TABLE users; --` is a string that says
something alarming rather than a statement. A value that is not a value on its
own is refused before anything runs. A plain body — no JSON content type — is
still just the script.

No REST resource tree over tables: that would be a second query
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

### A browser opens the same subscription at `/watch`

`GET /watch` upgrades to a WebSocket and carries the change feed. Send one
message saying what to follow, and changes arrive as they happen:

```js
const socket = new WebSocket("ws://localhost:8080/watch");

socket.onopen = () => socket.send(JSON.stringify({
  namespace: "prod", database: "library", table: "users", from: 0,
}));

socket.onmessage = (event) => {
  const change = JSON.parse(event.data);
  // {"sequence":4,"table":"users","id":"1","became":"written","value":{"name":"ada"}}
};
```

`from` is a position in the log, not "from now", so a client that was
disconnected resumes exactly where it stopped by sending back one more than the
last `sequence` it handled. `0` is everything the log still holds, and leaving
`table` out follows every table the session may read.

A browser cannot set an `Authorization` header on a `WebSocket`, so the follow
request may carry `user` and `password` instead; the header is used when a
client can send one. Both are a credential in the clear, which this store
already says of every route it serves — it has no TLS and belongs on a network
the operator protects. Against a store that has an owner, a follow carrying
neither is refused in words rather than left following nothing, so a client can
tell "not permitted" from "nothing has happened yet".

Following takes the connection over: a socket that is pushing is not also
reading requests. A client that wants both opens two.

The server half of RFC 6455 is written here rather than taken from a crate, and
the four things a hand-written one usually gets wrong are the four it is tested
on. A **close** is echoed with the code the client sent, so a clean end is not
reported as an error. A **ping** is answered with a pong carrying the same
payload, because an intermediary checks that before deciding the socket is alive.
A **fragmented** message is reassembled, including when a control frame arrives
between the pieces — which is legal, and which a loop written as "read until FIN"
gets wrong. And `permessage-deflate` is **declined** by being left out of the
answer: an extension neither implemented nor declined is one the client then uses.

A frame's declared length is checked against a ceiling before a byte is reserved
for it, and the reassembled message is bounded separately — a sender that
fragments without limit gets past a per-frame ceiling by construction. An
unmasked frame from a client ends the connection with `1002` rather than being
accepted, because masking is what stops a socket smuggling chosen bytes past
something on the path that only reads the start of a stream.

An upgraded socket is counted as a **feed** rather than a request, so a shutdown
drains what will finish and ends what will not, instead of waiting its full
deadline for a connection that was never going to close on its own.

### A console at `/`, served by the node itself

`GET /` is a page that runs bgvQL and watches a table. It is **in the binary**,
not a redirect to something hosted: a node on a private subnet, in a datacentre
with no route out, or on a laptop on a plane is exactly the case that most needs
an interface, and it is the case a redirect leaves with nothing.

So the page reaches nothing but the node serving it. No framework, no CDN, no web
font — a single remote reference would quietly take that property away, which is
why a test reads the served bytes and requires every URL in them to be
same-origin and answerable by this process.

It is a **client like any other**, with no route of its own: it runs scripts
through `POST /script` and follows changes through the `/watch` socket described
above, signing in with the same credentials `curl` would use. Anything the
console can do, a `curl` can do — and if a console feature ever cannot be
expressed against the public API, the API is missing something.

The console is a default-on Cargo feature. A size-sensitive build turns it off
and carries none of its bytes:

```sh
cargo build -p bgv-db-http --no-default-features   # `/` answers 404
```

Two things at v1, because they are the two worth having: run a script and read
the answer, follow a table and watch changes arrive. Values are rendered as text
and never as markup, so a record that happens to hold a `<script>` tag is data.
Credentials travel in the clear here exactly as they do on every other route —
this store has no TLS and belongs on a network you protect — and the page says so
rather than leaving it to be discovered.

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

Both ends live in this one crate, so a change to a frame breaks the other end at
compile time rather than in somebody's deployment. A client takes only its half:

```sh
cargo add bgv-db-wire --no-default-features   # the client, without the node
```

The default carries the server, which reaches the storage engine — so a client
built with it compiles the engine, the serving crate, and a password hasher for
credentials a client never hashes, in order to send a `SELECT` down a socket.
Turning the default off is the difference between 42 crates and 17, and nothing a
client calls lives behind the switch.

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

### Being told rather than asking

```rust
let feed = Client::connect("127.0.0.1:9080")?
    .follow(&Follow { from: last_seen + 1, table: Some("users".into()) })?;
```

This is why the protocol is framed rather than request-and-reply: a node that
pushes has to be able to send what the client did not ask for. Subscribing
**takes the connection over** — a socket delivering changes is not also answering
scripts, and letting it do both means multiplexing, a much larger protocol for a
case nobody has. A client that wants both opens two.

`from` is a position in the log, **inclusive**, so a subscriber that was
disconnected comes back with one more than the last change it handled and gets
exactly what it missed. `committed_tail() + 1` means "only what happens next";
`0` means everything the log still holds.

**A slow subscriber cannot lose anything** — it can only be behind. That is
inherited rather than promised: a subscription is a durable cursor over the log,
not a queue in front of it, so the buffer is the log, and the only way to lose a
change is to ask to (`skip_to`, which counts exactly what it discards).

Which decides what a client that stops reading altogether gets. Its socket fills,
the node's write blocks, and after thirty seconds that **connection** ends.
Nothing is buffered on its behalf and nothing is dropped: it reconnects from the
position it had. Buffering in the node instead would rebuild exactly the queue
the design removed, and its losses would be caused by memory pressure rather than
by a decision anyone made. Both halves are asserted — the feed ends short, and a
resumed subscription gets the rest.

A subscription **reads records**, so it answers to the same identity a `SELECT`
does: on a closed store an anonymous connection is refused, and a subscriber
signs in by running a request with credentials first — the session belongs to the
connection, so it is still signed in when it follows. And because the log holds
every namespace and database in the store, a subscription is confined to the one
the session selected. Without that, "watch everything" would mean rather more
than the caller who typed `USE` meant by it.

A pushed change names its table, for the reason an answer does. `LIVE SELECT` as
a bgvQL statement would be built on this and is not built.

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

bgv ./data --serve 127.0.0.1:9080 --http 127.0.0.1:8000
                                       one process, both surfaces, one store
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

`--serve` and `--http` name **one address each** rather than sharing one, so a
single process holds whichever subset you ask for and both listen over the same
store. Either may be given alone. An address handed to something that is not
serving is refused rather than ignored, because a port that was named and never
opened is worse than one that was refused — nothing tells you which happened.

**There is no line editing or history** — both mean a dependency, and a terminal
library is a large surface to take for a convenience, so `.help` says so rather
than leaving it to be found by pressing up.

## Stopping it

`SIGTERM` or `SIGINT`, and it stops in stages:

0. the node says **it is not ready** and goes on serving for five seconds, so
   whatever routes traffic to it can stop doing so while it can still answer,
1. every surface stops accepting new connections,
2. requests already in flight are given up to twenty seconds to finish,
3. subscriptions are ended — after the drain, because they never end on their
   own and waiting for one in step 2 would mean the drain never completes,
4. the store is closed, which flushes it and releases the file lock.

**A second signal exits immediately**, so a drain that hangs is not a trap.

New work is refused *before* existing work is interrupted, which is the whole
point of the order: a client mid-request is not punished for a deployment. A
subscriber loses nothing either way — its cursor is a position it holds, so it
reconnects exactly where it stopped.

Step 0 exists because a refused connection is not an answer. Without it the port
closes at the instant the node stops wanting work, so a load balancer meets a
dead socket and retries instead of routing elsewhere — the readiness route would
have nothing to report to and nobody able to ask. **The five seconds are paid by
every shutdown**, and a second signal skips them.

**It does not detach.** No fork, no pidfile. A database that daemonises itself
fights its supervisor: `systemd` loses readiness detection and the main PID,
Docker's PID 1 exits and takes the container with it, and Kubernetes reads that
as a crash loop. Run it in the foreground and let the supervisor supervise.

## Is it well

```
bgv ./data --health          # exits non-zero when it is not
curl -s localhost:8000/health   # is this store readable
curl -s localhost:8000/ready    # will this node take work right now
curl -s localhost:8000/metrics  # the numbers behind both
```

**Two routes because a supervisor acts on them in opposite ways.** A readiness
failure means *stop sending traffic*; a liveness failure means *restart it*.
Wire them the wrong way round and a node that is shutting down on purpose, or
whose disk has filled, gets restarted into the same state on a loop.

`GET /ready` answers **503** when the node is leaving — step 0 of the shutdown
above — and otherwise answers exactly what `/health` answers, because there is
one store and it has one opinion about itself. Neither route needs a credential.

Since `/health` carries that opinion, **it belongs on a rotation check rather
than on a restart check.** A background failure in the engine is not something a
restart clears.

`GET /metrics` is the Prometheus text format — plain text with a documented
grammar, so emitting it costs a function here rather than a dependency. Uptime,
the committed sequence, background errors, and per surface: connections in
flight, open subscriptions, answers, refusals and whether it is ready.

```
bgv_uptime_seconds 1841.402
bgv_committed_sequence 20418
bgv_connections{surface="wire"} 3
bgv_answers_total{surface="wire"} 91204
bgv_refusals_total{surface="wire"} 17
bgv_ready{surface="http"} 1
```

A **refusal** is a request the node answered with a failure instead of a result —
one definition, mapped by each surface onto its own vocabulary. It is not a
connection turned away during shutdown; that is a different number and this is
not it.

Also without a credential, for the third time the same reason: a scraper that
needs one is a scraper nobody configures. What it exposes is operational, with no
user data and no schema in it — and this store already says it has no TLS and
belongs on a network you trust.

An engine does its compaction, its flushing and its write-ahead work on its own
threads, and a failure there surfaces at **no call a caller makes**: the store
keeps answering reads while the thing that keeps them has stopped. It is the one
failure this store cannot notice by being used, so something has to ask.

An unwell store answers `GET /health` with **503** and the complaint. That is
deliberately not a new alerting system — a 503 is taken out of rotation by every
load balancer and paged on by every monitor, so the alert is the one that already
exists rather than a second one written here and exercised never.

## Which node am I talking to

```
bgv> SELECT * FROM $node;
```

```json
{ "id": "9d3f1a…", "roles": ["serving", "writable"],
  "membership": "alone", "version": "0.0.0", "endpoints": [] }
```

Through the ordinary read path — no new route, no endpoint, nothing a console is
handed privately. It needs no `USE`, because a node is not in a database, and
only an **owner** is answered: roles and endpoints are this machine's position in
a topology, and there is no smaller truthful version of them to show somebody
else.

The **id is generated once** and survives every restart, which is what makes it
an identity rather than a session token. It is sixteen bytes of real operating-
system randomness, and a store whose randomness source cannot be read **refuses
to open** rather than falling back to a clock or a process id — an id that might
collide fails silently, and a node that will not start says so once.

The **version moves**, and it is the only field here that does. It is rewritten
whenever the binary is replaced, which gives an upgrade a moment at which it is
visible: after the format is settled and before the store has served anything.
That moment is where a future data migration runs, and it is the only one where
the previous version is still readable.

`roles` is a **set**, not a mode, because the real cases combine: read-only is the
absence of `writable` rather than a flag of its own, which makes "a read-only node
forwards writes" a rule about roles instead of a second kind of state.

## Configuring it

There is no configuration file and no environment variable for what a node is for
or which peers it has. Both are statements, and what they write lives in the
store:

```
bgv> DEFINE NODE ROLES serving, writable ENDPOINTS 'db-1.internal:9000';
bgv> DEFINE REPLICA second AT 'db-2.internal:9000' ROLES serving, writable;
bgv> INFO FOR NODE;
```

```json
{ "id": "9d3f1a…", "roles": ["serving", "writable"], "membership": "alone",
  "version": "0.0.0", "endpoints": ["db-1.internal:9000"],
  "cluster": { "peers": [{ "name": "second", "endpoint": "db-2.internal:9000",
                           "roles": ["serving", "writable"] }] } }
```

A peer's `ROLES` is the same field as this node's, written about the other side,
and it is what a forwarded write is routed by: a node that may not write sends
the statement to the peer whose roles carry `writable`. Leaving the clause out
declares a peer with **no** roles, which takes no writes — the safe absence, since
an operator who forgot it gets a refusal naming the clause rather than a write
landing on a node nobody said could take one.

A node configured by a file beside a store configured by statements is **two
sources of truth for one node**, and they agree until the first restore.

The answer comes back as **two groups on purpose**. The flat fields describe
*this machine* and live in its local metadata, which a backup does not carry;
everything under `cluster` describes the *topology* and is a catalog record,
which a backup does. So a restore of last night's file onto a fresh machine gives
it the peer list and **not** the original's identity — and being able to see
which half a field is in is what stops that from being something you have to
remember.

Either clause of `DEFINE NODE` may stand alone, and one left out leaves its field
alone; what a clause names replaces what was there. There is no statement that
configures a *remote* node — you configure a node on it.

## Backing up

A backup of this store is its **log**, because the records, the indexes, the
catalog, the search statistics and the vector graph are all derived from it by a
pure function (ADR-0001). So a restore is a replay, through the same code a
replica runs.

```
bgv ./data --backup ./monday.bgvlog
bgv ./restored --restore ./monday.bgvlog

bgv --verify ./monday.bgvlog                       # changes nothing, needs no store
bgv ./data --backup ./tuesday.bgvlog --from 4001   # only what happened since
bgv ./restored --restore ./monday.bgvlog --upto 3000
```

`--verify` reads a backup and says what it holds, applying none of it and opening
no store — which is what makes it something a script can run on a schedule rather
than a thing somebody does once. Each record carries a checksum, so a file that
is the right *length* and holds the wrong *bytes* is caught; framing alone only
catches a file that was cut. That detects **corruption**, which is what happens
to files, and it does not claim to detect tampering, which needs a key.

`--from` writes an **incremental** backup, and the file says in its header what
it continues from — so restoring one onto a store that is not standing exactly
there is refused rather than silently producing a store no log explains. A base
plus its increments restores to a store that answers what the original answers,
which is the acceptance test rather than the description.

`--upto` stops a restore at a chosen sequence. There is nothing to rewind and
nothing to undo: the log *is* the store, so a replay that stops leaves the store
holding exactly what it held then.

**A store's `*.log` files are not logs to tidy away — they are its newest data.**
Removing the live one discards every write since the last flush, silently: the
store opens, answers, reports no failure, and is simply an earlier store. A
write-ahead file the engine has already recorded is refused when it goes missing,
but the live one cannot be, because once it is gone there is nothing left to
notice with. Copy the whole directory, or use `--backup`.

A restore refuses a store that is not where the file continues from — a whole
backup needs an empty store and an increment needs the store its base left
behind. Merging a backup into an unrelated store is not a restore, and the
sequences would land with a different meaning. A file that has been cut short restores what it holds and says
so on the error stream, because a backup interrupted at record nine thousand is
still nine thousand records and refusing it outright would throw away what
somebody is holding in a bad week.

A backup's header names the **build that wrote it**, alongside the framing and
record-codec versions already there. Those two say how to find a record and how
to decode one; neither says what the build that produced it *meant*. So an older
backup restores into a newer binary and the restore reports where the file came
from — that is the ordinary upgrade and the reason to record a version at all —
while a file from a **newer** build is refused, because guessing at what a newer
writer meant produces records nobody wrote, and it does it silently.

**What this makes testable is worth more than the feature.** If a restored store
differed from the original anywhere, something here would not be derived from the
log — so the acceptance test restores a store that has exercised every engine and
compares the two keyspace by keyspace, byte for byte.

**One key is deliberately excluded from that comparison, and it is the
interesting one.** A node's own identity is not derived from the log and does not
travel in a backup, so a restore onto a fresh machine produces a *different*
node. Without that, restoring last night's backup to check that it restores would
hand the copy the original's identity, and two processes would answer to one id
with nothing reporting it. It is asserted to differ in a test of its own, because
a hole in a comparison would also cover the key going missing entirely.

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
