# bgvQL — the milestone-1 subset

Normative for what this milestone accepts. bgvQL is the only way structure comes
into existence in this store: there is no schema file, no migration DSL and no
side door in the API. A namespace, a database, a table, a space and an index all
begin as a statement.

This document specifies a **subset**, and the boundary is drawn by one rule:

> The subset is exactly what the store underneath can execute today.

The store offers three access paths — a record by its identity, every record of a
table, and the records an index points at — and the statements below reduce to
those three. Nothing here implies a planner with a choice to make, because at
this milestone it does not have one. That is a scoping decision, not a claim
about the language's eventual shape.

`docs/value-system.md` and `docs/key-grammar.md` are its companions: the first
says what a value is, the second what the bytes are. Neither is restated here.

---

## 1. Shape

A script is a sequence of statements separated by `;`. A statement is one verb
and its arguments. Statements are case-insensitive in their keywords and
case-sensitive in every name.

There are four families, and the split is deliberate:

| Family | Verbs | What it touches |
|---|---|---|
| session | `USE` | which namespace and database the rest of the script means |
| definition | `DEFINE`, `DROP` | the catalog |
| data | `CREATE`, `SELECT`, `UPDATE`, `DELETE`, `GET`, `SET`, `DEL`, `KEYS` | records |
| transaction | `BEGIN`, `COMMIT`, `CANCEL` | the unit of work |

## 2. Session context

```
USE NAMESPACE prod;
USE DATABASE orders;
USE NAMESPACE prod DATABASE orders;
```

Every later statement resolves unqualified names inside that context. A statement
may qualify a name explicitly (`orders.users`), and an explicit qualification
always wins over the session.

A data statement issued with no database selected is an error, not a default.
Guessing which database a write belongs to is the one mistake that cannot be
undone by reading the result.

## 3. Literals

Literals map onto the fifteen types of the value system, and nothing else is
literal syntax.

| Literal | Type |
|---|---|
| `NONE` | `none` — the field is not there |
| `NULL` | `null` — it is there and holds nothing |
| `true`, `false` | `bool` |
| `42`, `-7` | `number`, integer |
| `1.5`, `1e10` | `number`, float |
| `dec 12.34` | `number`, exact decimal — the prefix is what keeps it exact |
| `'text'`, `"text"` | `string` |
| `0x0a1b` | `bytes` |
| `2s`, `1h30m`, `-500ms` | `duration` |
| `datetime '1970-01-01T00:00:00Z'` | `datetime` |
| `uuid '…'` | `uuid` |
| `users` (in a type position) | `table` |
| `users:1`, `users:'ada'` | `record` |
| `[a, b]` | `array` |
| `{ name: 'ada' }` | `object` |
| `1..10`, `1..=10` | `range` |
| `set [a, b]` | `set` |

Three things this table is saying on purpose:

- **`NONE` and `NULL` are different literals** because they are different values.
  A language that spelled them the same would erase a distinction the storage
  layer goes out of its way to keep.
- **A decimal is written with a marker.** `12.34` is a float; `dec 12.34` is
  exact. Money written without the marker is a float, and that is the mistake
  worth making loud rather than convenient.
- **A record literal is `table:id`**, which is also how a record id is written
  everywhere else in the language.

Two lexical rules are stated here because their opposites fail quietly. A `.`
begins a fraction only when a digit follows it, so `1..10` is a range and not the
float `1.` beside `.10`. And digits touching a letter are a duration, whatever
the letter is — `5y` is a duration with an unknown unit and is refused, rather
than the number five beside a name that fails somewhere else.

`SET` serves both the key-value verb and the set literal. Which one is meant is
decided by whether a `[` follows, and nothing else in the grammar makes that
ambiguous.

**A field name inside an object may be a reserved word**, so `{ unique: true,
where: 'here' }` is written the obvious way. A field name is always followed by
`:` and can never be a verb in that position, so nothing about the grammar
depends on context. A name that is not a word at all is written as text:
`{ 'two words': 1 }`. Everywhere else — tables, spaces, indexes, databases — a
reserved word is not available as a name.

## 4. Definition statements

```
DEFINE NAMESPACE prod;
DEFINE DATABASE orders;
DEFINE TABLE users;
DEFINE SPACE sessions;
DEFINE INDEX by_email ON users FIELDS email UNIQUE;
DEFINE INDEX by_name ON users FIELDS last, first;

DROP INDEX by_email ON users;
DROP TABLE users;
DROP SPACE sessions;
```

An index names one field or several, in order; without `UNIQUE` two records may
share an entry. `DROP SPACE` and `DROP TABLE` do the same thing — a space is a
table (ADR-0010) — and both spellings exist so that a script reads the way its
author thinks about what it removes.

Each writes one catalog entry, and the catalog is records (`docs/key-grammar.md`
§9), so a definition takes part in the transaction that issued it: a script may
define a table and write to it, and either both land or neither does.

`IF NOT EXISTS` is written **before the name** — `DEFINE TABLE IF NOT EXISTS
users` — which is where the same clause sits in SQL and therefore where it will
be typed. It is not the same as re-running the statement: a name is unique
within its parent, and a plain `DEFINE` over an existing name is refused.

Two behaviours of `DEFINE INDEX` are stated here because they are surprising and
because neither raises an error:

- An index declared on a table that already holds rows **indexes none of them**
  until it is backfilled. Maintenance sees writes, and rows that predate the
  index are not writes.
- A `UNIQUE` index therefore **does not constrain those rows** either, until the
  backfill succeeds. A unique index declared on populated data and left
  un-backfilled is declared and not enforced.

`DROP TABLE` removes the definition. It does not delete the table's records,
because that is bulk work whose cost belongs where a caller can see it.

## 5. Record statements

```
CREATE users:1 = { name: 'ada', email: 'ada@example.com' };

SELECT * FROM users:1;
SELECT * FROM users;
SELECT * FROM users WHERE email = 'ada@example.com';

UPDATE users:1 = { name: 'ada', email: 'ada2@example.com' };
DELETE users:1;
```

`CREATE` and `UPDATE` are not two spellings of one verb. **`CREATE` over a
record that already exists is refused**, and **`UPDATE` over one that does not
exist is refused**. The alternative — either verb quietly doing the other's job —
loses a record with nothing anywhere to notice, and `SET` already exists for the
caller who means "whatever is there, replace it".

`SELECT` resolves to exactly one of the three access paths, and which one is
decided by the target rather than by a cost model:

| Form | Path |
|---|---|
| `FROM users:1` | the record by its identity |
| `FROM users WHERE <indexed field> = <value>` | the index |
| `FROM users WHERE <field> = <value>` (no index) | the table, testing each record |
| `FROM users WHERE <field> LIKE <pattern>` | the table, testing each record |
| `FROM users` | every record of the table |

A `WHERE` names a field and a test:

```
SELECT * FROM users WHERE email = 'ada@example.com';
SELECT * FROM notes WHERE body LIKE '%lovelace%';
SELECT * FROM notes WHERE body ILIKE 'ada%';
```

`=` is exact equality on the whole value. `LIKE` is SQL's pattern match, and it
is spelled the way SQL spells it because that is what a person or a tool writes
without thinking: the pattern covers the **whole** value, `%` stands for any run
of characters, `_` for exactly one, and `\` escapes either. That whole-value
anchoring is why a substring search is written `'%text%'`. `ILIKE` is the same
test ignoring case.

Only text satisfies a text pattern — a number in that field is not an error, the
record simply does not match.

Deliberately nothing cleverer: no tokenising, no stemming, no ranking. Those
belong to an analyzer, and a scan-shaped approximation of one now would give
answers that a real text index later disagrees with. A query whose answer changes
when an index is added is worse than a slow one.

**Which access path runs is decided by what exists, not by how the query is
written.** An equality on an indexed field is an index read; everything else
reads the table and tests each record. The statement is identical either way,
so adding an index later makes existing queries faster without rewriting any of
them. The path taken is reported with the result, so a scan is visible rather
than folklore.

**With one exception, and it is sharp.** An index declared on a table that
already holds rows indexes none of them until it is backfilled (§4). Until then
the same statement a scan answered correctly is answered by an index that knows
nothing about those rows — and it returns *fewer* records, with no error. On
populated data, define the index and backfill it before relying on it, or the
change of access path is also a change of answer.

**An index read is confirmed, not trusted.** Every candidate the index offers is
re-checked against the reading transaction's own snapshot, so a stale entry can
never produce a row that does not match. At an older snapshot the result is
therefore correct and possibly short: a record that held the value then and has
changed since has no entry left to find. Reading at the newest committed state is
exact.

## 6. Key-value statements

A space is a table whose records hold a single value rather than an object, a key
is a record id, and a value is any value (ADR-0010). The verbs are separate from
the record verbs because the access shape genuinely differs — there are no fields
to project and no index to choose — and pretending otherwise would make one of
the two models awkward.

```
SET sessions:'abc' = { user: users:1, expires: datetime '2026-09-01T00:00:00Z' };
GET sessions:'abc';
DEL sessions:'abc';
KEYS FROM sessions;
KEYS FROM sessions RANGE 'a'..'m';
```

- `SET` writes; the whole value is replaced, never merged.
- `GET` returns the value, or `NONE` when the key is not there. `NONE` and a
  stored `NULL` are different answers, which is the point of having both.
- `DEL` removes the key.
- `KEYS` lists keys, optionally over a range. A range is meaningful because keys
  are record ids and record ids are order-encoded (`docs/key-grammar.md` §5), so
  the bound names a contiguous stretch of the space rather than an arbitrary
  subset. At this milestone the bound is applied to a scan of the space rather
  than seeked to: the answer is the same, the cost is not, and saying so here is
  cheaper than a reader discovering it under load.

### Composition

A key-value read is an **expression**. It may appear anywhere an expression may,
and it returns a `Value` — which is what every other model stores, so nothing
around it needs to know which model produced it.

```
SELECT * FROM users WHERE email = GET emails:'primary';

CREATE audit:1 = {
  actor:  GET sessions:'abc',
  target: (SELECT * FROM users:1),
};
```

Two rules make the composition mean something:

- **One snapshot.** A key-value read inside a record query reads at the same
  snapshot as the query around it. Two models that cannot share a snapshot are
  two databases sharing a process.
- **One transaction.** A script may write a space and a table and commit both, or
  neither, because both live under one database and a database is the unit a
  transaction may not leave (ADR-0008 §4).

A space is not indexed by value. A record whose payload is not an object projects
to no fields, so an index over a space holds nothing. Searching a space by its
values is a separate feature and is not claimed here.

## 7. Transactions

```
BEGIN;
  CREATE users:1 = { name: 'ada' };
  SET sessions:'abc' = users:1;
COMMIT;
```

`CANCEL` discards. A statement outside `BEGIN` is its own transaction.

A script that opens a transaction and never closes it **discards the work and
raises an error**. Committing it would commit work the author never said was
finished; discarding it quietly would hide that the script ran at all.

The isolation level is **snapshot isolation**, and its two permitted anomalies
are part of the contract rather than defects: write skew, and phantoms. Both are
described in `docs/storage-contract.md` and demonstrated by the store's own
tests. A caller who needs an invariant that write skew could break materialises
it into a value both transactions write — which is exactly how the catalog keeps
a name unique.

A conflict is reported, never retried silently. The losing transaction wrote
nothing, and re-running it needs a fresh read, not a repeat.

## 8. What is deliberately absent from this milestone

Named here rather than merely missing, so each absence reads as a decision:

| Absent | Why |
|---|---|
| joins | no planner, and a join without one is a nested scan pretending otherwise |
| graph traversal | the edge key kind is reserved and unimplemented |
| a text **index** | `LIKE` works today over a scan; the index that makes it fast, with its analyzer, is its own milestone. The statement will not change when it lands. |
| vector search | a reserved key kind, no engine yet |
| ranking, highlighting, stemming, fuzzy matching | analyzer decisions; approximating them over a scan now would disagree with the index later |
| aggregation and grouping | needs an execution layer this milestone has not built |
| functions and expressions beyond literals and reads | a language surface to design once, not accrete |
| permissions in the language | there is no session identity yet |
| schema on a table | tables are schemaless at this milestone; `DEFINE INDEX` is the only per-field statement |

## 9. What is fixed here, and what can still move

| Decision | Status |
|---|---|
| bgvQL is the only way structure is created | **contract** (ADR-0003) |
| A space is a table of single values; workspace = database | **contract** (ADR-0010) |
| Key-value verbs are expressions, and compose at one snapshot | **contract** |
| `NONE` and `NULL` are distinct literals | **contract** — the storage layer keeps them apart |
| The decimal marker | fixed; dropping it would silently make money a float |
| The three access paths | fixed for this milestone; a planner changes how one is chosen, not what exists |
| Verb spellings and clause names | movable while the language is unimplemented |
| Everything in §8 | additive |
