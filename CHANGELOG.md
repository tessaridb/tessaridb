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

Unreleased. 860 conformance cases define the language and run in the build.

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
