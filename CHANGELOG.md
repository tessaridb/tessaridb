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

## 0.0.2-alpha — 2026-08-27

Unreleased. 589 conformance cases define the language and run in the build.

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
