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

## 4. Definition statements

```
DEFINE NAMESPACE prod;
DEFINE DATABASE orders;
DEFINE TABLE users;
DEFINE SPACE sessions;
DEFINE INDEX by_email ON users FIELDS email UNIQUE;

DROP INDEX by_email ON users;
DROP TABLE users;
```

Each writes one catalog entry, and the catalog is records (`docs/key-grammar.md`
§9), so a definition takes part in the transaction that issued it: a script may
define a table and write to it, and either both land or neither does.

`DEFINE ... IF NOT EXISTS` is accepted and is not the same as re-running the
statement: a name is unique within its parent, and a plain `DEFINE` over an
existing name is refused.

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

`SELECT` resolves to exactly one of the three access paths, and which one is
decided by the target rather than by a cost model:

| Form | Path |
|---|---|
| `FROM users:1` | the record by its identity |
| `FROM users WHERE <indexed field> = <value>` | the index |
| `FROM users` | every record of the table |

A `WHERE` over a field with no index is refused at this milestone rather than
silently executed as a scan-and-filter. A statement whose cost is a table scan
should say so, and `FROM users` already does.

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
- `KEYS` lists keys, optionally over a range. **A range works** because keys are
  record ids and record ids are order-encoded (`docs/key-grammar.md` §5) — the
  space is scanned, not filtered.

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
| full-text and vector search | same — reserved kinds, no engine yet |
| aggregation and grouping | needs an execution layer this milestone has not built |
| `WHERE` over an unindexed field | refused rather than silently a scan-and-filter |
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
