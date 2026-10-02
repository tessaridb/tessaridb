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

## 0.21.0-beta — 2026-10-02

### Added

- **Clients are served over TLS 1.3, and a cluster serves them in the clear only
  when told to** (G054, ADR-0108 D4). `--tls-cert` and `--tls-key` (or
  `TESSARIDB_TLS_CERT` / `TESSARIDB_TLS_KEY`) put the wire port, HTTP and `/wire`
  behind TLS 1.3 with no mixed port. A node with peers refuses to start without
  them unless `--client-plaintext` says the network is trusted; a single node
  keeps serving in the clear and says so on every start. `--at` verifies a node
  with `--tls-authority`. A client limited to TLS 1.2 fails the handshake.
- **Certificates renew without a restart.** A node re-reads its client and peer
  certificate files every two seconds and presents a renewed pair from the next
  connection on; open connections finish on the one they started with. A pair
  that does not belong together — the certificate written and the key not yet —
  is refused, the previous certificate stays in use, and the node says so once.
- **`REVOKE CERTIFICATE '<sha256>'`** refuses a peer certificate on every node
  the row reaches, in both directions of the peer link, whatever the node is
  subscribed to. `INFO FOR NODE` lists the revoked fingerprints under
  `cluster.revoked`, and the administration trail records who revoked each one.
- **A node joining a cluster is approved** (ADR-0108 D9). A peer row binds a node
  only by `NODE`, by a pinned certificate (`DEFINE REPLICA … FINGERPRINT
  '<sha256>'`), or by a one-time token: `CREATE JOIN TOKEN FOR REPLICA r EXPIRES
  10m` answers the token once and the row keeps only its digest and expiry; the
  new node offers it with `--join-token` (or `TESSARIDB_JOIN_TOKEN`). Every
  binding is recorded in the administration trail.
- **A dropped node is never admitted again.** `DROP REPLICA` of a row that named a
  node records the removal on every node: the node's greeting is refused at the
  handshake whatever certificate it presents, and no row may name it again
  (`NodeTombstoned`). `INFO FOR NODE` lists removed nodes under
  `cluster.tombstoned`.
- **A store can be encrypted at rest** (ADR-0108 D7). `--encryption-key-file`
  (or `TESSARIDB_ENCRYPTION_KEY_FILE`) names a private 32-byte key: every file the
  storage engine writes is encrypted (XChaCha20, a random nonce per file), and
  every backup the node produces — `BACKUP`, `BACKUP … TO`, `GET /backup`,
  `--backup`, `--snapshot`, `--dump` — is sealed (ChaCha20-Poly1305, so a backup
  cut or altered does not open). A store opens only the way it was created, and
  the refusal names which key is missing or wrong; `--backup-key-file` restores a
  backup sealed under another key, which is how a store moves to a new one.
- **`/metrics` reports when each presented certificate expires**
  (`tessari_tls_certificate_expires_seconds`, by surface), to a caller who may
  read the node's topology.
- **Any node answers any request over the peer link** (ADR-0108 D1–D3). A request
  a caller cannot follow elsewhere is carried to the node that can answer it,
  under an assertion signed with the node's key; no password crosses the link.
- **One sign-in budget for the whole cluster** (ADR-0108 D5). A clustered node asks
  the store line's leader whether a name may try a password and reports how it
  went, so N nodes are not N allowances.
- **Changes to users, grants, replicas and the failover policy are recorded in the
  audit trail**, inside their own transaction.
- **Leaders send each commit to their followers as it lands**, on a held stream,
  instead of being collected every ten seconds; replication lag on three
  processes went from 10 s to 16 ms at the median.
- **A leader knows what each follower has made durable** and can hold a commit
  until enough voters hold its position (ADR-0106).

### Changed

- **A peer row with no `NODE` is no longer bound by the first node to greet.**
  Until this release a row left open was bound to whichever peer holding a
  certificate the cluster issued arrived first, and that peer received the row's
  whole reach. Such a row now waits for `NODE`, `FINGERPRINT` or a join token.
- **1492 conformance cases** define the language and run in the build.
- **A leader is replaced in about a second.** The lease is 800 ms with a 150 ms
  guard, renewed about every 300 ms, and every voter is canvassed in parallel; a
  killed leader was replaced in 859 ms at the median.
- **`DEFINE FAILOVER` drives the lease and the rounds** it always stored, and a
  leader's lease is the shortest hold any voter that elected it granted.
- **`/metrics` shows replication and leadership figures only to a caller who may
  ask `INFO FOR NODE`** (ADR-0108 D8).

### Fixed

- **A node leading a placed range no longer loses that range's records.** A node
  that replicated the whole store collected every log from the store line's
  leader, including the logs of ranges placed on other lines — or on itself — and
  when that follower could not continue the log the node copied its state over
  its own range, removing records only it held. The store line now carries only
  the logs it governs, and a node placed to lead a range is never copied over.
- **A voter judges a range's ballot on the line it holds**, not on its greeting,
  which describes only the range it is placed on — so a former leader or a
  follower holding the line refuses a candidate behind it.

- **A refused challenger no longer ends a live lease**: a voter holding a live
  grant adopts a ballot's epoch only past that grant (Q-880).
- **A leader its peers elected keeps its lease** when the deciding round did not
  carry its own vote.
- **A single-leader range keeps one history**, and a follower whose copy a deposed
  leadership finished is told so and re-seeds rather than reading itself level
  (Q-879).

## 0.20.1-beta — 2026-10-02

### Fixed

- **The command line no longer submits half a statement.** A line that closed
  one statement with `;` and began another — `CREATE notes:1 = { … }; SELECT body`
  with the rest on the next line — sent the unfinished second statement on its
  own, which was refused for what it lacked. A statement begun after a `;` now
  waits for its own `;`.
- **An unknown filter or stemmer language is refused at the word, with the
  choices named.** `FILTERS lowercase, stemmer(klingon)` now says *a stemmer
  language: english, russian, german, french or spanish* at `klingon`, and an
  unknown filter name lists `lowercase, ascii or stemmer`, where the refusal used
  to point past the word and name nothing.
- **Two search refusals lost a run of spaces in the middle of their message**
  (`SearchIsItsOwnOrder`, `NotSearched`). A test now holds every refusal message
  in the engine to having no run of spaces and no line break.

### Changed

- **Documentation: the `at` of `INFO FOR HISTORY` is a position in the
  database's log, not a number `VERSION` takes.** The two count different things
  and differ as soon as anything else is written; read a history's entries for
  what changed and use a version from `INFO FOR VERSIONS` to read the past.

## 0.20.0-beta — 2026-10-01

### Added

- **A write sent to the wrong leader is redirected** (G051, ADR-0101).
  `WriteIsElsewhere` now leaves the wire as the `Elsewhere` frame, `settled`, to a
  client that greeted protocol minor ≥ 1, and HTTP `POST /script` as `307` with a
  `Location`, where it used to be a refusal and a `409`. `SpansLeaderships` stays
  a refusal. A client of minor 0 still gets the refusal.
- **`DEFINE REPLICA … CLIENTS AT '<host:port>' HTTP AT '<url>'`** — where a client
  reaches each member, so a redirect names an address a client can speak to
  rather than the peer door. Optional; `INFO FOR NODE` reports both per peer.
- **A node holding part of a split table sends what it cannot gather to a node
  holding all of it** (G051 C4). `NotHeldHere` — a read inside a transaction,
  under `VERSION`, a join side, a `FETCH`, an `UPDATE` or `DELETE` — names a
  member whose `REPLICATES` covers the table's database and which this node has
  heard serving, and leaves the wire as the `Elsewhere` frame, `transient`, to a
  client of minor ≥ 1. With no such member, or over HTTP, it is the refusal it was.
  `ShardMapMoved` — a gathered read whose leader holds a different map of the
  table — names that member and leaves the wire the same way.
- **`INFO FOR DATABASE` names the database's searches** under `searches`, where the
  caller reads at least one of their tables — the console's database sheet shows them.
- **`session::context()`** — `{ node, namespace, database }`: the node this
  session is talking to and the tenancy it selected. Open to every session; it is
  what a client following a redirect checks on arrival and selects again there.
- **An ordered `LIMIT` over a split table ranks on the leaders** (G051 C5,
  ADR-0102). `ORDER BY … LIMIT n` whose keys read only the record — vector
  distances included — sends each lacking shard's first `n` rather than all of its
  records, so an exact nearest-neighbour read or a top-n over shards past 100 000
  records answers instead of `GatheredTooMuch`. Every node in a cluster must run
  this build: an older leader refuses the new gather section, and the read is
  `NotGathered` until it is upgraded.
- **A score over a split table is measured against the whole collection on a
  node holding part of it** (G051 C6, ADR-0103). The leader of each lacking shard
  counts its documents, tokens and the documents holding each asked word, and
  every record of the read is scored from its own text, so `search::score` and
  `ORDER BY FUSE` answer what a node holding every shard answers.
- **An `OR` whose every side an index serves is read through those indexes**
  (G051 T7.2). `title MATCHES 'ada' OR body MATCHES 'lovelace'`, or any mix of
  searched and valued sides, is the union of each side's candidates, tested again
  against the whole condition; `EXPLAIN` reports the shape `union` and every index
  read. A side with no index leaves the read a scan.
- **`stemmer(russian)`, `stemmer(german)`, `stemmer(french)` and
  `stemmer(spanish)`** (G051 T7.3) — the Snowball algorithms for those languages,
  each checked against its whole published vocabulary (134 869 words, every one
  stemmed as published). `stemmer` stays English and is stored as it was, so an
  existing analyzer reads back unchanged.
- **A starred word in a query string is a prefix, and it is scored** (G051 T7.4,
  ADR-0104). `body MATCHES 'vector sea*'` is the word `vector` and a word
  beginning with `sea`, with `OR`, `NOT` and the index working around it; a
  quoted phrase may end in one, `'"ada lov"*'`. `search::score` weighs a starred
  word as one term over the sixty-four most-held words it begins, sharing the
  largest of their document frequencies — so type-ahead can be ranked by the
  store. A score holding a starred word is refused with `NotHeldHere` on a node
  holding part of a split table.
- **A judgment set and a relevance harness** (G051 T9.0, ADR-0100 D3).
  `benchmarks/judgments/docs.tsv` grades 81 queries over the documentation site's
  pages; `cargo run --release -p tessari-bench --example relevance` reports
  NDCG@10, MRR@10 and cold and warm latency. Type-ahead queries moved from
  NDCG@10 0.219 to 0.802 with the starred word, the whole set from 0.603 to 0.695.
- **`search::explain(field, query)`** (G051 T7.5, ADR-0100 D1.8) — the score
  `search::score` answers, with the collection's numbers and one entry per asked
  word and per starred word: how often the record holds it, how many records do,
  its weight and its contribution. The contributions add up to the score exactly.
- **A ranked page after the first is read by the pruned walk** (G051 T7.5,
  ADR-0100 D1.9). `ORDER BY search::score(…) DESC AFTER <record> LIMIT n` resumes
  below the anchor's score instead of scoring the whole table, and no longer
  carries the `cursor-walked` note.
- **`DEFINE INDEX … SEARCH [POSITIONS] [OFFSETS] [NO SCORE]`** (G051 T7.6,
  ADR-0100 D4) — what a search index keeps beside its postings. `POSITIONS`
  decides a phrase from stored token ordinals (`EXPLAIN` shape `phrase`),
  `OFFSETS` marks a whole-word highlight from stored byte ranges, `NO SCORE`
  keeps membership alone and no statistics, and a score over it is refused as over
  no index. No option changes an answer; an index written before them reads as a
  scored index with neither.

- **`DEFINE SEARCH`: several fields of several tables ranked as one collection**
  (G051 C9, ADR-0105). `SELECT … FROM SEARCH <name> MATCHES [PREFIX | FUZZY |
  INFIX] '<query>' [WHERE …]` ranks every member table's records by BM25F, with a
  `WEIGHT` per field, `NO FUZZY` / `NO PREFIX` / `NO PHRASE` per field, and
  `SYNONYMS <set>` per field. `search::score()`, `search::table_name()`,
  `search::snippet()` (the best 24-token window of the `SNIPPET` fields, as byte
  offsets) and `search::highlight(field)` answer about each record; `COMPLETE
  '<beginning>'` answers ranked type-ahead; `GROUP BY` over the source answers
  facet counts. A table the reader may not read, or whose member fields they may
  not all read, is not searched. `INFO FOR SEARCH`, `DROP SEARCH`, and the state
  script carry it.
- **`DEFINE SYNONYMS <name> { word: ['alternative'] }` and `DEFINE STOPWORDS
  <name> ['word']`** — query-time word sets, store-wide names a search reads when
  it runs, so changing one never rebuilds an index.
- **`MATCHES INFIX '<piece>'`** — a field's text holds a term containing every
  piece typed. A `SEARCH` index now keeps every suffix of its dictionary's terms
  (key kind `0x1f`) and serves it as `infix-terms`; an index built by an earlier
  release has no suffixes and is answered by the scan until `REBUILD INDEX`.

### Fixed

- **A write a follower forwards reaches the leader in a cluster run with peer
  credentials.** The forward dialled the leader's `AT` address, which is the peer
  door and speaks TLS, so every such write was refused with *that is not a
  TessariDB node*; it now dials `CLIENTS AT` when the row declares one.
- **A `SEARCH`, `SPATIAL` or `VECTOR` index over several fields indexed the
  first alone** (G051 T7.2). `DEFINE INDEX … FIELDS title, body SEARCH` was
  accepted, and a search of `body` then answered as though no index existed. It is
  now refused with `IndexReadsOneField`; declare one index per field. A store that
  already holds such an index keeps it as an index over its first field.
- **`search::score` on a node holding part of a split table was silently
  wrong** (G051 C6). A gathered record scored `0` and a held one was measured
  against this node's shards alone, so the order differed from the whole table's
  with no refusal and no note. The "did you mean" suggestion there came from the
  node's own dictionary and could say nothing was nearer when a shard it lacked
  held the word; it is now withheld on such a node.
- **A `DELETE` on a node holding part of a split table no longer removes only
  that part.** A conditional or span `DELETE` read just the shards this node
  holds, removed what matched there and reported that count as the statement's;
  `DELETE` of one record held elsewhere answered as though it had removed it, and
  `UPDATE` of one answered `NoSuchRecord`. Each is now refused `NotHeldHere`, as
  the docs already said.

- **A redirect is no longer sent after part of the script committed.** A client
  follows a redirect by sending the script again, and a script is not a
  transaction: `CREATE …; SELECT … STALENESS 1s` had committed its `CREATE`
  before the read was redirected, and following it would have written twice. A
  read or a write redirect is now sent only when nothing in the script committed;
  otherwise the refusal (wire) or `409` (HTTP) answers. Present for reads since
  the frame existed.

### Security

- **A field grant now hides a searched field from the score and the suggestion as
  well as from the match** (G051, Q-861). A caller granted `read ON t FIELDS a`
  already got no records from `b MATCHES …`, but `search::score(b, …)` still ranked
  records by `b`'s text and a misspelt `b MATCHES …` still answered `did you mean`
  with a word `b` holds — both are read from the index by identity or by term and
  never touched the record the grant redacted. A hidden field now scores `0` for
  every record, as a field the record does not hold, and earns no suggestion at all.
  Every release up to and including `0.19.0-beta` has this leak: text that a field
  grant hides on those builds should be treated as having been probe-able by a
  caller who could search the table.

### Changed

- **1488 conformance cases** define the language and run in the build.
- **`MATCHES FUZZY` counts swapping two adjacent letters as one edit** (G051
  T7.4, ADR-0100 D1.4), where it counted two, so it reaches a few more words —
  `vetcor` is now one edit from `vector`.
- **A query string holding `word*` now means words beginning with it.** The
  asterisk used to be dropped by the tokenizer, leaving the word itself, so such
  a query answers more records than it did.


## 0.19.0-beta — 2026-10-01

### Added

- **A split table's map changes while it serves** (G050, ADR-0095). `ALTER TABLE
  t SPLIT AT 'm'` retires the shard holding the point and mints two in its place;
  `ALTER TABLE t MERGE SHARD 4, 5` retires two neighbours and mints one. No record
  moves: a retired shard's log keeps what was written to it and is still
  collected, fed and backed up, and every write after the change is filed by the
  new map. `INFO FOR TABLE` reports the map's `version` and its `retired` shards.
  Refused by name: `NotASplitTable`, `SplitPointOnABoundary`, `ShardNotLive`,
  `ShardsNotAdjacent`, and `ShardNamedByAReplica` for a shard a `REPLICATES
  SHARD` or `LEADS SHARD` row names. A gathered read asking a node whose map
  differs is refused `ShardMapMoved` (retriable, HTTP 409).
- **A gathered read is worked out on the shards' leaders** (ADR-0097). A `WHERE`
  and an unordered `LIMIT` travel to each leader, and `count`, `sum`, `mean`,
  `min` and `max` — with `GROUP BY` — are folded there and merged here, under the
  caller's visibility, so a grouping read over more records than a gather holds
  now answers. A fold the leader cannot do exactly (floats in `sum` or `mean`, a
  comparison across kinds, an evaluation error) gathers the records as before.
- **A table partitioned by region** (ADR-0096): `DEFINE TABLE t (…) IDENTITY uuid
  PARTITION BY region` names each record `'<region>:<uuid v7>'`, so `SPLIT AT`
  over region names gives each region a shard, placed on its node with `LEADS
  SHARD`. A `WHERE region = 'de'` reads that region's records only, and
  `EXPLAIN` names the one shard. Refused: `PartitionNeedsGeneratedUuid`,
  `PartitionMismatch` (also for an `UPDATE` changing the region).
- **A placement moves**: `ALTER REPLICA b LEADS SHARD …` / `… LEADS NONE` (ADR-0098).
  The node a range is taken from stops standing for it, and a voter that has applied
  the move refuses it a ballot there, so its lease runs out and the new candidate is
  elected; writes into the range continue except while that election runs. A row that
  is not a range's last candidate can be moved or dropped; the last one is still
  refused `PlacementCannotBeDropped`.

### Fixed

- **A range placed on a node that follows the store's leader never took a write**:
  the store leader's leadership row was taken to cover the placed range, so the
  range's own leader could neither record its win nor write. A leadership row now
  governs only the ranges on its own line.
- **A node subscribed to one shard could lead the whole store** and then answer a
  read of a split table from its one shard as if it were the whole. A node whose
  own row replicates less than `STORE` now stands only for the range it is placed
  on.

### Changed

- **A moved shard map is stored in a new shape.** A map no statement has moved
  keeps the bytes it always had; a moved one is stored with its version and its
  retired shards, which a build before this one refuses to read.
- **1465 conformance cases** define the language and run in the build.

## 0.18.1-beta — 2026-10-01

### Fixed

- **A log backup of a store written before each database had its own log
  restores again.** Such a store keeps those records in its store log, and a
  restore filed each record by what it touched rather than by the log the backup
  read it from, so `--restore` into an empty store stopped at once with `log gap:
  the next record must be 1, but N was offered` — while `--verify` passed the same
  file. A restore now files every record in the log its section names.

## 0.18.0-beta — 2026-10-01

### Changed

- **A backup is a snapshot unless the log is asked for** (G049, ADR-0094). `BACKUP`,
  `BACKUP … TO`, `GET /backup`, `--backup <file>` and the console now write a state
  snapshot. The log is `BACKUP LOG`, `GET /backup?as=log`, or any form with a
  position (`BACKUP FROM n`, `?from=n`, `--backup <file> --from n`; `--from 1` is
  the whole log). A client's `backup()` with no position now receives a snapshot.
- **A serving node bounds its log by default**: the newest 100 000 records of each
  log are kept and the rest pruned. `TESSARIDB_RETAIN_RECORDS` sets another count
  or `none`; `DEFINE NODE RETAIN` stored on the node wins over both, and `RETAIN
  NONE` is now remembered as a choice. `INFO FOR NODE` reports `retain_source`
  (`statement`, `environment`, `default`). A store upgraded from an earlier version
  starts pruning at its first housekeeping pass — take a snapshot first if its
  history matters, or start it with `TESSARIDB_RETAIN_RECORDS=none`.
- **The refusal for a read below a pruned log names its repair**: a follower node
  copies its leader's state by itself; a client re-reads what it follows and follows
  again from the current tail.

### Added

- **A follower below its leader's pruned log re-seeds itself** (ADR-0094 D3). A node
  that leads nothing copies its leader's state over one peer connection, then follows
  again from where the copy stood; a node that leads a range reports `stranded` and is
  restored from a snapshot. `INFO FOR NODE` answers `cluster.upstream` (`state`,
  `copied_records`, `copies`); `/metrics` adds `tessari_replica_state{state}`,
  `tessari_replica_copied_records` and `tessari_follower_behind_records{node}`; the
  console's cluster map shows the sync state and how far the furthest follower is
  behind.
- **A snapshot of one place**: `BACKUP STATE OF NAMESPACE n` or `OF n.d` (also bare
  `BACKUP OF …`) carries that place's records and the catalog that defines it, with
  the store's users, and restores into an empty store. Two places are two snapshots.
- **A snapshot is never held in memory** (ADR-0094 D6): `GET /backup` writes it to an
  unlinked file in the node's temporary folder and sends it from there with its exact
  `Content-Length` (the protocol forbids chunked framing), and `BACKUP … TO` writes it
  straight into the file. Measured on a debug build, the node's peak memory during
  `GET /backup` was 25.9 MB for a 21.8 MB snapshot and 28.5 MB for an 87.0 MB one
  (idle 20.4 and 21.3 MB), against 160 MB and 553 MB when the same snapshot is answered
  whole over `POST /script` — use `GET /backup` or `TO` for a large store, and leave the
  node's temporary folder room for one snapshot.
- **A snapshot on a cluster** is taken on any node holding the whole place, a
  follower included; a node holding part of it — some shards of a split table —
  refuses with `NotHeldHere`, naming the shards it lacks.
- **1463 conformance cases** define the language and run in the build.

## 0.17.1-beta — 2026-10-01

- **The console's Vault tab reads and writes records** (G048). A **Records** pane
  lists a database's vaults, a vault's record ids a page at a time, reveals one
  record on click (recorded by the store before it answers), writes one field and
  shows the audit trail. It sends statements over `POST /script` with the record
  id and the written value as bound parameters, so neither is in the statement
  log; a revealed value is kept only on the page and removed by Hide.
- `INFO FOR DATABASE` answers `vaults` beside `tables` and `topics`.

## 0.17.0-beta — 2026-10-01

- **Backups written on the node, from the console.** `BACKUP [STATE | SCRIPT] TO
  '<name>'` writes the backup into the node's backup folder — `--backup-dir`, or
  `TESSARIDB_BACKUP_DIR`, which the image sets to `/var/lib/tessaridb/backups`
  inside its volume — and answers `{ path, bytes, form }`. A name that would leave
  the folder (`..`, absolute, a link on the way) is `BackupNameRefused`, an existing
  file is `BackupExists`, a node with no folder is `NoBackupFolder`; the file is
  written aside, verified where it was written, and only then renamed into place.
  The console's new **Backup** tab (⌘6) runs it.
- **A backup of chosen namespaces and databases.** `BACKUP SCRIPT OF NAMESPACE crm,
  prod.orders` writes those places, their records and indexes, and the analyzers
  their fields use (`IF NOT EXISTS`), and no users; its header says it is a part. A
  log or a snapshot of a part is refused with the reason.
- **`RESTORE SCRIPT FROM '<name>'`** runs a script from the backup folder into a
  live store beside what it holds. It only creates: a database that exists is
  `RestoreTargetExists`, and a script that deletes, drops, declares a user or writes
  into a place it did not create is `RestoreRefused` — both before anything is
  written, and a restore refused while it fills takes away what it created. The
  Backup tab restores from the same folder.
- **A vault you can reach without writing its passphrase into a script**
  (G048, ADR-0092). `GET /vault`, `POST /vault/unseal` (the body is the
  passphrase), `POST /vault/seal` and `POST /vault/passphrase`, and a wire frame
  of its own (tag 17, **protocol 1.2**), carry the passphrase as a field and never
  as statement text; no refusal and no log line quotes it. Wrong passphrases are
  throttled like wrong passwords (`PassphraseThrottled`).
- **An unseal lasts ten minutes**, then the store seals itself; `--unseal-for` or
  `TESSARIDB_UNSEAL_FOR` (`1h`, `90s`) sets the period per node. `INFO FOR SEAL`
  answers `{ state, seals_at, unseal_for }`, `state` being `uninitialised`,
  `sealed` or `unsealed`.
- **`CHANGE VAULT PASSPHRASE FROM '…' TO '…'`** wraps the same master key under a
  new passphrase — no secret is re-encrypted, the old passphrase stops unsealing,
  and a backup taken before the change still opens with the old one. A store never
  unsealed is `NoVaultRoot`.
- **A vault may carry its own passphrase** (ADR-0093). `DEFINE VAULT team
  PASSPHRASE '…'` wraps the vault's key under a key derived from that passphrase
  instead of the store's master key, so neither the store's passphrase nor
  store-wide authority opens it, and it can be declared on a sealed store.
  `UNSEAL VAULT team WITH '…'`, `SEAL VAULT team`, `CHANGE VAULT team PASSPHRASE
  FROM '…' TO '…'` and `INFO FOR SEAL OF team` act on that vault alone, with its
  own ten-minute period and its own throttle; the same four exist as
  `/vault/{ns}/{db}/{vault}[/unseal|/seal|/passphrase]` and as a target in the
  wire frame. Naming a vault that opens with the store's passphrase is
  `VaultUsesStorePassphrase`. `INFO FOR VAULT` now says which custody a vault has.
- **The console's Vault tab** (⌘7) unseals, seals and rekeys the store's key and any
  one vault over the vault routes, so a passphrase typed there is never statement
  text, never in the statement log and never kept in the page.
- **`INFO FOR VAULT team RECORDS [AFTER team:'x'] [LIMIT n]`** lists a vault's
  record ids a page at a time (a thousand by default, ten thousand at most), with
  no value in the answer.
- The console answers `HEAD /` and every console asset as `GET` without the body,
  with `Cache-Control: no-cache` and a strong `ETag`; a matching `If-None-Match` is
  a `304`, so a browser never runs an old console against an upgraded node.
- Fixed: `INFO FOR RECIPIENTS OF team:$id` and `INFO FOR VAULT team RECORDS AFTER
  team:$after` refused their parameter as unbound, so a client had to write the id
  into the statement text. Both now bind it.
- **1462 conformance cases** define the language and run in the build.

## 0.16.0-beta — 2026-09-30

- **A backup that survives a pruned log** (G047, ADR-0091). Beside the log two more
  forms, both the store's current state: `BACKUP STATE` / `--snapshot` / `GET
  /backup?as=state` writes every live record at one version (`.tessarisnap`), and
  `BACKUP SCRIPT` / `--dump` / `GET /backup?as=script` writes TessariQL that
  rebuilds the store (`.tessariql`), naming in its header whatever it cannot
  carry. `--restore` and `--verify` read a snapshot by its opening bytes; a
  restored snapshot stands where it was taken, so the log after it applies on top.
  Measured: one record written 10 000 times is 1 641 008 bytes of log and 1 100
  bytes of snapshot.
- `DEFINE USER … PASSHASH '<argon2id hash>'` declares a user from a stored hash,
  refused below the store's own hashing parameters.
- **Fixed: a log backup of a pruned store** was refused but left its file behind,
  and `--restore` read that file as an empty store with exit 0. Every backup is
  now written aside and moved into place only when whole, and the refusal names
  `--snapshot`.
- **Fixed: vectors written in one transaction were not linked to each other.**
  The graph was read from committed state per record, so several vectors in one
  commit came out with no edge between them and `APPROXIMATE` reads under-answered.
  Run `REBUILD INDEX` on a vector index filled by multi-record transactions before
  this release.
- **Fixed: `INFO FOR TABLE` on an edge table** answered a definition that refused
  when run again, because it wrote the endpoint fields and indexes the `EDGE` word
  makes for itself.
- The docs said a pruned log's backup begins at its horizon; it is refused. They
  now say so, and point at the snapshot.

## 0.15.0-beta — 2026-09-30

**A browser speaks the wire protocol.** `GET /wire` on the HTTP port upgrades to
a WebSocket that carries the wire protocol byte for byte — the greeting, the
frames and all seventeen value types — so a page gets the store's own values
rather than JSON's six. The socket is a session of the wire node itself: its
connection ceiling counts TCP and WebSocket sessions together, and its drain,
bridge and redirects are the same. Credentials travel in the request frame as
over TCP; an `Authorization` header or a cookie on the upgrade is ignored, because
a browser attaches both to a socket any page opens. A node started without a wire
address answers `/wire` with `404`; a full node answers `503` before upgrading;
a text message closes the socket with `1003`. Protocol specification §3.13.

**A space over HTTP.** `/kv/{ns}/{db}/{space}/{op}/{key…}` reads and writes a
space as a cache, a counter and a lock: `GET`/`PUT`/`DELETE` a key (with an
expiry and an `if=absent|present` condition), `swap`, `incr`, `expire`,
`persist`, `lock` and `unlock`, and `GET /kv/{ns}/{db}/{space}?prefix=…` lists
keys. Each is one space statement through the caller's session; a condition that
does not hold answers `false` rather than an error; `unlock` is an expiring
conditional write, never a delete. A table that is not a space answers `404`.
Protocol specification §5.10.

**The console lists a space.** A Spaces pane on Run finds the spaces of a
database, lists keys by prefix and shows one key's value and how long it has
left, through the `/kv` routes.

Nothing in this release changes the storage format.

## 0.14.0-beta — 2026-09-30

**A series can be ordered by when its events happened.** `DEFINE SERIES
readings RETAIN 30d TIME at` names each record from its own `at`, so a late or
backfilled reading lands in its place and the retention is about the event. A
windowed grouping can be filled over a stated range (`FILL NULL | PREVIOUS |
LINEAR | <value> FROM … TO …`); `LATEST BY <field>` answers the newest record per
key, one index seek per key; `ASOF JOIN` pairs each record with the newest one of
another series at or before it; `increase`, `rate` and `delta` fold counters by
the instant, a fall counting as a reset; and `DEFINE ROLLUP` keeps per-window
`count`, `sum`, `min` and `max` in the transaction that writes the series.

**Aged records go as one range.** The node's housekeeping removes what a series'
floor has hidden with one range removal per series, index entries first, below
the oldest open reader's floor. It is each node's own storage work: nothing is
written to the log or the feed. A million aged points take milliseconds; the
replaced per-record pass stopped after its first 512.

**A series takes about a fifth of the space.** The log is compressed densely
from its first flush (its files reach the bottom level without a rewrite, so an
uncompressed flush stayed uncompressed), the bottom level uses a trained
dictionary, and event identities carry fewer random bits. A million regular
readings take 28 bytes per point on disk, where `0.13.1-beta` took 148. Stores
written by `0.13.1-beta` open unchanged.

**`POST /series/{ns}/{db}/{series}`** appends a batch of events in one
transaction and answers how many landed; every client (`0.4.0`) has `append`.
The console lists a database's series and rollups on Run.

**1444 conformance cases** define the language and run in the build.

## 0.13.1-beta — 2026-09-29

**A new Kafka consumer takes the messages already on its topic.** A group the
broker has never seen used to start at the end of the topic — the client's
default — so everything published before `DEFINE KAFKA CONSUMER` was silently
never ingested. It now starts at the oldest message still on the topic. A group
that has already committed an offset resumes from it, exactly as before, so a
running consumer is not replayed by the upgrade. A topic consumer already began
at the oldest message; a test now holds both to it.

No change to the language, the wire format or the on-disk format.

## 0.13.0-beta — 2026-09-29

**A topic can be read into a table by a declaration.** `DEFINE TOPIC CONSUMER
events_in FROM events GROUP 'into-rows' INTO event_rows IDENTITY event_id MAP
amount AS total ON FAILURE quarantine` makes the node read the topic as a member
of that group and write each message as a record — and because the read, the
writes and the acknowledgement commit in one transaction, each message is applied
to the store **exactly once**. `ON FAILURE stop` halts at a message that cannot
be applied; `quarantine` hands it back so the group's dead letter keeps it. It
runs in every build, on the node's runtime, starts and stops within a second of
`DEFINE` or `DROP TOPIC CONSUMER`, and writes with its declarer's authority.
`INFO FOR TOPIC CONSUMER` describes one; `INFO FOR TOPIC` lists them under
`ingested_by` (ADR-0087).

**The console shows which topic consumers read a topic.** The Topics screen
lists, for the chosen topic, each topic consumer reading it — its group, the
table it writes and whether it runs on the node that answered.

**Breaking, for programs that embed the engine.** A stored consumer's
`ConsumerDefinition` carries its source as `feed: Feed` (`Kafka { brokers,
topic, format }` or `Topic { table }`) instead of the three fields. Nothing
changes for a statement or for a store: a Kafka consumer is written exactly as
before and every existing one reads back unchanged. `Session::atomically` is
new — several scripts in one transaction its caller commits.

**1438 conformance cases** define the language and run in the build.

## 0.12.0-beta — 2026-09-29

**A topic can be shared by a group of workers who acknowledge each message.**
`DEFINE GROUP 'billing' ON TOPIC events ACK DEADLINE 30s` makes the readers
under that name a group: a read hands messages out and holds each in flight
until `ACK` says it is done; `NACK` hands it out again now or after a delay; a
deadline that passes hands it out again on its own; and past `DELIVERIES n` it
is given up on and appended to the group's `DEAD LETTER TO` topic. `IN FLIGHT n`
(default 1, which keeps the order) says how many a group may hold at once, and
`ALTER GROUP … START AT n` moves it. `INFO FOR TOPIC` reports each group's
position, committed position, lag, messages in flight, redeliveries and dead
letters. A reader under a name with no group keeps exactly what it had: its
position moves with its own transaction (ADR-0086).

**`/metrics` reports topics to a scraper that signs in.** Given a credential,
the scrape adds, for every topic that caller may read, how many messages it
holds, its last position, each reader's lag, and each group's lag, messages in
flight, redeliveries and dead letters — read through the caller's own session,
so grants decide which topics appear. Without a credential the scrape is what it
was: no topic or group is named, because a name is schema. A credential that is
refused is answered `401`. `INFO FOR DATABASE` now lists its `topics` beside its
`tables`.

**The console has a Topics tab.** It lists the topics of a chosen namespace
and database with how many messages each holds, its retention, its readers and
groups and how far behind the furthest one is; shows a topic's readers and
groups in full; pages through its messages without moving anybody's position;
and creates and removes topics and groups and moves a group to another
position. The three that lose something ask for the name to be typed again and
say beforehand what they will cost. Topics is the second destination, so the
keys are now ⌘1 Run, ⌘2 Topics, ⌘3 Cluster, ⌘4 Access and ⌘5 This node.

**1433 conformance cases** define the language and run in the build.

## 0.11.0-beta — 2026-09-29

**One runtime serves every surface.** The node used to give every held
connection its own operating-system thread; the wire protocol, HTTP with its
watch socket, the peer door, the cluster rounds and the housekeeping now run as
tasks on one multi-threaded runtime, and store calls cross to it through a
bounded bridge. What a client can do and what it is answered are unchanged —
every serving surface was checked against 0.10 on a running node — and what
changed is what a node can hold.

### Serving

- **Held connections are bounded apart from work in flight.** Each surface now
  holds up to 16 384 connections (it was 400, one thread each), of which at most
  400 are inside the store at once. A statement that finds every one of those
  places taken is refused as busy, and its connection stays open, instead of the
  connection being turned away at the door.
- **An idle subscriber costs a task, not a thread.** Measured on one node:
  10 000 subscribers held on 14 threads, a new client accepted in under 3 ms and
  answered in under 0.2 ms while they are held. Before, 400 subscribers filled the
  node on 409 threads and every other client — query or feed — was refused.
- **A feed wakes when something lands**, whichever surface, peer or background
  round wrote it, rather than checking the store on a timer.
- **Throughput is unchanged.** Request traffic at 1, 8, 64 and 400 connections
  answers as fast as 0.10 did; a client sending one statement at a time on one
  connection is served on one thread while it stays busy, as the old node served
  every connection.
- **The peer door serves each peer on its own task**, up to 64 at once — one
  peer that connected and said nothing used to hold the door for everyone.
- **Stopping drains every surface at once** rather than one after another, so
  the worst case for three surfaces with stuck work is 20 s rather than 60.
- **A listener that fails ends its surface** with a logged reason instead of
  retrying the same failing accept in a loop.
- **A large backup declares its length.** `GET /backup` answered a store of
  more than about 38 kB chunked, which the protocol forbids on every route; it
  now sends `Content-Length` at every size. The body is the same bytes.

### Performance

- **Vector distances are about two-thirds faster.** Ordering numbers that are
  far apart no longer converts them to decimals, and a distance borrows its
  operands instead of copying them: an exact nearest-neighbour scan went from
  3.0 to 1.8 ms at p50 (330 → 556 reads a second on the bench's 2 000 vectors).
  Every other in-process workload measured the same as 0.10.

### Compatibility

- The on-disk, backup and wire formats are unchanged: a store and a backup
  written by 0.10 open under this version, and every 0.10 client works against
  it.
- Building from source needs **Rust 1.98**.
- For code that embeds the `tessaridb` library: its public functions now return
  typed errors rather than strings, so a caller matching on message text needs
  to match on the error instead.

**1420 conformance cases** define the language and run in the build, unchanged
from 0.10.0-beta.

## 0.10.0-beta — 2026-09-28

**Faster where it was measured to be slow.** Every change below answers a cost a
profile named, was measured before and after on the same data at the same
durability, and returns exactly what the path it replaced returned. The language
and the wire format are unchanged; see the last section for the one thing a store
written by 0.9 may now refuse.

### Performance

- **Concurrent commits share one sync.** With `power-loss-safe` durability every
  commit used to pay its own device flush while holding the store's single write
  turn, so sixteen writers committed no faster than one. A commit now checks and
  builds its batch under the turn, queues it and lets the next writer in; the
  first waiting commit lands the whole queue in one write and one sync. Measured
  on disk with sixteen writers: about 185 → 1 500 commits a second, per-commit
  p99 255 → 18 ms; one writer is unchanged. Nothing in a group is readable before
  it is synced, and every commit is still checked against the store as the
  commits ahead of it leave it.
- **Writers inside one process queue instead of racing.** Commits that touched
  different records were refused after eight lost races for the store-wide
  version; they now wait their turn.
- **The catalog is read once, not per statement.** Namespace, database and table
  rows are held between statements for every reader at or above the last change
  to them, and table definitions are decoded once per stored form: a read by id
  went from 5.0 to 3.3 µs at p50, a create from 16.7 to 12.7 µs.
- **An update leaves alone every index whose fields it did not change**, the
  full-text analyzer included.
- **Many readers no longer queue on one lock.** The registry of live snapshots is
  split per thread: sixteen readers on disk went from about 286 000 to 415 000
  reads a second. Two to four disk readers are 10–15 % slower, because their
  waits moved into the storage engine's own per-iterator lock; that trade is kept
  deliberately and is recorded.
- **Vector distances are computed in one pass** over both vectors with nothing
  allocated: an exact nearest-neighbour scan is about 16 % faster, with answers
  identical to the bit.

### Correctness and safety

- **On macOS a synced commit now reaches the medium.** The storage engine was
  built without full-flush support, so a `power-loss-safe` commit on a Mac could
  sit in the drive's cache. It is now built with it, and the build refuses a Mac
  target without it. A synced commit on a Mac therefore costs milliseconds, as it
  should; Linux is unaffected.
- **A leader's write fence is judged again once a commit holds its turn**, so a
  commit admitted with little lease left can no longer land after the fence shut.
- **Values nest at most 64 containers and statements at most 64 expression
  levels.** One frame of deeply nested arrays could end the node before signing
  in; it is now refused by name, on decode and on write alike.
- **An HTTP request body larger than 16 MiB is refused with `413`**, before it is
  read when its length is declared — bodies were read whole before any
  credential was checked.
- **A panic ends the work that met it, not the node**: one connection, one
  request or one background round. Background work restarts after a second, a
  consumer whose thread panicked is no longer reported as running, and a panic
  while landing a group of commits answers that group "outcome unknown" instead
  of stopping every later commit.

### Compatibility

- The on-disk and wire formats are unchanged, and a store written by 0.9 opens
  under this version. The one exception is a stored value nested deeper than 64
  containers, which 0.9 accepted and this version refuses to read; a store
  holding one must be re-ingested with the value flattened.

**1420 conformance cases** define the language and run in the build, unchanged
from 0.9.1-beta.

## 0.9.1-beta — 2026-09-26

**The README says what the store is for now.** No engine change: the language, the
wire format and the on-disk format are exactly those of `0.9.0-beta`, and a store
written by either opens under the other.

- The README opens on the stack an AI application would otherwise run — a
  database, a search engine, a vector index, a cache and locks, queues, an event
  log, places, relations, files and sealed secrets — and what answers each here,
  with the trade stated beside it: not the fastest at any one of them, enough at
  all of them, and able to grow onto more machines as the data does.
- It lists the five clients, names the published image `0.9.0-beta`, carries the
  licence's current change date and counts the test targets as they are.

**1420 conformance cases** define the language and run in the build, unchanged
from 0.9.0-beta.

## 0.9.0-beta — 2026-09-26

**The geo gaps a places application meets.** Distance to shapes, radius reads
served by the spatial index, a name and a place in one statement, and counting by
cell.

- **`geo::distance` measures to shapes.** Between a position and a path, an area
  or several of them it is the distance to the shape's **nearest point** — zero
  when the shape covers the position, by the same exact rule `geo::covers` uses,
  and otherwise found by a search that never drops a piece of an edge that could
  hold a nearer point. Edges are the lon–lat straight lines the geometry says.
  Two shapes both larger than a position are still refused by name. A
  nearest-first read over a table holding paths or areas is answered by the scan.
- **A radius read is served by the spatial index.**
  `WHERE geo::distance(at, $here) < r` (and `<=`, either way round, either
  argument) reads the records whose boxes meet the query's box widened by `r` —
  widened far enough that every position within `r` is inside, including at the
  poles and across ±180 — and tests the distance on those. `EXPLAIN` says
  `region`. A lower bound (`> r`) stays a scan: the outside of a disc has no box.
- **A name and a place in one statement**: `MATCHES` with a radius, or
  `ORDER BY FUSE (search::score(…) DESC, geo::distance(…))` to rank by both.
- **`geo::cell(position, level)`** answers the spatial index's own cell at that
  level (0 the world, 32 the finest) as a polygon, so `GROUP BY geo::cell(at, n)`
  counts a zoomed-out map's points per cell and the key draws itself. Cells are
  not equal in area; a density is `count(*) / geo::area(geo::cell(at, n))`.

**1420 conformance cases** define the language and run in the build, up from
1413.

## 0.8.0-beta — 2026-09-26

**Several orders in one read, fused by rank.** `ORDER BY FUSE (…)` ranks the
records of a read by two or more orders at once — a text score, a vector
distance, a geographic distance, or any other key — and combines them by where
each record came in each order, never by adding the values: a relevance score
and a distance are not on one scale.

```
SELECT title FROM notes
 ORDER BY FUSE (search::score(body, 'lock') DESC, vector::cosine(embedding, $q) WEIGHT 2)
 DEPTH 50 LIMIT 10;
```

- A record earns `weight / (60 + place)` from each branch that places it within
  the first `DEPTH` (default 100); the fused order is the sum, ties by identity.
  A record no branch placed is not in the answer.
- The `WHERE` applies to every branch. A fused read is exact: every branch ranks
  every record that passed the filter, and `APPROXIMATE` does not change that.
- `search::ranks()` in a fused read's projection answers where each branch placed
  the record (`none` outside a branch's depth), and is refused with **`NotFused`**
  anywhere else. A fused read projects after it orders, so its branches read the
  stored record rather than a projected alias.
- One branch, a weight of zero, `DEPTH 0`, a key beside `FUSE (…)`, `GROUP BY` and
  an `AFTER` cursor are refused by name.

**1413 conformance cases** define the language and run in the build, up from
1405.

## 0.7.0-beta — 2026-09-26

**A topic: an append-only order of messages whose readers keep their positions
in the store.** For the job a message broker does in a project that would rather
not run one — slower than a broker, with one guarantee a broker beside a
database cannot give.

- `DEFINE TOPIC events [RETAIN d] [MAX BYTES n] [PUBLIC RATE n PER d]`.
  Appending is `CREATE` and `INSERT`; each message gets a **dense position**
  (1, 2, 3 … no gaps) decided when it commits, so a reader never sees 8 before 7
  exists. A message is never changed: `UPDATE`, `UPSERT` and `DELETE` of one are
  refused naming the topic. Naming the identity makes an append idempotent.
- `READ FROM events [FOR CONSUMER 'name'] [AFTER n] [LIMIT n]`. A named reader's
  position is stored **and moves in the reader's own transaction**, so an effect
  it writes into this store commits with the position — exactly once per
  message. Readers under one name take turns and between them are given every
  message once.
- `RETAIN 7d` removes old messages through the log; a reader passed over is told
  by a `lapsed` note, never skipped silently. `INFO FOR TOPIC` reports each
  reader's position and lag.
- `PUBLIC RATE n PER d` (with `MAX BYTES`) lets a caller that has not signed in
  **append and do nothing else** on a closed store: generated identities only,
  nothing that reads, counted per node in memory. An append past it is refused
  with **`TopicRateExceeded`**.

**A change feed follows a split table.** A feed over a split table — or over a
database holding one — reads the database's log and its shards' logs, delivers
their changes in the order the node committed them, and gives every change a
`cursor` to resume from with nothing missed or repeated. The wire and WebSocket
feeds carry it as an optional trailing field, so a feed over an unsplit table is
unchanged. A feed over a split table on a node that does not write all of it is
refused by name.

**1405 conformance cases** define the language and run in the build, up from
1390.

**On disk:** a topic's catalog entry carries its declaration, and three new key
kinds hold its positions (offset, entry and head); readers' positions live in a
new system table. A store written by this build is not promised to open under an
earlier one.

## 0.6.0-beta — 2026-09-26

**A space can hold at most a declared number of keys.** `DEFINE SPACE cache MAX
10000` keeps the ten thousand most recently written keys: a commit that would
go past the limit removes the least recently modified ones, never a key that
commit itself writes. `DEFINE SPACE tickets MAX 500 EVICT NONE` refuses the key
past the limit instead, with **`SpaceFull`**.

- The limit is a count of keys, not bytes, and it holds under concurrency: it is
  checked in the commit against the committed state, so two writers adding
  different keys at once cannot both pass it.
- Evictions are ordinary deletes in the committing transaction's log record, so
  followers apply them and the change feed shows them. A commit that alone adds
  more keys than the limit is refused rather than trimmed.
- A space is now its own kind: `INFO FOR TABLE` writes it back as `DEFINE SPACE
  name [MAX n [EVICT NONE]]` — it used to come back as `DEFINE TABLE …
  SCHEMALESS`, losing the word.

**1390 conformance cases** define the language and run in the build, up from
1386.

**On disk:** a space's catalog entry carries its declaration, and a limited
space keeps a modified-order index under a new key kind. A store written by
this build is not promised to open under an earlier one.

## 0.5.0-beta — 2026-09-26

**A space can be run as a cache: a key expires, a counter increments without
losing a write, a lock takes itself, and a walk of the keys reads only the
stretch it lists.** Everything here behaves the same on the memory backend and
the disk one.

- **A key can expire.** `SET sessions:'abc' = … EXPIRE 30m` (a duration from
  now or a datetime), `EXPIRE key 10m`, `PERSIST key`, and `TTL key` — a
  duration, `NULL` for a key that never expires, `NONE` for no key. Once its
  instant passes, the key is answered by **no** read of its table: `GET`,
  `KEYS`, a scan, a point read and an index-served read alike. A plain `SET`
  clears an expiry; a `SET` whose expiry is not in the future is refused
  (**`InvalidExpiry`**); an `EXPIRE` already past removes the key. The instant
  is stored in the record's version, so a follower and a restarted node judge it
  exactly as the writer did.
- **Expired keys are removed through the log.** Each expiring version has an
  entry in a new expiry index, written in the same batch as the record, and the
  node's housekeeping removes what has expired as ordinary deletes — replicated,
  fenced by leadership, and seen on the change feed. Reads never wait for it.
- **`INCR key [BY n]`** adds to a number and answers the result; a missing key
  counts from zero and an expiry is kept. **`SET … IF ABSENT | IF PRESENT |
  IF = value`** writes only when the condition holds and answers whether it
  wrote, so `SET lock = 'me' IF ABSENT EXPIRE 30s` is a lock that frees itself.
  A lone one of these that loses a race is run again for up to a second, so four
  writers incrementing one key land every increment and none is refused.
- **`KEYS FROM s PREFIX 'user:42:'`**, **`AFTER k`** and **`LIMIT n`**: a key
  walk now seeks to where it starts and stops where it ends — a prefix over
  three keys in a space of a thousand reads a handful of entries — and pages
  with no key repeated or dropped. Expired keys are never listed.
- `expire`, `persist` and `ttl` are not reserved words; they stay usable as
  field and table names.

**1386 conformance cases** define the language and run in the build, up from
1369; the key-value corpus also runs against the disk backend.

**On disk:** a record version may now carry an expiry instant behind a new flag
bit, and the expiry index is a new key kind. A store written by this build is not
promised to open under an earlier one.

## 0.4.0-beta — 2026-09-25

**A table can be split into shards, each shard can be led by a different node,
and a node holding part of a table still answers a read of all of it.**

- **`SPLIT AT`** declares a table's shards when the table is declared:
  `DEFINE TABLE orders (total int) IDENTITY uuid SPLIT AT 'g', 'p';` makes three
  shards bounded by those identities. `INFO FOR TABLE` reports each shard's
  bounds, and its definition script re-creates the split. Only a table declared
  `IDENTITY uuid` can be split, the points must be in order, and **a table's
  shards are fixed when it is declared** — there is no later split and no merge.
- **Each shard is logged, led and replicated on its own.** A commit that touches
  one shard is filed in that shard's log. A peer can subscribe to one shard with
  `REPLICATES SHARD prod.shop.orders 2`, and every subscriber now receives the
  definitions above what it holds, so its readers can name what it has.
- **`LEADS`** places a range's leader: `DEFINE REPLICA … LEADS SHARD
  prod.shop.orders 2` makes that node a candidate for the range, and a placed
  range is an election of its own, with its own epochs and lease. Writes to two
  shards led by two nodes are taken by both at once, and a shard with two
  candidates fails over between them. A transaction writing ranges that two
  nodes lead is refused as **`SpansLeaderships`**, naming both. A row carrying
  `LEADS` cannot be dropped (**`PlacementCannotBeDropped`**) in this release.
- **A read of a split table is gathered.** A node holding some shards answers a
  `SELECT` over the whole table by fetching the shards it lacks from their
  leaders over three new peer frames, and runs the statement itself, so grants
  and hidden fields apply exactly as they do locally. The answer carries the note
  **`gathered`**, naming the shards fetched, because it is not one snapshot. A
  read inside a transaction or under `VERSION` is not gathered and is refused
  with **`NotHeldHere`**; a shard nobody can serve refuses the whole read with
  **`NotGathered`**; more than 100 000 records is **`GatheredTooMuch`**. Nothing
  is ever answered in part.

**Two replication fixes change what an existing follower holds. Re-bootstrap
every follower that was bootstrapped before this release** — a fresh copy of
the state, which `Error::BelowLogStart`'s repair describes.

- **A user's grants now travel wherever the user goes.** Users reached every
  subscriber while grants reached only whole-store ones, so a namespace, database
  or shard follower held a restricted user **unrestricted**. A follower
  bootstrapped earlier still holds the user without the grant.
- **A follower applies one writer's logs in the order the writer committed
  them.** It used to collect the store, namespace, database and shard logs one
  after another, and could end at an **older value than its leader** when a
  record was written in one log and then in another. Each log record now carries
  its writer's commit order and a collection round merges every log by it. A
  follower that diverged before this build keeps the older value.

- **The console shows what an answer says about itself.** The query pane prints
  every note a read carries — `gathered` among them — where it used to drop
  them, the cluster map shows each peer's `LEADS` placement, and the sentence
  that said there is no sharding is gone.
- **`--health` no longer reports a store's history as position zero** after an
  upgrade from a log older than the writer-qualified format; the sentence says
  whose position it is and how far the other log reaches.

## 0.3.0-beta — 2026-09-17

**The log stops growing, a read can say where its answer must come from, and how
long a cluster waits is something an operator writes down.**

- **`DEFINE NODE RETAIN 100000 RECORDS`** bounds the log, and `RETAIN NONE` puts
  it back. **The default is off**, so a store that says nothing keeps the whole
  log exactly as before — this release changes nothing for anyone who does not
  ask for it. `INFO FOR NODE` reports the window as `retain`, where `null` means
  *unbounded* rather than *very large*. The setting is local, like `ROLES`: a
  disk budget describes one machine, so it does not replicate and a restored
  backup does not inherit it.

  The count is the whole bound, deliberately. A reader inside the window is safe
  because it is inside it; one further behind is **refused by name**, told where
  the log now begins, and needs a fresh copy of the state. Holding the log down
  to whatever the slowest reader still needs is how one stuck subscriber fills a
  disk with nothing anywhere in an error state. The last record always survives
  whatever number is set, and `RETAIN 0 RECORDS` is refused rather than clamped.
  Space returns when the store next compacts, not at the statement.

- **`SELECT … ANSWERED BY LEADER`** admits only the node that decides writes for
  those records; `ANSWERED BY ANY` is what a read means when it says nothing. It
  is not a tighter `STALENESS` and the two compose — a follower at zero lag is
  *level*, not authoritative, so no freshness bound can express *this must come
  from where writes are decided*. Which node may answer is settled first, because
  the leader satisfies every bound and deciding it first therefore cannot
  overturn the freshness decision. A node that does not lead redirects to one
  that does, or refuses when it knows of none.

- **`DEFINE FAILOVER AWARENESS … COLLECTION … ROUND … CAMPAIGN … LEASE …`** sets
  how long the cluster waits before replacing a leader. It is a replicated row
  rather than a file or a flag, because two nodes holding different files is not
  a conflict anything detects: each is internally consistent, and the
  disagreement surfaces as two nodes that both believe they may write. All five
  clauses are required — the periods are checked against one another, so a
  partial statement could only mix new values with old ones. Four relations are
  enforced and each refusal names the direction to move. A policy carries the
  leadership it was written under and installs only if that pair outranks the one
  already held; neither number is a clock.

- **A three-node cluster replicates.** Three faults sat between an election and a
  replica, each invisible until the one above it was removed: a refused collect
  absorbed and never logged, a cursor seeded from this node's own log rather than
  the peer's, and a node counting the replicated membership row that names
  *itself* among its voters.

- **A membership row is identified by the peer it names.** Two nodes declaring
  different peers used to overwrite one another's row — same allocated id, no
  error, row count unchanged.

- **A node holding data of its own is refused a join.** A cluster allocates
  namespace ids from its own counter, so collecting would replace the definition
  at the address that node's records are filed under, and every one of them would
  then be read through another tenancy's name, schema, replication class and
  grants. Nothing would be deleted, which is what made it worth refusing. There
  is no *join anyway* switch: remove the namespaces with `DROP NAMESPACE`, which
  walks you down your own tree, or join on an empty store.

- `INFO FOR HISTORY OF` no longer calls itself `complete` when the log begins
  above its beginning.

## 0.2.2-beta — 2026-09-16

**The source is public, and every link points at its canonical name.**

- **The engine repository is public.** TessariDB is published at
  `github.com/tessaridb/tessaridb` under BUSL-1.1, alongside the protocol
  specification and the Rust SDK, which were already public.
- **Repository and organisation names are lowercase.** GitHub resolves owner and
  repository names case-insensitively, so nothing about this changes behaviour —
  the written form now matches the canonical one, which matters because
  `Cargo.toml`'s `repository` field is carried into package metadata and a Go
  module path encodes each uppercase letter as `!<lower>`.
- Two documentation comments that referred to internal tooling by name were
  rewritten to state the reasoning directly. No behaviour changed.

## 0.2.1-beta — 2026-09-16

**A node can be drained, and a record can be asked what happened to it.**

- **`DEFINE NODE ROLES NONE`** clears the roles of the node the statement runs
  on. The node keeps its data, its identity and its place in the membership, and
  stops answering clients — which is how a machine is taken out of service
  without being stopped. The empty role set was already a state the store could
  hold; what was missing was a way to ask for it. `NONE` is a whole answer and
  not a member of the role list, and `DEFINE REPLICA … ROLES NONE` stays refused,
  because a peer is declared rather than amended and an absent `ROLES` already
  clears there.
- **`INFO FOR HISTORY OF person:1`** answers what a record became and when, one
  entry per write, newest first. Nothing is written to produce it: every commit
  already records the address of what it changed, so this is a reading of the
  log rather than a second event store. The walk is bounded and the answer says
  when it stopped short of the log's beginning, so a short list is never mistaken
  for a whole one. It is not `INFO FOR VERSIONS OF`, which reports whether a
  record is contested and answers one version however many times it was written.
- **The console** gains the drain on a node's drawer — which names what a drain
  costs before it offers the button, and says when a membership row would
  override it — and draws a record's history on its detail sheet. A namespace, a
  database and a table have none, and the sheet says why rather than showing an
  empty list.

The log still grows without bound and nothing prunes it; that is unchanged by
this release, which only reads it.

## 0.2.0-beta — 2026-09-15

**The cluster.** A node joins a running cluster, leadership is elected and
fenced, a namespace says how many copies are kept and how many nodes may write
it, a read may say how stale an answer it will take, and two nodes may write one
range. The middle number moves because the language and the on-disk format both
grew: a store written by `0.1.x` is rewritten once at open, and there is no way
back.

**A node joins rather than being configured.** The newcomer declares **no
membership of its own** — one `--seed` naming a node and an address, and the
membership reaches it through the same replication as everything else. That is
not a convenience: a cluster whose nodes were each told their own membership
before starting holds several logs carrying *different records at the same
positions*, written under no leadership, so nothing reports it. A node that
writes no membership row cannot produce that. The seed is read only while the
catalog names no peer other than this node itself, so a restart needs no flag.

**A leadership is a lease, and it fences before it is replaced.** A node whose
catalog names another node writes under a leadership and at no other time; until
a majority of the coordinating members grants it one it refuses writes, with a
refusal that names the remedy. The lease counts down to the moment **this node**
stops, deliberately earlier than the moment the cluster may hand the leadership
to somebody else — the gap covers two clocks nobody synchronised. It is measured
on elapsed time, because a clock that steps backwards would *extend* a fence.

**A write for a range another node leads is refused with somewhere to go.** The
refusal names the address, the node to expect there and the epoch that node took
the range under, and it is a different sentence from *holds no leadership*: the
first says go there, the second says wait. A redirect reaches a client as a
**frame** with a `settled`/`transient` discriminator rather than as an error, and
maps to `307` over HTTP.

**A read may name how stale an answer it will accept.** `STALENESS` is a
**candidate filter, never a marker** — it decides which nodes may answer rather
than labelling the answer, because a marker nobody is obliged to read is not a
guarantee. A bound tighter than twice the awareness interval is refused **with
the floor named**, and a read no node can satisfy is refused rather than quietly
promoted to the one node that certainly can.

**Two writers on one range, and the conflict is named rather than resolved.**
`DEFINE NAMESPACE … MULTI MASTER` admits writes on more than one node. What that
gives up is single-copy semantics: two nodes can each write one record without
seeing the other, and **neither version is newer** — no clock settles it, and the
engine cannot tell you which is right because nothing knows. So a write onto such
a record is **refused**, naming the record, both surviving versions and a node
whose write one carries and the other has not seen. `DEFINE TABLE … LAST WRITER
WINS` is the declared alternative; the last writer is the **caller** and not a
timestamp, and every version it discards is **counted**, because a loss nobody
records is a loss nobody can check. `INFO FOR VERSIONS OF person:1` reads it
back, on a single-leader range too.

**The log is per range, and a backup carries one section per log.** The sequence
and the epoch are properties of a range rather than of the machine, so a key
carries its home. A whole backup and a whole restore are unchanged; a **bounded**
one — `--from`, `--upto` — refuses a multi-log store, because one sequence means
nothing across several logs and backing up one range while calling it the store
is the failure that would not announce itself.

**A record's version is its own counter.** It had been the log position a replica
resumes from, the version every mutation is stamped at, and the applied-position
guard, all at once — which worked only while a single leader made every timeline
the same timeline. The new counter is seeded at open from the applied position,
so existing records keep the numbers they hold.

**Index maintenance chose definitions by table id alone, and a table id is not a
key on its own.** Ids are handed out store-wide while the system catalog reserves
the first eighteen, so a catalog write selected the indexes of whatever user
table shared its number and wrote its value into that user's keyspace. Two
databases in different namespaces could not share a name, and a record could not
hold a value that was also some database's name — both refused by an index
neither statement mentioned, and only ever when a value happened to collide.
Selected by the whole tenancy now.

**`INFO FOR NODE` answers in two named groups**, the flat fields describing this
machine and everything under `cluster` describing the topology, because that is
the line a backup must not cross. It reports each peer's subscription, which
followers have actually collected and how far behind each is in **both**
sequences and time, and the time left on this node's lease.

### What is still absent

- **The nearest few is over positions, and the spatial index is untuned**, as in
  `0.1.0-beta`.
  <!-- absent: distance-between-two-larger-shapes -->
  <!-- absent: nearest-first-under-a-where -->
  <!-- absent: measured-covering-budget -->
- **No sharding.** A namespace lives where its replication says and a range is
  led by one node or declared open to several, but the store does not split a
  range across machines and there is no scatter-gather read.
  <!-- absent: sharding-execution -->
- **No cross-range transaction.** A transaction is judged on the node leading the
  range it writes.
  <!-- absent: cross-range-transactions -->
- **Not published to crates.io.** Every crate carries `publish = false`.
  <!-- absent: published-to-crates-io -->
- **No migration between versions**, with the one exception above: a `0.1.x`
  store is rewritten once at open, one way.
  <!-- absent: migration-between-versions -->

## 0.1.1-beta — 2026-09-11

A patch release, and the reason to cut one rather than wait is the first
entry below: a write could drop a hold by saying nothing about it, and that
defect is live in `0.1.0-beta`. Everything here is additive — every statement
that parsed under `0.1.0-beta` parses here and means the same thing — which is
why the middle number does not move.

**A write cannot drop a hold by saying nothing about it.** The store writes
`claimed_until`, `attempts` and `claimed_by`, and a caller that names one of
them is refused. A caller that **omits** them was not — and a whole-record
`UPDATE jobs:1 = { url: 'b' }` omits every field it does not mention, so the
record came back with no hold, no deadline and no attempt count. Nothing was in
an error state, and the work was claimable again while its holder still believed
it held it: the failure the conditional `UPDATE` below was built to prevent,
reached by a door nobody had checked, because every test of the guard used
`SET` — which merges over the stored record and so carries the three fields
along by accident.

The two halves are now one rule. A caller may not introduce or change one of
those fields, and it follows that a caller may not remove one either, so a write
that leaves them out carries them forward. Refusing such a write would have been
the other reading and is the wrong one: the conditional whole-record write is
exactly the compare-and-set a consumer holding work performs.

The attempt count is the half with teeth of its own. A ceiling a caller can
clear by rewriting the record is not a ceiling — a record that had poisoned
three workers could be recycled by the fourth, indefinitely. That is fixed by
the same rule.

**`DEFINE QUEUE` says whether it is strict and which graph it is in.** A queue
was described from the start as an ordinary table plus a hold that lapses, but
its declaring word could say neither of the two things an ordinary table says
about itself. That cost more than symmetry: a work table in a record model is
normally strict and is normally an end of a link, and such a table could not be
a queue at all — `DEFINE EDGE` refuses a table belonging to no graph, declaring
the table first and the queue second is refused because the name is taken, and
reaching strictness through `ALTER TABLE` leaves a table `INFO FOR TABLE` can no
longer write back as a statement.

`DEFINE QUEUE tasks TIMEOUT 10m SCHEMAFULL IN work` now says both. The flags
follow `TIMEOUT` and `ATTEMPTS`, are order-free against each other, and neither
is accepted twice — the same reading `DEFINE TABLE` gives its own. **A queue
that names no flag means exactly what it meant before**: lenient, and in no
graph, which is the rule a declaration carrying no columns already followed.
`INFO FOR TABLE` writes the strictness word back with the rest; a queue in a
graph is still reported as undefinable, in the same sentence as every other kind
in a graph, because that writer holds a graph id and nothing that resolves one
to a name.

**`UPDATE … WHERE` — the language has a compare-and-set.** Until now TessariQL
offered exactly one: a conditional `DELETE` as the guard followed by a `CREATE`
as the failure signal, because a create over a record that is still there is
refused and discards the transaction. It is correct, and against a queue it is a
silent disaster — a claim lives **on** the record, so recreating the record drops
the hold with no error at all, and the work is claimable again while its first
holder still believes it holds it. Any store that versions its records and wants
the queue hits this, so the fix is in the language rather than in a consumer.

`UPDATE orders:7 SET status = 'paid', version = 4 WHERE version = 3` changes the
record only if it still says what the caller last read. A condition that does not
hold is a **refusal** and not a count, which follows from what this verb already
is: `UPDATE` asserts the record is present and refuses when it is not, so
asserting it is in a particular state is the same assertion one step further in.
The failure discards the work above it in the transaction, which is what makes
the clause a guard rather than a filter — a guard a caller can forget to check is
not one. The condition reads the record **as stored**, so `WHERE version = 3`
beside `SET version = 4` compares the value that is there. `UPSERT` takes no such
clause and says so, because it asserts nothing about the record it writes.

**A file route cannot delete a record out of an ordinary table.**
`DELETE /files/{ns}/{db}/{name}/{path}` ran a plain record delete, which asks
nothing about whether the name is a bucket — so it answered `204` against any
table at all, reporting a file removed from a bucket that does not exist, and
removed the record outright whenever one carried that path as its id. `PUT` and
`GET` were never exposed to this because they are file statements and resolve the
bucket themselves; a delete is not one, and nothing re-checked it when the
listing route was fixed. It now asks `INFO FOR BUCKET` first, so all four routes
agree at last about what a bucket is.

**A bucket that is not there answers `404`.** All four `/files` routes refused a
name that is not a bucket with `400`, and nothing had chosen `400` — it is where
the error map sends everything it has no arm for. A request for a bucket that is
not there is not a malformed request, and `400` told a caller they had written it
wrongly, which was the one thing they had not done. The whole file surface now
reads one way: **`404` means it is not here**, whether the missing part is the
file or the bucket, and the sentence in the body says which. A missing namespace
or database is unchanged, and the protocol specification now states all of this.

**A claimed queue record can be written to again.** Taking a job and then
recording anything on it was refused — `UPDATE jobs:7 SET stage = 'fetched'` on a
record you hold came back naming `claimed_until`, a field you had not typed,
while the same statement on the same table succeeded whenever the record happened
to be free. The guard that keeps `claimed_until`, `attempts` and `claimed_by` the
store's own was reading the whole record about to be written, and on a held record
that carries all three whatever the statement said. It now refuses a caller who
**introduces or changes** one of them; carrying one forward untouched is not
writing it. The three refusals it existed for are unchanged, and the hold, its
deadline and its attempt count survive the update.

**Released.** Tagged `v0.1.1-beta` on `main`, and published as
[`tessaridb/tessaridb`](https://hub.docker.com/r/tessaridb/tessaridb) —
`0.1.1-beta` and `latest`, `linux/amd64` and `linux/arm64`.

## 0.1.0-beta — 2026-09-10

**Released.** Tagged `v0.1.0-beta` on `main`, and published as
[`tessaridb/tessaridb`](https://hub.docker.com/r/tessaridb/tessaridb) —
`0.1.0-beta` and `latest`, `linux/amd64` and `linux/arm64`.

a session, `CLAIM` signs the hold with it, and `RELEASE ALL FROM jobs` hands back
everything that session holds — answering the records it freed rather than a
count. Releasing somebody else's hold is refused, naming them; before this, any
caller who could write the table could drop any hold with nothing anywhere
saying so.

**The name is shared on purpose and the identity that must not collide is not
yours.** Several workers under one name divide the work between them and none
displaces another — that is what one declared name means. Only the *instance* is
unique, and the store mints it, so two workers cannot collide however they are
configured and there is nothing to fence.

`RELEASE ALL FROM jobs FOR CONSUMER 'billing'` reaches the whole group, including
live sessions, which is how a restarted worker reclaims what its predecessor
left. It is spelled out because it can take work from somebody still doing it.

**A queue may be `SCHEMAFULL`.** It could not be before: the three fields the
engine writes onto a record it hands out — `attempts`, `claimed_until` and
`claimed_by` — are declared by nobody and are written after a caller's record has
been validated, so a strict queue refused every claim it had just taken. They are
now excused by the table's kind, exactly as a vault's key set is, and a caller
who writes them is still refused by the guard that says they are the engine's.

`RELEASE jobs:7 FOR CONSUMER 'billing'` is the same choice about one record. The
bare form compares instances, so it frees only what this session took; the named
form compares group names. A client that holds one connection for many logical
callers needs it, because it is minted a fresh instance every time it declares a
name and its own earlier hold would otherwise belong to somebody else. A hold
nobody signed belongs to no group and the named form leaves it untouched.

A session that declares nothing signs nothing, and its holds stay releasable by
anybody — so nothing written before this changes behaviour. A different consumer
name does **not** replay the queue: records are deleted when the work is done, so
a name scopes releasing and reading and nothing else.

**Broker ingestion now names its broker: `DEFINE KAFKA CONSUMER`.** The
statement is otherwise unchanged — same clauses, same guarantees, same
refusals — but `DEFINE CONSUMER`, `DROP CONSUMER` and `INFO FOR CONSUMER[S]`
are gone and their bare spellings are **refused with a message naming the
replacement**, rather than left to mean something else later.

The reason is that the general word was being held by one broker. Every noun in
the statement — brokers, topic, group — comes from Kafka, so naming it is
honest, and it leaves room for a second broker to be added without a fight over
whose statement `DEFINE CONSUMER` is. The word itself is wanted back for a
queue's readers, which is what a consumer is everywhere else.

`KAFKA` is **contextual and not reserved**: it is a subject after `DEFINE`,
`DROP` and `INFO FOR`, and an ordinary identifier everywhere else, so a table,
field or parameter called `kafka` is still spellable. That is asserted by a
test rather than claimed.

**A view is a name for a read.** `DEFINE VIEW engineers AS SELECT * FROM staff
WHERE team = 'eng'`, and then `SELECT * FROM engineers` anywhere a table can be
read. Nothing is stored under it and nothing is maintained: a statement naming a
view is rewritten to carry the read before anything else happens, so the read
runs the way any other read does and `INFO FOR TABLE` answers with the statement
that declared it, character for character.

Three things about it are decisions rather than details, and each is in the
reference beside the statement.

**It is a name for a read, not a faster way to run one.** The view's read is held
whole before your statement asks anything of it, so `SELECT * FROM engineers
LIMIT 1` reads what the view reads — it does not stop where the answer fills, the
way the same statement over a table does. It is bounded rather than unbounded: a
view naming no `LIMIT` runs under the ceiling every held read runs under, and
past ten thousand records the read is **refused** rather than shortened.

**A view reads with the caller's permissions, and a grant on a view is refused.**
The tables the view names are checked against your own grants, exactly as if you
had written the read out by hand — so a caller who may not read `staff` may not
read a view over `staff`, and the refusal names `staff`. Views elsewhere often
run with the authority of whoever declared them; that is a separate decision with
its own consequences and is not what this word does today.

**Maintained results are the change feed's job.** There is no `MATERIALIZED`
spelling and no plan for one: a store that needs a maintained result writes one
into an ordinary table from the change feed, where the writes it must react to
already are — and the result is then a table you can index, back up and grant on.
Maintaining it inside the writing transaction would make every write to a table
pay for every view over it, silently.

A view is the ninth table kind, so the on-disk format changes only by a field on
a table definition that did not exist. The downgrade is sharper than the queue's
and is stated rather than defended: a build that predates views reads a view's
entry as a plain table over an empty prefix, so `SELECT` answers **nothing**
rather than the view's records.

**An engine was added.** A **queue** is a table whose records are handed out one
holder at a time under a hold that lapses — `DEFINE QUEUE jobs TIMEOUT 30s
ATTEMPTS 5`, then `CLAIM FROM jobs`, `DELETE jobs:7` when the work is done and
`RELEASE jobs:7` to hand it back early. That makes ten engines over one substrate
rather than nine. **1369 conformance cases** define the language and run in the
build, up from 1237.

The design is the part worth reading, because a queue is normally where a store
grows a lease manager and this one does not. A claim is an ordinary **write**, so
it is sequenced into the log and replicated by the mechanism every other write
uses. The instant it lapses is computed once by the session taking it and
**written into the record** — the rule `time::now()` already follows, so a
replica applies what was written rather than asking its own clock. And a hold
lapses because a later reader finds that instant in the past: the comparison is
the expiry, so there is no reaper, no timer and no state outside the log to
rebuild after a restart.

Exclusivity needed no new machinery either. Two workers that pick one record both
write that record, which the store's snapshot isolation already resolves — the
first committer wins and **the loser is refused**, writing nothing. The retry
belongs to the worker: the store does not quietly re-select the next record on
its behalf, so a worker loop treats a write-conflict refusal the way it treats an
empty answer, and one that has steady work claims a batch. This paragraph said
"and re-selects" until it was measured with four competing processes; the
sentence was wrong and the correction is here rather than silent.

Delivery is **at-least-once**, the same guarantee `DEFINE KAFKA CONSUMER` states, and
`CLAIM` is **not idempotent**: a worker whose reply is lost and which asks again
gets a different record while the first stays held until its deadline. Both are
written into the reference rather than left to be derived.

The on-disk format changes only by a field on a table definition that did not
exist. A definition written by `0.0.6-beta` reads back unchanged, and a store
holding a queue opened by `0.0.6-beta` reads that table as a plain table —
records intact, and every refusal the word carries gone.

**The `0.0.3-alpha` section below says 1105 again**, which is what its own tag
carries. Successive waves had been raising that number to the corpus's current
size so that the check requiring the CHANGELOG to state the badge's figure would
pass, which quietly made a released section describe a release it is not about.
The count above satisfies that check instead.

## 0.0.6-beta — 2026-09-08

### The release itself

**Released.** Tagged `v0.0.6-beta` on `main`, and published as
[`tessaridb/tessaridb`](https://hub.docker.com/r/tessaridb/tessaridb) —
`0.0.6-beta` and `latest`, `linux/amd64` and `linux/arm64`.

**This release adds an engine.** A **vault** is a table whose declared fields
marked `SECRET` are encrypted before the record is written, which makes nine
engines over one substrate rather than eight. It changes the on-disk format only
by adding a record shape that did not exist; a store written by `0.0.5-alpha`
opens and reads under it unchanged, and a store holding a vault does not open
under `0.0.5-alpha`.

**The suffix moves from `alpha` to `beta`**, and the compatibility promise does
not move with it: this file's opening paragraph still governs. Versions before
1.0 do not promise compatibility with each other, and there is still no migration
between them.

The claim the whole engine is written against is narrower than "the database
cannot read it", and it is worth reading exactly:

> **The stored bytes are never plaintext, no key that opens them is written to
> disk unwrapped, and a sealed or restarted node cannot open anything for
> anybody.**

A running, unsealed server *can* decrypt — it has to, to answer `REVEAL`. What a
vault takes away is every copy of the data that is not that running process.

### Added

- **`DEFINE VAULT`, and `SECRET` on a field declaration.** The sealing happens
  below the layer that serves reads — the ciphertext *is* the stored value — so
  the search index, the change feed, the replication log and every backup carry
  ciphertext without any of them having to know what a vault is. That placement
  was chosen by measurement: an exfiltration survey found no generic read path
  in this store consults the table kind, so sealing at the session layer would
  have left plaintext in all four.

- **`UNSEAL VAULT WITH '…'` and `SEAL VAULT`.** The master key exists in memory
  between those two statements and nowhere else. It is per process, so a restart
  seals the store and an unattended restart is impossible — a real operational
  cost, stated here rather than discovered during one. The passphrase is a quoted
  string rather than an expression, because an expression would put a secret
  through the evaluator.

- **`REVEAL`**, the only statement that turns a sealed value back into plaintext.
  It names one record and takes no `WHERE` and no `ORDER BY`: a filter over a
  secret is an oracle that answers one bit per statement, and an ordering is the
  same oracle more slowly.

- **`ADD RECIPIENT` / `REMOVE RECIPIENT` / `INFO FOR RECIPIENTS OF`** — an opaque
  set the engine carries and never interprets. What a recipient's material *is*
  stays yours to decide, because deciding it here would mean holding the second
  key hierarchy that answers it.

- **An audit trail every `REVEAL` writes to before its answer leaves**, and a
  read that cannot be recorded is refused rather than served. `INFO FOR AUDIT`
  and `INFO FOR AUDIT BY '…'` read it back, answered only to a caller who
  administers the whole store — the trail is store-wide because a read is
  recorded before anybody knows whose tenancy it belonged to.

- **`DROP VAULT`**, which destroys the key rather than the rows. That is the only
  deletion claim a store can honestly make. Where it stops is documented: a
  backup taken *before* the drop restores a vault that opens, so destroying a
  secret is two acts — drop the vault, and expire the copies that predate it.

- **A quoted field name** in `DEFINE FIELD`, `ALTER TABLE … ADD/ALTER/DROP FIELD`
  and `REVEAL`. Found because a vault's flagship example was impossible: a field
  called `password` could be written in an object literal and could not be
  declared, and a vault is strict, so it was a field nothing could hold.

### Changed

- **A vault record is edited field by field**, and this is the difference worth
  knowing before you rotate a secret. An edit seals only the fields it names,
  leaves every other envelope untouched, and reuses the record's own data key —
  so every wrap made by `ADD RECIPIENT` still opens. Replacing the record whole
  mints a fresh data key, so the recipient entries are cleared.

  What is refused is narrow: an assignment on a vault may not **read** the
  record. The fields an edit names are the fields you supplied, so writing them
  needs nothing opened; an expression that reads the record would have to open a
  sealed value to answer, and opening a secret is `REVEAL`, which records itself
  before it answers.

- **`INFO FOR`'s subject list names `GRAPH` and `RECIPIENTS OF`**, both of which
  had parsed since they existed without appearing in the message that lists what
  it accepts.

### Not defended against, published with the engine

Eight things, listed in full in the language reference and on the documentation
site. The one worth repeating here is the eighth, because it is the one whose
obvious fix does not work: **an attacker who can write to the storage backend
gains a rollback.** Restoring one record's stored bytes from an older copy makes
the secret that was rotated out live again, because the envelope binds the table,
the record and the field — everything identifying *which* value this is, and
nothing identifying *when*. A version bound into the envelope is restored along
with the ciphertext it was meant to police, so both sides of the comparison move
together; closing it needs a counter outside the store, and an embedded engine
whose only durable state is the attacked backend has none. Rotate at the source,
and treat backend write access as equivalent to holding the secrets.

## 0.0.5-alpha — 2026-09-07

**Released.** Tagged `v0.0.5-alpha` on `main`, and published as
[`tessaridb/tessaridb`](https://hub.docker.com/r/tessaridb/tessaridb) —
`0.0.5-alpha` and `latest`, `linux/amd64` and `linux/arm64`.

**This release changes no grammar and no on-disk format**, and a store written by
`0.0.4-alpha` opens and reads under it unchanged. It changes one **answer**, and
that change is a fix: a record updated into a match inside its own transaction
was missing from a full-text read that reported itself served by an index.

Everything else here is about what a read **costs**. Three of the four are the
same fault in different places — work done to produce records nobody asked for.

### Fixed

- **A record this transaction just wrote is now found by a full-text match in the
  same transaction.** Every index read settles the transaction's own writes after
  its walk; the two term reads were the exception, so `MATCHES` could miss a
  record the same transaction had just updated into matching. Reproducible on
  `0.0.4-alpha`. The field's analyzer is now passed down from the session rather
  than resolved a second time in storage — computing it in two places is what
  once let a query tokenise differently on the index side and answer correctly
  with nothing.

### Faster

- **A `LIMIT` behind a `WHERE` no longer reads the whole table.** The bound
  cannot be pushed into the source, because it counts records that *match* while
  the source counts records that *exist*; it now arrives as the break the
  consumer already returned. A match found at the third of 100 000 records went
  **83.3 ms → 1.2 ms**, `LIMIT 5` over 12 435 matches 86.8 → 1.1, `LIMIT 10` over
  everything 94.3 → 1.1.

  A read whose bound is never filled still costs the table, correctly — finding
  out that nothing matches *is* reading the table. `ORDER BY`, `AFTER`, `SPLIT`,
  `FETCH` and grouping each take the bound away rather than answering short.

- **A bounded index-served read stops fetching where its answer fills.** Every
  candidate record used to be built before the caller could stop, so a bound
  bought nothing: `WHERE n > 0 LIMIT 10` over 100 000 candidates cost **141.8 ms
  → 27.7 ms**, `LIMIT 1` 144.6 → 27.6, and 10 000 candidates 15.2 → 3.5.

  The **entry walk** deliberately still runs to the end. A bounded read answers
  the records a scan of the same predicate answers, so which candidates hold the
  lowest identities is not known until all of them are named — an index narrows
  a read and never changes what it returns.

- **An index-served `MATCHES` no longer re-analyses each candidate's whole text**
  to re-test the clause the index answered exactly. On a common word the re-test
  *was* the query: `MATCHES 'the'` **22.6 ms → 1.7 ms**, and a scored search
  23.4 → 2.4. It is skipped only where believing the index and re-testing it
  answer the same records — a plain conjunction of terms, no phrase, no `NOT`,
  no capped expansion — because that re-test also enforces field-level redaction,
  the reader's snapshot and the transaction's own writes. `PREFIX` and `FUZZY`
  keep it.

- **The command line no longer re-scans a script from the beginning for every
  statement.** Inside an open transaction the statement scanner restarted at the
  start of the buffer each time, so loading a script looked quadratic — 76 µs per
  write at 500 records and 589 µs at 8 000. That cost was the client's, not the
  engine's.

## 0.0.4-alpha — 2026-09-06

**Released.** Tagged `v0.0.4-alpha` on `main`, and published as
[`tessaridb/tessaridb`](https://hub.docker.com/r/tessaridb/tessaridb) —
`0.0.4-alpha` and `latest`, `linux/amd64` and `linux/arm64`.

**This release changes no answer, no grammar and no on-disk format.** It is one
performance fix, and it is the first release since `0.0.1-alpha` for which a
store written by the previous version opens and reads identically — the index in
the corpus this was measured against was written by the old build and answers the
same records under the new one.

### Faster

- **Full-text search is 5.6× faster on every path — index, scan, ingest and
  highlighting alike.** They all run the field's declared analyzer, and the
  analyzer's cost was almost entirely the stemmer: measured over 404 documents
  and 58 350 words, tokenising and lower-casing took 4.3 ms, adding the ASCII
  fold 6.1 ms, and adding the stemmer **121.6 ms**.

  The stemmer was bound by the allocator rather than by the algorithm. Testing
  whether a word ends in a suffix built that suffix as a vector of characters
  first, and Porter2 tests sixty-odd suffixes per word — twenty-five in step 2
  alone — so a word cost sixty-odd heap allocations before any letter was
  compared. Two more places rebuilt the whole word as a string per call for the
  same reason.

  Comparing letter by letter instead: stemming fell from **1.98 µs to 0.27 µs per
  word**, the full analyzer chain over that corpus from **121.6 ms to 21.8 ms**,
  a selective indexed `MATCHES` from **9.7 ms to 1.8 ms**, and loading the corpus
  — which stems every word again to build the index — from 0.30 s to 0.20 s.

  The stems themselves are unchanged, which is the property that matters: the
  published Porter2 worked examples and exception tables pass as before, and the
  records returned for the same queries are byte-identical against the previous
  build.

## 0.0.3-alpha — 2026-09-05

**Released.** Tagged `v0.0.3-alpha` on `main`, and published as
[`tessaridb/tessaridb`](https://hub.docker.com/r/tessaridb/tessaridb) —
`0.0.3-alpha` and `latest`, `linux/amd64` and `linux/arm64`.

This release is full-text search. `MATCHES` could ask for a whole word and score
it; it can now ask for the word a reader has started typing, the word they meant
rather than the one they typed, a phrase, either of two words, and not a third —
and it can say where in the text it matched. 1105 conformance cases define the
language and run in the build, up from 1035.

Nothing here changes an answer a `0.0.2-alpha` statement already gave. Every
operator below is new surface, and the on-disk index format changed to carry it —
so, as ever before 1.0, **a store written by `0.0.2-alpha` is not promised to
open under this one.**

### The words a reader has started typing

`MATCHES PREFIX 'vecto'` asks for a word **beginning with** each word typed: a
conjunction across the query, a disjunction inside each word. A separate operator
rather than a `*` inside the string, because a wildcard would make every query a
parse of the caller's own data.

The minimum is three characters and it is a refusal (`PrefixTooShort`), raised
before an access path is chosen — so adding an index can never change whether the
statement runs. The expansion cap is *not* a refusal: a prefix reaching more than
sixty-four terms is answered by the scan instead, because a cap that refused would
make a statement succeed on a table with no index and fail once somebody added
one.

### The word they meant

`MATCHES FUZZY 'vectr'` asks for a term within two edits. It is declared, never
automatic: a query that finds nothing is not retried as a fuzzy one behind your
back, because a reader shown three candidate spellings cannot tell which the
store decided they meant.

The first three characters are not fuzzy, and that is part of what the operator
**means** rather than a trick the index plays — the scan applies the same rule, so
the answer does not change when somebody declares an index.

### Phrases, either, and not

A quoted `MATCHES '"ada lovelace"'` is a phrase, and `~n` after the closing quote
declares its slop. A tail that is not `~` and a whole number is refused
(`MalformedSlop`) rather than read as an exact phrase.

`OR` unions the word beside it and `NOT` excludes the word after it. The operators
are uppercase and that is load-bearing: they are recognised on the query as
written, before analysis, which is what keeps `salt or pepper` meaning three
words. A query that excludes without requiring is refused
(`NegationWithoutTerm`) — it names the complement of a posting list, which is the
one thing an inverted index cannot enumerate.

Weighting a field is multiplication over two scores. There is no `^3` form,
because arithmetic already does it with precedence a reader knows.

### Did you mean

A read answers with a `suggestion` beside its records when a term the query named
is one the collection does not hold. **It never enters the executed query** — the
records returned are exactly the ones the statement asked for, whether or not a
correction was found.

The trigger is a term the collection does not hold rather than an empty answer,
because a query with one word misspelled usually still returns records. An
excluded term is left alone: correcting an exclusion is the one direction of error
that removes records the reader wanted.

### Where the text matched

`search::highlight(body)` answers an array of `{ start, end }` byte ranges, one
per token this read's own query reached. It takes the field **alone** — the terms
are whatever the statement already asked of that field, so a second copy of the
query cannot disagree with the `WHERE` about what matched.

The marks cover the text that matched rather than the characters typed: a search
for `cafe` marks `Café`, and a fuzzy search for `vectr` marks `vector`.

### The best few, without scoring the rest

A ranked read carrying a `LIMIT` no longer scores every record. It walks the
postings of the query's own terms and stops walking a term once the most it could
still contribute falls below the score already in last place. `EXPLAIN` reports
access `ordered` with shape `scored`.

The result is **exact** — the same rows scoring the whole table would have put
first — and exactness is now a returned property on every plan rather than
something a caller has to infer from the access path.

### Also

- A term dictionary, with a term's frequency as a point read rather than a walk.
- A score's per-record numbers are read from the posting itself.
- A projected ranked read takes the bound too: `SELECT title, search::score(…) AS
  score … LIMIT 10` was refused the bound that `SELECT *` received, for a reason
  inherited from a sibling recognizer and measured away.
- The node runs as a launchd agent on macOS.

## 0.0.2-alpha — 2026-09-02

**Released.** Tagged `v0.0.2-alpha` on `main`, and published as
[`tessaridb/tessaridb`](https://hub.docker.com/r/tessaridb/tessaridb) —
`0.0.2-alpha` and `latest`, `linux/amd64` and `linux/arm64`. The image carries
the binary and nothing else: no source, no toolchain, 108 MB.

This is the first version anybody can obtain without access to this repository.

1035 conformance cases define the language and run in the build.

The dates above and below are the days the versions were released. Work on this
one began on 2026-08-27, which is what this heading said while it was still
unreleased.

### Breaking — a table is not a document

`DEFINE COLLECTION` is the word for records that carry fields nobody declared,
and `DEFINE TABLE` now means what it says: a table that declares its fields
refuses the ones it does not. Three changes follow, and each has a one-line
answer.

| a script that says | now | write instead |
|---|---|---|
| `DEFINE TABLE t;` | refused | `DEFINE COLLECTION t;` if its records carry undeclared fields, `DEFINE TABLE t SCHEMALESS;` to keep the old reading exactly |
| `DEFINE TABLE t (a string);` then a record carrying `b` | refused | declare `b`, or add `SCHEMALESS` |
| `DEFINE TABLE t EDGE;` | unchanged | — an edge table declares no columns because the store supplies `out` and `in` |

**Nothing stored is redefined and no migration step is owed.** Strictness has
always been a property of the stored table rather than of the default, so a
table declared lenient before this release goes on being lenient, and the
`collection` flag added to the catalog reads `false` on every record written
before it existed — which is the right answer for all of them. The change is to
what a *new* declaration means, and it is felt when an old script is run again.

`SCHEMAFULL` is still accepted and now says what is already true. `SCHEMALESS`
became a reserved word; a field or table named `schemaless` needs renaming.

### Added

- **`DEFINE BUCKET media MAX n`** — the largest file a bucket accepts, in bytes.
  Optional, and absent means unbounded, so every bucket declared before the
  clause existed keeps its meaning. Written as a count rather than as `5MB`
  because digits touching a letter are a duration in this language whatever the
  letter is, and giving one clause a shorter spelling would be a lexical change
  everywhere. The ceiling is compared against the file **as it will be** rather
  than against the bytes a statement carries, which is the only placement that
  holds: a ranged write splices into stored bytes, so a file passes the ceiling
  while no single write is near it. There is no `HOLDS` clause narrowing a
  bucket by content type — the store has no content type for a file, so such a
  clause would enforce the caller's claim about the caller's own bytes, which is
  the assertion a bucket refuses `CREATE`, `UPDATE` and `SET` to avoid; the
  absence is recorded in the language reference §8.
- **`APPROXIMATE EFFORT n`** — a read says how many candidates the walk may keep
  in hand. Larger explores more, costs more, and finds more of the true nearest;
  the engine's own budget applies when the clause is left out. It belongs to the
  read rather than to the declaration, so a caller who needs a better answer for
  one query does not have to redeclare the store and one who needs a cheaper
  answer does not degrade everybody else's. It stands only after `APPROXIMATE`
  and is at least one, and it never reaches the index's **construction**: the
  build walks the same graph to choose a new record's neighbours, so a read's
  budget leaking there would make the index a function of whichever reads
  happened to run beside the writes, and two replicas replaying one log would
  build different graphs.

- **`DEFINE VECTOR`** — a store whose records are vectors, declared as one:
  `DEFINE VECTOR embeddings DIMENSION 768 DISTANCE cosine`, then `CREATE` into
  it and `SELECT` out of it like any other table, with `INFO FOR VECTOR` and
  `DROP VECTOR` naming it by the word that made it. It stands for exactly three
  statements — a collection, a `TYPE vector<n> REQUIRED` field called `vector`,
  and a vector index over that field — and runs them through the same functions
  the long spellings run through, so there is no second code path to disagree
  with the field one. What the word adds is that the three cannot come apart:
  a width with no index declares a shape nothing searches, an index with no
  width admits a row of the wrong shape and reports it as infinitely far from
  everything, and neither without `REQUIRED` admits a record with no vector at
  all. Both clauses are required and neither has a default. Reads are unchanged:
  `APPROXIMATE` is still the only way to obtain an approximate answer, and
  `INFO FOR VECTOR` reports measured recall or `NONE`, never a computed figure.

- **Measured recall** — `REBUILD INDEX` on a vector index now measures what
  fraction of the true nearest the index actually returns, and `INFO FOR VECTOR`
  reports it. Until then a store reports `NONE`, which is the honest answer
  rather than a gap: the figure comes from comparing the index's own walk against
  an exhaustive search over the same records, and that comparison is only free
  where the whole graph and every stored vector are already in hand — which is
  the rebuild and nowhere else. The queries are the store's own vectors, sampled
  by position in key order so two replicas replaying one log publish one number,
  and each query record is removed from both the truth and the answer, because a
  vector is always nearest to itself and counting that free hit would put a floor
  under every figure. The percentage never appears alone: it carries the `k` it
  was measured at, how many queries it averaged, the engine constants in force,
  and **how many records the store held at the time** — recall decays as records
  arrive after a rebuild, so a lone number would go on looking current forever.

- **`DEFINE GEO`** — a store of places declared by one word, standing for a
  collection, a `geometry` field the store requires, and the spatial index that
  finds them. `INFO FOR GEO` answers with the word rather than with the three, so
  a store read back out of the catalog is still a store, and `DROP GEO` removes
  it — refusing a table that is not one rather than dropping it. It takes **no
  clause narrowing the shape it holds**, unlike `DEFINE VECTOR`'s required width,
  and the asymmetry is the point: a wrong-width vector is not an error but a
  plausible ordering, while a read that orders by distance from a point already
  refuses a record that is not one and says so. Narrowing the store would also
  have made a table of regions inexpressible, and a region is a place.

- **`INFO FOR GEO` reports what refining the index costs** — how many records its
  cells offered against how many the bounding-box test kept, plus the entries
  read per record reached. A spatial index answers exactly but does not *filter*
  exactly: it filters by box, and a box is not a geometry, so a read produces
  candidates the predicate above it throws away. The ratio is the health of the
  whole arrangement, and it is the only thing that makes a structurally awkward
  row — a river, a road, a border, whose box is many times its own area —
  visible. Measured by a build from the store's own places used as queries, so
  every replica computes the same figure from one log; `REBUILD INDEX` is how a
  current one is obtained, and absence means never measured rather than a ratio
  of zero.

- **`TYPE vector<n>`** — a field declares how many components its vectors hold,
  and a write of any other width is refused **at the write**. Without it `array`
  was the only thing that could be said, which is true of a 768-wide embedding
  and says nothing: a 512-wide row sat legally beside it, and the mistake
  surfaced only where the distance functions met them — per read, long after the
  write, and not as an error, because a vector of the wrong shape is infinitely
  far from everything rather than wrong. Declared on the **field**, since a table
  may hold two vectors of different widths and each is held to its own. Both
  spellings take it — `DEFINE FIELD … TYPE` and the column list of
  `DEFINE TABLE` — the width is written out rather than bound, there is no
  width-less `vector` and no `vector<0>`, and `vector` stays a name a caller may
  use.
- **`DELETE a->edges->b`** — an edge is removed by naming the pair it joins.
  Its identity is derived from its endpoints, which is what makes `RELATE`
  idempotent, and it is never shown — so without this form the only way to
  remove an edge was to rebuild that string by hand. Works on both an edge kind
  and an edge table, takes the adjacency in both directions with it, and refuses
  a pair the edge does not join exactly as `RELATE` does rather than removing
  nothing and reporting success.
- **`DEPTH n`** — one step repeated, answering with every distinct record within
  `n`. `n` is an integer **literal** and the grammar has no position here for a
  parameter or an expression, so every walk this language can write states its
  own length. The walk is breadth-first over a set of records already seen,
  which is what makes the bound bind: without it a single cycle would let the
  work grow with `n` while the statement still looked bounded. `DEPTH` takes
  exactly one step and that step must name the table it lands on; a chain and a
  walk ending on edges are refused rather than answered one way in silence.
- **`INSERT INTO t (cols) VALUES (…), (…)`** — several records in one statement
  and one transaction, at identities the store produces, answered back in the
  order the rows were written.
- **`CREATE users = { … }` — a write that does not make you invent a name.** The
  identity's absence is the whole difference: there is no second verb and no
  flag, so `CREATE users = { … }` says the caller has a record and no name for
  it, and `CREATE users:1 = { … }` says they have both. The addressed form is
  unchanged, and `UPDATE`, `UPSERT`, `DELETE`, `SET` and reads still take an
  address — each of them points at a record that already exists, where an
  address is the honest shape.

  The generated form answers with the identity it produced rather than `done`,
  because the caller did not choose it and has no second statement that would
  find the record again. What the identity *is* comes from the table, not the
  statement: a counter from `1` upwards by default, a UUIDv7 where the table was
  declared `IDENTITY uuid`. The counter is per table and is allocated inside the
  writing transaction, so two writers cannot receive the same number — and when
  it reaches an identity a record already holds, it **walks past** it rather than
  refusing. It has to: a refusal would discard the counter's advance along with
  the transaction, so the next attempt would collide in the same place, and a
  table whose low identities were imported could never take a generated write
  again.

  `INSERT` now reads the same declaration through the same code, which is a fix
  as much as a feature: it minted a UUID into every table before this, including
  one declared to count, so two verbs could name records in one table under two
  schemes and nothing afterwards could say which scheme a missing record was
  written under.
- **`DEFINE COLLECTION`** — see above.
- **A record identity is answered in the spelling that addresses the record.**
  The protocol has always said identities are text "exactly as the store spells
  them", and that a client naming a record writes that text into its next script.
  That was true only for integers. A UUID identity was answered as thirty-two
  undivided hex digits, which this language does not read as an identity at all —
  pasting it back produced *"not a duration this store can hold"*, a refusal
  naming nothing a reader could act on. A text identity was answered unquoted,
  which is worse: `users:1` for the string `'1'` parses, addresses the **integer**
  record `1`, and returns a plausible answer about a different record. The wire,
  the console and record references inside values now all answer `1`, `'ada'`,
  `uuid '0195e0a1-…'` and `0x0a1b` — the four forms the grammar reads. The HTTP
  JSON surface is unchanged; it renders into JSON's types rather than into this
  language, and its identity field keeps the form it had.
- **`VERIFY`** — the third way to close a transaction: run every check a `COMMIT`
  runs, then discard the work. `CANCEL` could never answer *"would this be
  refused?"*, because every check that refuses a write runs inside the commit, so
  a cancelled transaction is one nothing ever disagreed with. The refusal is the
  same one in the same words because it is the same code — `VERIFY` is the commit
  with its last step, writing the batch, left out.
- **The undeclared-field refusal carries the declaration that would accept the
  write.** `table 7 declares no field nickname` became `table people declares no
  field nickname, and record people:1 carries one; declare it with `DEFINE FIELD
  nickname ON people TYPE string``. The kind comes from the value that was sent,
  and the whole statement is parsed before it is offered — a field name can
  arrive through a bound parameter rather than a script, so it is not always a
  name this language can spell, and a suggestion that would not read back is
  withheld rather than printed. It names only the field, table and value the
  caller just sent: the more useful-sounding "did you mean `salary`?" would name
  a field their grants may hide.
- **A refused batch names every row that was wrong**, not the first, so a caller
  fixing an `INSERT` sees all of it at once instead of one commit per mistake.
  One bad row is still refused exactly as it was; a batch of one is not a batch.
- **`INFO FOR TABLE` carries a `definition`** — the table, its fields and its
  indexes as TessariQL that re-creates them, rendered from the catalog at the
  moment of the read. Reading a schema and re-creating one become one operation.
  It is withheld rather than approximated: a part with no faithful spelling
  makes the whole definition absent and an `undefinable` field names the part,
  because a script that nearly re-creates a table is worse than none — it runs.
  A caller whose field grant hides part of the table gets `undefinable` too,
  since a declaration built from a narrowed list claims to re-create a table it
  would not re-create.
- **`INFO FOR TABLE` reports two things the catalog already stored.**
  `collection` says the table was declared with `DEFINE COLLECTION` rather than
  being a `SCHEMALESS` table that behaves alike, and an index now reports
  `spatial` alongside `unique` and `search`. Both were absent from the report
  while present in the store, so a collection described as a lenient table and a
  spatial index described as an ordinary one were reports that read as complete.

### Security

- **A grant covered only the tables a statement's `FROM` named.** A projection, a
  `WHERE`, an `ORDER BY`, a `GROUP BY` and a written value are expressions, and
  an expression may hold a read — so a caller granted `read` on one table could
  read any other table in the database through a subquery written in any of those
  positions, with no error raised. Every expression position is now walked, and
  a statement is refused naming the table it was not granted. If you are running
  `0.0.1-alpha` with more than one grant-governed user in a database, treat this
  as a disclosure of everything in that database to every such user.

### Added

- **A grant whose reach misses the subject's own tenancy is refused.**
  `GRANT read ON NAMESPACE staging TO nina`, where `nina` was declared
  `ON NAMESPACE prod`, previously **succeeded and did nothing**: a declared
  tenancy is asked before the held set, so the holding was stored where nothing
  would ever consult it, and the only evidence the operator had that it landed
  was the statement not complaining. The test is overlap in either direction —
  `manage ON STORE` granted to a user of `prod` stays legal, because containment
  runs downward and it is usable there. `REVOKE` is unchanged, because removing
  an inert holding stored before this rule is how it gets cleaned up.
- **`INFO FOR ACCESS TO TABLE orders` answers who reaches one table**, one row
  per user the caller administers, each saying whether that user may read it and
  whether they may write it. Every row is obtained by signing a throwaway session
  in as that user and putting a real `USE`, `SELECT` and `DELETE` to the ordinary
  authorization path — the report is the deciding check rather than a second
  opinion about it, because two opinions of one rule agree until they do not, and
  when they stop nothing fails: the report simply becomes fiction, read by the one
  person with no way to check it. It needs `govern`, and refuses rather than
  narrows, for the reason `INFO FOR USER` does.
- **A read can answer from an earlier point in the store's history.**
  `SELECT * FROM docs VERSION 4102` reads the store as it stood at that log
  sequence. Records were already versioned by a suffix on their own key, so this
  is the read the store performs anyway with a different sequence. The clause
  names a sequence rather than a timestamp because the sequence is the store's
  only ordering authority — two commits in one millisecond are ordered by it and
  by nothing else, so no timestamp could name a point between them.
- **An index no longer answers a read taken behind the committed tail.** Index
  entries carry no version and are derived at commit, so consulting one for a
  historical read drops records that have since changed and admits records that
  have since started matching — neither with an error. Every index-served read
  now falls back to the scan, and a graph traversal, which has no scan to fall
  back to, is refused.
- **Reclamation records the floor it ran at**, and a read below that floor is
  refused rather than answered from whichever versions happen to survive.

- **A field's declared type can be a union of string literals.**
  `DEFINE FIELD status ON articles TYPE 'draft' | 'published' | 'archived'`, and
  the same after a column name in the columnar spelling. `TYPE string` is true
  about a status column and says nothing; an `ASSERT` says the right thing in
  the wrong place, where a reader of the schema does not look and a reader of the
  refusal gets a condition instead of a list.

  The set is a set: members are sorted and deduplicated, so two declarations
  naming the same members are the same type however they were typed. A default
  outside the union is refused when the field is declared rather than when it
  first bites, and `INFO FOR TABLE` reports the union as the field's type.

- **A table and its fields are one statement.**
  `DEFINE TABLE people (name string REQUIRED, rank string DEFAULT 'viewer')`
  declares the table and every field in it, with the same options the long
  spelling takes — `REQUIRED`, `DEFAULT`, `ANALYZER` and `ASSERT` — and the type
  written positionally, because nothing but a type can stand after a column
  name.

  It is a desugaring rather than a second feature: each column goes through the
  same code path `DEFINE FIELD` reaches, so a constraint declared this way is
  checked against the rows already in the table, and a refusal at the third
  column rolls back the first two and the table with them.

  **Columns do not imply `SCHEMAFULL`** — of the two readings, this is the one
  the other can be written from, since strictness is one word away while a
  lenient table with declared columns would otherwise have no spelling. And
  `IF NOT EXISTS` covers the whole declaration, table and columns alike, so the
  statement can be re-run.

- **Six more of the twelve catalog objects can now be undeclared, and one says
  why it cannot.** `DROP BUCKET`, `DROP ANALYZER`, `DROP REPLICA`,
  `DROP DATABASE` and `DROP NAMESPACE` join the five drops that already existed;
  `ALTER TABLE … SET SCHEMAFULL | SCHEMALESS` joins `ALTER USER`. A store could
  previously be put into a shape no statement could get it out of, and the way
  out was editing the catalog by hand.

  **Each of these refuses while something still points at it**, counting what it
  found and naming the first, so acting on the refusal needs no second query.
  `DROP ANALYZER` refuses while a field names it — the attachment is by name, so
  nothing enforces it and a dangling one produces a search that quietly stops
  matching. `DROP DATABASE` and `DROP NAMESPACE` refuse while anything is inside.
  **There is deliberately no `CASCADE`**: a statement that removes an unbounded
  amount on the strength of one name is exactly what `DELETE … LIMIT` was added
  to prevent, under another spelling.

  `ALTER TABLE … SET SCHEMAFULL` binds what may be *written* from its commit
  onwards and does not revisit the rows already stored, which is what keeps a
  `DEFINE`-shaped statement from doing work proportional to the data.

  `DROP NODE` is **declined rather than missing**, and the refusal says so:
  `DEFINE NODE` writes this process's own configuration outside the transaction,
  so its inverse is a configuration edit — the message names that, and names
  `DROP REPLICA` as the statement that stops counting another endpoint as a peer.

- **The columnar spellings: `ALTER TABLE t ADD FIELD …`, `… ALTER FIELD …` and
  `… DROP FIELD …`.** `ADD` and `DROP` say what `DEFINE FIELD … ON t` and
  `DROP FIELD … ON t` already said, in the order somebody thinking about the
  table writes them; the declaration itself is parsed by one function, so the
  two spellings cannot drift into accepting different options.

  `ALTER FIELD` is the one that is not sugar. A second `DEFINE FIELD` is refused
  because the catalog reserves the name, so redeclaring needs its own statement —
  and it drops and declares **in one commit**, which means the stored rows answer
  for the *new* declaration through the store's own schema pass. Altering a field
  to a type its rows do not satisfy is refused outright, writing neither the
  removal nor the replacement.

- **`crypto::sha256` and `crypto::sha512`.** Two pure functions, text in and
  lowercase hex out, so that `crypto::sha256(body) = $expected` is writable — an
  array of thirty-two numbers would not be. The argument must be text:
  `crypto::sha256(type::string(x))` is the sentence for anything else, because
  hashing the canonical rendering of any value would promise that `3`, `3.0` and
  the decimal `3.0` — one value in this store — have one digest. Not for
  passwords, and there is deliberately still no function that exposes the
  credential hasher to a query.

- **`variance`, `stddev`, `median` and `collect`, and a memory ceiling that stopped
  believing every fold is cheap.** The two statistical folds reduce as they go —
  Welford's recurrence carries a count, a mean and a sum of squared deviations, so
  they cost three numbers whatever the group's size — and they are the **sample**
  forms, dividing by `n − 1`, with the population form left sayable as arithmetic
  rather than given a second name. `median` and `collect` cannot reduce: an exact
  median must see every value to find the middle, and `collect`'s answer *is* the
  collection. That distinction was not merely a cost note. A read held inside
  another statement is capped at ten thousand records unless it folds, and the
  reason written in the code was that a fold's answer "does not grow with the
  table" — true of every fold that existed, false of `collect`, so
  `SELECT collect(x) FROM huge` in an expression was the exact unbounded
  allocation the cap exists to refuse, waved through by the word *fold*. Folds now
  carry a retention classification, the cap reads it, and a `LIMIT` no longer
  lifts it for a collecting read, because a `LIMIT` bounds what a fold answers
  with rather than what it reads — the refusal names the escape that does work.
  `median` answers exactly and normalised rather than returning the middle value
  as written, so that three equal numbers of three kinds cannot give three
  different answers depending on where a sort left them.

- **`rand::uuid()` generates an identifier, and the planner learned that reading
  no record is not the same as being safe to evaluate once.** A field can now say
  `DEFAULT rand::uuid()`. The function is the small half: every expression that
  reads no record was evaluated **once above the records** and the result reused,
  which is right for the twenty-nine functions that came before and wrong for a
  generator — `SELECT rand::uuid() AS id FROM users` would have written one
  identifier into every row, with no error, no failing test, and nothing visible
  until two records that should differ did not. `Function::purity` now has a
  third answer for exactly this, the fold consults it, and `time::now()` still
  folds so that one statement observes one instant. The value is a real
  version-4 UUID with its version and variant bits set, because the type renders
  in the canonical form and something on the other side will parse it back. The
  bytes come from the operating system's randomness source, and a source that
  cannot be read is a refusal rather than a fallback to a clock or a counter.

- **Fourteen functions open an object and reshape an array** —
  `object::keys values len`, `array::distinct sort reverse flatten join slice`,
  `string::split slice replace`, `math::sqrt pow`. A path takes a literal name
  or position and no range, so none of these was sayable. One rule settles most
  of what they do: **of two candidate behaviours, the one the other can be
  written from wins.** `array::sort` is ascending and there is no `sort_desc`
  because `array::reverse(array::sort(x))` is it; `array::distinct` keeps the
  first occurrence because sorting throws away an order nothing recovers;
  `array::flatten` removes one level because two is the function written twice.
  `object::keys` and `object::values` correspond position by position, which
  nothing else in the language could establish. `array::join` reads elements by
  `type::string`'s rule rather than a rule of its own. Positions count
  characters, never bytes. `math::sqrt` refuses a negative rather than answering
  a NaN that would compare false against everything and travel silently, and
  `math::pow` keeps two whole numbers whole while refusing a result outside the
  integer range rather than saturating to the largest number in the store.
- **Eight `time::` functions read the calendar out of an instant** — `year`,
  `month`, `day`, `hour`, `minute`, `second`, `unix` and `from_unix`. A reading
  is a value like any other, so it filters, orders, groups and projects, which
  is what makes a calendar report sayable without storing the year beside the
  instant and keeping the two in step. Everything is UTC, because an instant has
  no zone. `time::second` is the second **of the minute** and `time::unix` is
  the seconds since the epoch — two questions that both answer an integer.
  `time::from_unix` refuses a fraction rather than truncating it, because
  `math::round` already says which whole second was meant; `time::unix` drops a
  sub-second remainder rather than refusing, because nothing else in the
  language could say that conversion and `time::now()` carries a remainder
  nearly always. The date arithmetic now has **one** home: a second copy is how
  a writer comes to disagree with its own reader on one day in four hundred
  years, silently.
- **Six `type::` casts** — `bool`, `int`, `float`, `string`, `datetime`, `uuid` —
  turn a value into a kind, and are named after the kinds themselves, so a cast
  and a `DEFINE FIELD … TYPE` spell the same word. **A cast is an assertion, not
  a projection**: it produces the kind it names or it refuses naming the value,
  never something near it. So `type::int('2.5')` is refused rather than
  truncated (`math::round` says which whole number was meant), `type::bool(1)`
  is refused rather than read as `true`, and `type::string([1, 2])` is refused
  rather than answered with a rendering. An **absent** argument still answers
  `none`, so a cast narrows a read over records of differing shapes; a value
  that is there and does not convert fails the read, because a filter that
  silently returned fewer rows than the question asked for is the one wrong
  answer nothing downstream can detect. `type::float` is deliberately the one
  place that rounds — `dec 19.99` has no exact float, and unlike `type::int`
  there is no other function that could say the conversion — while an integer
  past 2^53 still refuses, since there the nearest float is a *different*
  integer.

- **`LET $name = <expr>`** binds a value for the statements below it, which is
  what lets one engine's answer become the next statement's question: a vector
  read into a graph walk into a write, each still resolving to exactly one access
  path. The value is substituted forward before those statements run, so a bound
  name reaches an index exactly as a caller-supplied one does.
- **`RETURN <expr>`** names the value a script answers with. At most one per
  script.
- After `LET $x =` and after `RETURN`, a read may be written without
  parentheses.
- **`SELECT *` composes.** A `*` may stand among the values written out, so
  `SELECT *, price * quantity AS total` answers with the record **and** the
  computed column — the shape that previously meant listing every field by hand,
  and watching that list break the next time a field was added. Where both halves
  offer a name the one written out wins, which is the rule an alias already
  follows over the field it shadows.
- **`OMIT <route>`** leaves fields out of what the star contributed, and out of
  nothing else. It takes a route, so `OMIT address.postcode` keeps the address;
  it refuses a position, because removing an element renumbers the rest. Without
  it every `SELECT *` over a table holding an embedding shipped the embedding.
- **`AFTER <record>`** resumes a page from a record instead of counting past one.
  `SELECT * FROM users AFTER users:1042 LIMIT 20` answers with the twenty records
  after that one, and where the read's order is the store's own it **seeks**: the
  records before the anchor are never read, so a page at the end of a table costs
  what a page at the start costs. `START 100000 LIMIT 20` read a hundred thousand
  records to answer with twenty, and was not even correct under concurrent
  writes — an insert behind the cursor shifted every later page by one, so a walk
  to the end skipped a record for every insert and repeated one for every delete.
  The anchor is a record identity because the answer already carries it: no token
  format, no new return channel, and no version of one. A read that named an
  `ORDER BY` resumes *that* order, which it cannot seek to, so it reads the
  records and reports `cursor-walked` rather than letting a deep page get quietly
  slower. A `START` beside it is refused, and so is a `GROUP BY`, a `FETCH` or a
  `SPLIT ON`, each of which answers with something that is not a record. An
  anchor from another table is refused too: identities carry no table once they
  are compared, so a cursor pasted from the wrong page would answer with real
  records and no complaint. Without an `ORDER BY` a page walk survives the
  deletion of the record it resumed from, because the position outlives the
  record standing on it.
- **`SPLIT ON <route>`** opens an array into one record per element, each
  carrying the element where the array stood — which is what makes "each tag, and
  how many notes carry it" sayable in a document store. It is applied after
  `FETCH` and before anything that groups, projects or sorts, so an `ORDER BY`
  sorts the rows and a `LIMIT` bounds them rather than the records they came
  from, and the identity rides onto every row a record produces. An empty array
  answers with no rows, because zero elements is zero rows; an absence, a scalar
  or an object passes through once, because an array says what the elements are
  and an absence says nothing about elements at all. One route, not a list: two
  would be a cartesian product and should have to say so.
- **`SELECT … FROM ONLY <source>`** says at most one record answers the read, so
  the answer is the record rather than a list of one and a caller reading one
  thing stops unwrapping. It is an assertion the author makes — a uniqueness that
  lives in the schema and in the data, which no parse-time rule could see — so it
  is tested once the read has run: more than one is **refused**, and the refusal
  says how many, because two is a duplicate and four thousand is the wrong
  `WHERE`. None answers `NONE`, since `ONLY` says *at most* one and an absence is
  a real answer to a question about one thing. `LIMIT` is applied first. On the
  wire and in the JSON the flag is carried beside the records, so a client that
  has never heard of the clause reads exactly what it read before. `ONLY` is the
  one clause word here that is **reserved** rather than contextual: it stands
  where a table name goes, and `FROM only limit 1` cannot be told apart from the
  marker in front of a table called `limit` by any amount of lookahead.
- **A read standing in an expression holds at most ten thousand records**, and
  past that the statement is refused rather than answered from a prefix. Its
  answer is a value built whole, so an unbounded read there is an unbounded array
  inside one statement — and it is the one position where a truncating default
  could not even have reported itself, since a value has no room beside it for a
  note. The refusal names `LIMIT` as the word that lifts it. A read that folds is
  exempt: its answer does not grow with the table. A materialised source and a
  join side already had to state a `LIMIT` and are unchanged.
- **`IF <test> THEN <a> [ELSE IF <test> THEN <b>]* [ELSE <c>] END`** computes a
  value that depends on a test, in any position a value stands — a projection,
  an assignment, a filter, an ordering. Only the arm that is taken is evaluated,
  so the untaken one need not be meaningful for the record it is skipped on.
  Without an `ELSE` the answer is an absence, which is what a route into a field
  the record does not have already answers.
- **`a ?? b`** answers with the left value unless it holds nothing. This is the
  one place `NONE` and `NULL` are treated alike — the question is whether there
  is a value to use — and everywhere else they stay different questions. It
  binds tighter than a comparison and looser than arithmetic, and the right side
  is evaluated only when it is needed.

- **`AS` names a join side**, and the row files it under that name:
  `FROM users AS u JOIN orders AS o ON u.name = o.who` answers `{ u: …, o: … }`.
  Once a name is separable from a table, **a table can be joined to itself** —
  `users AS person JOIN users AS boss` — which is what aliases were needed for.
  The one-sided-join refusal moved from the table to the name, so
  `users JOIN users` and `users AS x JOIN orders AS x` are both still refused,
  and a name given where there is no join is refused rather than ignored.
- **A `FROM` may name a read**: `SELECT * FROM (SELECT … LIMIT n)`, and
  `JOIN (SELECT … LIMIT n) AS o` on either side. The answer is the inner records
  themselves, so the outer statement reads them as it would read a table. The
  inner read **must state a `LIMIT`** — a materialised source holds every record
  it answers with, so one that could grow without limit is refused rather than
  truncated at a number nobody wrote. A joined read must also name itself with
  `AS`, having no name of its own.
- **A `WHERE` after a materialised read** asks about what that read *produced*:
  `FROM (SELECT who, count(*) AS n … GROUP BY who LIMIT n) WHERE n > 1`. `WHERE`
  belongs to the table position, so a grouped read and a traversal had nowhere to
  put one; wrapping either gives it one, and the condition can name a value no
  condition inside the read could have.

- **`UPSERT t:1 = { … }`**, and `SET` and `MERGE` after it, write the record
  whether or not it is already there. A third verb rather than a flag, because
  the three differ in what they assert beforehand — `CREATE` says the record is
  absent, `UPDATE` says it is present, `UPSERT` says neither — and keeping the
  first two is what makes the third safe to add. Over an absent record it starts
  from an empty object, so the edit shapes need no special case.
- **`UPDATE t:1 MERGE { … }`** folds an object into the record and leaves what it
  does not name. Deep where both sides hold an object; the incoming value whole
  everywhere else, so an array replaces an array. An explicit `NULL` is written —
  removing a field stays `SET route = NONE`. The object stands in the value
  position like every other object literal, so a bare name inside it is a table
  rather than a route into the record: `MERGE $patch` is the shape this is for,
  and computing from the record remains `SET`'s job.
- **`THROW <expr>`** refuses the script, with a message the caller sees. `IF`
  made a decision computable and this makes it enforceable: a rule the store does
  not itself check — "this order is already paid" — could be detected in the
  language and not acted on. A refusal inside a transaction discards the work
  above it, which is what makes a guard clause worth writing rather than a
  comment.
- **`RETURN BEFORE` / `RETURN AFTER`** on `CREATE`, `UPDATE`, `UPSERT` and
  `DELETE` — the write answers with the record it produced or the one it
  replaced. What this removes is the second statement: reading back what was
  just written cost a round trip to learn a value the store had in hand. Absent
  by default, so a write that does not ask still answers `done`.

  The two pairings that could only ever answer `NONE` are **refused** rather than
  answered: there is no record before a `CREATE` and none after a `DELETE`.
  `RETURN DIFF` is not built and is refused too, rather than accepted and
  ignored — what a diff of an array should look like is a design question, not a
  missing line.

- **A read now says what it did, on the answer, without being asked.** An
  outcome carrying records carries **notes** beside them — `fell-back` when an
  index held the order and could not fill the bound, `approximate` when the
  answer is the best a graph found rather than provably the best there is, and
  `subquery-ceiling` when a materialised source reached the `LIMIT` it stated and
  the outer statement therefore asked its question of a prefix.

  A note never changes what a statement answers: a caller that ignores every note
  gets exactly the records it would have got before notes existed. `EXPLAIN`
  answers a question you have to know to ask; a note is the store volunteering
  the one thing about this answer you would have wanted to know.

  `fell-back` fires on an index that **declined**, never on a table that has
  none — a bounded ordered read over an unindexed table gave nothing up, and a
  note on it would fire so often that nobody would read the ones that matter.

  Notes reach `Outcome::notes()` and the HTTP body, where the `notes` key is
  absent when there is nothing to say. They do not cross the binary protocol yet:
  a decoder there checks it consumed every byte of a body, so a field appended to
  one is not something an older client ignores, and carrying notes needs the
  protocol's minor version to gate them.

- **`EXPLAIN` and an answer report one plan structure.** Until now there were two
  descriptions of the same read speaking different words: a traversal explained
  as `graph` and answered `index`, a join explained as `join` and answered
  `index` or `scan` depending on which side got probed, a materialised source
  explained as `materialised` and answered whatever the *inner* read had done —
  which claimed an index the outer statement never touched. Neither report was
  wrong; together they made the plan unusable, because comparing what a statement
  said it would do against what it did meant translating between two
  vocabularies.

  There is now one `Plan` type, one renderer, and one function filling the fields
  that describe a chosen index — called by the read that runs the choice and by
  the `EXPLAIN` that only describes it. `AccessPath` gains `Approximate`,
  `Graph`, `Join` and `Materialised` so both sides say the same word.

  The two agree everywhere but one case, and that case is why both are reported.
  Whether a descending ordered index will fill the statement's bound is the read
  itself, so `EXPLAIN` reports the order it chose while a read whose index ran
  out reports the scan it settled for — and the answer carries the `fell-back`
  note naming both.

  The plan reaches `Outcome::plan()` and the HTTP body as a `plan` object beside
  the `path` word. The binary protocol still carries the access path alone; an
  older client reads an unknown path tag as the scan, which is the one path that
  promises nothing, so the widened vocabulary degrades rather than breaking.

- **`USING <path>` and `USING INDEX <name>`** let a read state which path it
  expects and be refused when it took another. A refusal, never a router: it does
  not choose a path and cannot make a read faster. It exists because the worst
  failure mode an indexed store has is the query that quietly stops using its
  index and starts scanning — the answer stays correct and the only symptom is a
  latency graph somebody has to be watching.

  Checked against what the read **did**, not what the planner chose. A descending
  ordered index that cannot fill the bound hands the read to the scan, and an
  assertion satisfied by the planner's intention would pass in exactly the case
  it was written to catch — so `USING ordered` is refused there and `USING scan`
  is permitted. The cost of a refused statement is the read it already did, which
  follows from the same rule.

  `USING INDEX` asks what the path word cannot: `index` says *an* index answered,
  `USING INDEX by_city` says which. An unrecognised word is refused before the
  read runs, naming the words that exist. An assertion inside a materialised
  source is about the inner read.

- **Notes now travel over the binary protocol.** A `Records` answer carries the
  kind and message of each note, so a client on the wire sees what the embedded
  and HTTP surfaces already showed. The `tessaridb` shell prints them under the
  answer.

  They were held back on the belief that the decoder asserted it had consumed
  every byte, which would have made an appended field indistinguishable from
  trailing garbage to an older client. That was wrong: an outcome is
  length-prefixed and the reader advances by the declared length, and the
  published protocol has always required a client to skip bytes it has no field
  for. No version gate was needed. The other direction — a newer client reading
  an older node — is the one that needed code: a body that ends after the records
  is a node with nothing to say, not a truncation.

- **A fourth note, `compared-across-kinds`.** A schemaless store lets one record
  hold `age: 30` and the next `age: '30'`, and `WHERE age = 30` then matches some
  of them — correctly, and narrower than the author meant, with no error
  anywhere. The read now says so.

  It never fires on an **absence**: a record without the field compares `none`,
  which is how a schemaless read narrows rather than fails, and a note there
  would fire on nearly every read in the language. The note names a pair of kinds
  **once** however many records produced it, and reads the same way whichever
  side of the `=` each half was written on.

- **`TIMEOUT <duration>`** puts a wall-clock ceiling on a read, and a read that
  passes it is **refused, not truncated**. When the ceiling passes the records
  are already in hand, so returning them costs nothing and looks like success —
  and a caller counting, summing or storing a partial answer would be wrong with
  no way to find out. The refusal carries how far the read got, which is the one
  thing a shortened answer would have told the caller, in a place it cannot be
  mistaken for the result.

  The ceiling is spent once per record, as the read produces it. That bounds
  where a long read spends its time — decoding, testing, projecting, sorting —
  and it is stated rather than implied: it does not interrupt a single storage
  call, and it does not reach a read standing in an expression, which has no
  channel to carry a budget into.

  A subquery's ceiling narrows and never widens: whichever of the inner and outer
  budgets expires first refuses, because an inner clause able to raise its
  caller's budget would make the outer ceiling a suggestion. `TIMEOUT 0s` and any
  negative span are refused when the statement is read, since neither names a
  budget a statement could satisfy. The word stays unreserved — an index may
  still be called `timeout`, and which reading is meant is settled by whether a
  duration follows.

### Fixed

- **Dropping a bucket orphaned the table its bytes lived in.** `DEFINE BUCKET`
  creates a companion chunk table whose name carries a byte no identifier can
  hold, so nothing could ever drop it by naming it — and `DROP TABLE` on the
  bucket left it behind permanently, after which redefining the bucket failed
  with the chunk table's name already in use. Found by the new corpus, which
  redefines a bucket it has just dropped.

### Changed — breaking

- **`Outcome::Records` carries its plan instead of a bare `AccessPath`.** The
  `path` field is now `plan: Plan`; `Outcome::path()` still answers the access
  path alone and is unaffected, and `Outcome::plan()` is new. Embedded callers
  that matched the variant by naming `path` read `plan.access` instead.

- **A traversal, a join and a materialised source report new access paths.** They
  answered `index` or `scan` before and now answer `graph`, `join` and
  `materialised` — the words `EXPLAIN` already used for them. A join no longer
  reports whether its right side was probed or scanned: neither side's path is
  how the joined answer was reached.

- **`Outcome::Records` carries a third field, `notes`.** Embedded callers that
  match it by naming both fields need `..`; callers that read `records()` and
  `path()` are unaffected. `Outcome` itself was already `#[non_exhaustive]`; the
  variant is deliberately not sealed, because the wire crate constructs one when
  it decodes a response.

- **A conditional delete must now state how much it may remove.**
  `DELETE FROM t WHERE …` takes either `LIMIT n` or `LIMIT ALL`, and a statement
  carrying neither is refused before it runs. Previously it accepted no bound at
  all, so a predicate wrong by one character emptied the table with nothing
  between the parser and the store.

  One of the two `LIMIT`s in the language that are not optional; the other bounds
  a materialised source. The asymmetry against an ordinary read is the point: a read that omits a bound answers with more rows than the caller
  expected, and a delete that omits one destroys data. `LIMIT ALL` costs one word
  and is how a retention policy says the whole matched set is what it meant.

  `LIMIT n` bounds **what is removed**, not what is examined — the condition
  decides first, so the statement means the same thing whichever index answered
  it.

  **To upgrade:** append `LIMIT ALL` to every existing `DELETE FROM … WHERE …`
  to keep its current behaviour, or a numeric bound where one is wanted. Nothing
  else changes; the single-record `DELETE t:1` form is untouched.

- **A join whose two sides hold different kinds of value at the key is now
  refused** instead of answering with no rows. Equality across two kinds is
  false, so such a join could only ever be empty — and empty is also the honest
  answer to a join over data that does not match. The two answers were identical
  and only one of them was a mistake: an identity stored as text on one side and
  as a reference on the other, which is the commonest way a join is written
  wrong and was invisible in the result.

  The refusal fires only when the answer is empty, both sides held something at
  the key, and their kinds share nothing. A join that produced any row is never
  refused, and neither is one over a table whose records have not arrived yet.

  **To upgrade:** nothing to change in a join that answers. A join that was
  answering `[]` because of a type mistake now says so; fix the data or the key
  it names.

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

476 conformance cases define the language and run in the build.

### What is not here

Stated as plainly as the list above, because an alpha that is vague about its
absences is worse than one that is missing more.

- **The nearest few is over positions, and the spatial index is untuned.**
  `DEFINE INDEX … SPATIAL` is written, maintained and now **read**: seven of the
  eight predicates are served by covering the query shape with cells, reading the
  entries under and above them, and rejecting what the stored bounding boxes
  settle before the exact predicate runs. `geo::disjoint` is the complement of a
  region, has no sound box filter, and stays an exact scan by design.
  The same index answers `ORDER BY geo::distance(at, …) LIMIT k` by walking
  cells cheapest-first, keyed by a distance nothing inside the cell can beat —
  exact rather than approximate, and therefore asking nothing of the statement.
  What is absent: a distance to a shape larger than a position, which is why
  that read is over positions; a nearest-first read under a `WHERE`;
  and any *measured* choice of how finely a query is covered — the budget is a declared constant,
  and the candidate-to-result ratio the store now measures is what will move it.
  <!-- absent: distance-between-two-larger-shapes -->
  <!-- absent: nearest-first-under-a-where -->
  <!-- absent: measured-covering-budget -->
- **No sharding, no replication, no cluster membership.** Peers can be declared
  and read back; nothing replicates between them. The language has words for
  these; the engine does not have the machinery.
- **Not published to crates.io.** Every crate carries `publish = false`. Build it
  from source.
  <!-- absent: published-to-crates-io -->
- **No migration between versions**, as above.
  <!-- absent: migration-between-versions -->

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
