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
  <!-- absent: distance-to-a-shape-larger-than-a-position -->
  <!-- absent: nearest-first-under-a-where -->
  <!-- absent: measured-covering-budget -->
- **No sharding, no replication, no cluster membership.** Peers can be declared
  and read back; nothing replicates between them. The language has words for
  these; the engine does not have the machinery.
  <!-- absent: sharding-replication-cluster-membership -->
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
