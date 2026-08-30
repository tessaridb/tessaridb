# TessariQL — the milestone-1 subset

Normative for what this milestone accepts. TessariQL is the only way structure comes
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
| `geometry { type: 'Point', coordinates: [2.35, 48.85] }` | `geometry` |

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

### Supplying a value from outside the script

A caller who has a value writes `$name` and supplies the value alongside the
script:

```
SELECT * FROM users WHERE name = $who;
CREATE users:3 = { name: $name, age: $age };
```

**A parameter is legal exactly where a literal is, and nowhere a name is.** It
may stand in a condition, in a projection, inside an object, an array, a set, a
range, a function argument or an arithmetic operand. It may not stand where a
table, a field, an index, a namespace, a user or a role is named. That is one
rule rather than a list of positions, and it is what keeps a caller who can
supply a value from thereby choosing which column is read.

A record is `table:id`, and the two halves fall on either side of that line: the
table is a name and the **id is a value**, so a parameter stands there.

```
GET sessions:$token;
READ media:$path;
```

A supplied identity must be something a record can be identified by — an integer,
text, a uuid or bytes. A float or an object is refused where it is supplied
rather than converted into text, which would quietly make `1.0` and `'1.0'` the
same record.

Two consequences follow from *when* a parameter is replaced, which is after the
script is parsed and before its first statement runs:

- **A supplied value cannot become syntax.** There is no stage left at which it
  could be read as grammar, whatever it holds. This is a property of the order
  the work happens in, not a claim about quoting.
- **A script either binds completely or does nothing.** A parameter with no
  value refuses the script, naming the parameter, before anything is written —
  so an unsupplied name in the last statement of a script does not leave the
  first one applied.

A value supplied under a name the script does not use is accepted; a caller who
reuses one set of values across two scripts has not made a mistake.

Every way in carries them. The embedded session takes a map, the wire protocol
carries the values in the store's own codec — so all fifteen kinds cross
unchanged and the server never has to *read* one — and the console takes
`--param <name>=<value>` with the value written as TessariQL, repeatably. The console
spells it in TessariQL rather than JSON because what the console prints already
pastes back into the next statement, and the value is parsed **in isolation**, so
`--param x='1; DROP TABLE users'` is refused as a value rather than smuggled in
as a statement.

Values are supplied as values and not as text, so `$age` bound to the number
`36` and `$age` bound to the string `'36'` ask different questions — the caller
never has to know how this language would have read a piece of text.

An index still serves a read whose value came in this way. Replacement happens
before anything plans the read, so `WHERE name = $who` is planned exactly as
`WHERE name = 'ada'` is.

### Naming a value the script itself produced

A caller's value comes from outside. `LET` names one the script worked out:

```
LET $city = 'london';
SELECT * FROM users WHERE city = $city;
```

Which on its own is a convenience. What it is actually for is this:

```
LET $nearest = SELECT id FROM notes ORDER BY vector::cosine(embedding, $q) LIMIT 5 APPROXIMATE;
LET $authors = SELECT * FROM users WHERE id IN $nearest;
RETURN $authors;
```

Three statements, three different engines — a vector index, an ordinary index,
and whatever the third read needs — and each one still resolves to exactly **one**
access path chosen by its own shape. That is the point. A read in this language
picks one path, which is what keeps its cost legible; a question crossing two
engines was therefore unsayable, not because the engines could not answer it but
because nothing carried the first answer into the second statement. A binding
carries it, and costs no planner.

**A bound name behaves exactly as a supplied one does.** Everything the section
above says about `$who` is true of `$city` — legal where a literal is legal and
nowhere a name is, standing for the id half of an identity and never the table
half, and reaching an index:

```
DEFINE INDEX by_city ON users FIELDS city;
LET $city = 'london';
SELECT * FROM users WHERE city = $city;    -- served by the index
```

That is not a coincidence, it is the same machinery. When a `LET` runs, its value
is **substituted into the statements that have not run yet** — the identical walk
a caller's map takes, for the identical reason. By the time any statement
executes, every name in it is a literal, so nothing downstream can tell which of
the two kinds of binding happened, and the planner still finds a right-hand side
where it looks for one.

Four rules follow, and each is refused where it is written rather than discovered
when something runs:

- **A name is bound once in a script.** `LET $x` twice is refused. With
  substitution, two bindings would make `$x` mean one value in part of the script
  and another further down, and a reader would have to count statements to know
  which.
- **A caller and a script may not name the same thing.** There is no reading of
  that which is not surprising: one of the two values is silently discarded.
- **A name must be bound above where it is used.** `$x` written before its own
  `LET` is refused before the first statement runs, which is what keeps the
  binds-completely-or-does-nothing property the section above states.
- **A binding is only as privileged as what it holds.** `LET $x = SELECT …`
  reaches every table that read reaches, and is refused if the caller may not
  read one of them.

`RETURN` names the value the script answers with:

```
LET $paid = SELECT count(*) AS n FROM orders WHERE status = 'paid';
LET $all  = SELECT count(*) AS n FROM orders;
RETURN { paid: $paid, total: $all };
```

A script holds **at most one**, refused where there are two — a second would make
"the answer" depend on which one ran. It does not end the script: ending it would
make everything below unreachable, and unreachable statements inside a
`BEGIN`/`COMMIT` would leave the transaction open.

After `LET $x =` and after `RETURN`, a read may be written **without**
parentheses, because it runs to the end of the statement and there is nothing for
a parenthesis to disambiguate. Inside a larger expression — `IN (SELECT …)` — the
parentheses say where the read stops, and are required.

**A read standing in an expression holds at most ten thousand records.** Its
answer is a value, and a value is built whole, so a read here with no bound of
its own is an unbounded array inside one statement:

```
LET $some = SELECT id FROM events LIMIT 1000;
```

Past the ceiling the statement is **refused**, and the refusal names `LIMIT` as
the word that lifts it. It is refused rather than cut at a number nobody wrote,
for the reason the required `LIMIT` on a materialised source gives: unbounded,
that read is expensive and right, while a silent prefix of it is cheap and wrong,
and here there is not even a note to say so — a value has no room beside it.

A read that **folds** is usually exempt, because its answer does not grow with
the table: `LET $n = SELECT count(*) AS n FROM events;` needs no bound, and
`LIMIT` would not have been one, since over a grouped read it bounds the groups
answered rather than the records read.

**Two folds are not exempt**, and they are the two whose answers *do* grow with
the table: `median` has to see every value to find the middle, and `collect`'s
answer is the collection. A held read using either gets the ceiling like any
other, and — for the same reason the exemption existed — a `LIMIT` on that read
will not lift it. Bound what it reads instead:

```
LET $some = SELECT collect(id) AS every FROM (SELECT id FROM events LIMIT 1000);
```

### Naming a value inside a record

A record payload is a tree: an object may hold objects and arrays, and those may
hold more, with no depth limit. A **path** is how something inside it is named.

Every fenced example in this document is executable TessariQL, so a path is shown in
one of the two positions it is read in:

```
SELECT * FROM people WHERE address.city = 'Paris';
SELECT * FROM people WHERE tags[0] = 'urgent';
SELECT * FROM people WHERE history[2].by.name = 'ada';
DEFINE INDEX by_home_city ON people FIELDS address.city;
```

A path is a field name followed by any number of steps: `.name` goes into an
object, `[n]` goes to a position in an array, counting from zero. Paths are read
in exactly two places — the left side of a `WHERE` (§5) and the projection of a
`DEFINE INDEX` (§4) — because those are where a *value inside a record* is meant.
They are not read where a table may stand, so `orders.users` keeps meaning the
table `users` in the database `orders`.

**A path that reaches nothing is not an error.** A missing field, a missing
field below one that exists, an object addressed by position, an array addressed
by name, a position past the end: every one of them means the filter does not
match and the index does not index. That is the rule a missing top-level field
has always followed, carried one level down, and it is what lets records of
different shapes share a table — which is the reason to hold documents at all.

A position is a whole number. Counting from the end would need a sign whose
meaning depends on the array's length, which is a decision about what a path
*means* rather than how one is written.

**`[*]` is every element rather than one of them**, and it changes what a path
*is*: a route with a position denotes a value, and a route with `[*]` denotes
**the values it reaches** — of which there may be none, one or many.

```
SELECT * FROM people WHERE tags[*] = 'urgent';
SELECT * FROM orders WHERE items[*].sku = 'b2';
```

What a context does with several values is the **context's** rule, and there are
three:

| Context | Rule |
|---|---|
| a comparison | holds when **any** of the reached values satisfies it |
| a projection | answers with **all** of them |
| an index | keeps **one entry per** element — a multikey index, §4 |

```
SELECT tags[*] AS all_tags FROM people;
SELECT items[*].sku AS skus FROM orders;
```

A projection **collects**: the values arrive in route order with duplicates kept,
because a projection reports what is there and deduplicating or sorting would be
a different statement. It needs `AS`, since a route ending in `[*]` has no name of
its own — every invented spelling is a convention learned from a surprise.

**Every record answers, and a record that reaches nothing answers `[]`.** A
relation is total: each record has a reach, and zero of them is an empty
collection rather than an absence. So an empty array, an absent field and a
single value all answer the same `[]` — they have the same reach, and a
projection that told them apart would be reading whether the field exists, which
`tags` on its own already answers. This does not bend the rule that a projection
reaching nothing omits its field: that rule is about an expression having *no
value*, and this one has one.

`[*]` stands as the **whole** projected value and not inside a larger one, so
`array::len(tags[*])` is refused: it has two defensible answers — the function
over the collected values, or the function applied to each of them — and a
language that picks one silently teaches the other by surprise.

Every position that is **not** one of the three contexts is refused by name: a
`[*]` in an ordering, a group key, a function's argument, a `FETCH` route or on
the right of a comparison is an error that says `[*]` is what it does not handle.

**`tags` and `tags[*]` are different routes, and an index is matched to a
condition by exact route equality.** An ordinary index over `tags` holds one
entry for the whole array, so it is never offered for a question about elements —
it would answer that question with an answer about arrays, and an index in this
store changes what a read costs and never what it answers. A condition over
elements takes the scan until a **multikey** index (§4) exists on the same route,
and the access path says which it took.

**A single value is not an array of one.** `tags[*]` over a record holding
`tags: 'urgent'` reaches nothing. That is the rule `CONTAINS` already follows,
and for the same reason: a mistake in a query should show as no match rather than
as a right-looking answer.

A field whose real name contains `.`, `[` or `]` cannot be addressed by a path.
Those three characters are what separates one step from the next.

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
DEFINE INDEX by_home_city ON users FIELDS address.city;
DEFINE INDEX by_tag ON users FIELDS tags[*];

DEFINE TABLE accounts SCHEMAFULL;
DEFINE TABLE follows EDGE;
DEFINE FIELD balance ON accounts TYPE decimal;
DEFINE FIELD opened_at ON accounts TYPE datetime DEFAULT time::now();
DEFINE FIELD holder ON accounts TYPE string REQUIRED;

DEFINE ANALYZER simple FILTERS lowercase, ascii;
DEFINE FIELD body ON notes TYPE string ANALYZER simple;

DEFINE USER root ROLE owner PASSWORD 'a long one';
DEFINE USER ada ON prod.orders ROLE editor PASSWORD 'another';
DEFINE USER grace ON prod.orders ROLE viewer PASSWORD 'a third';

DROP USER ada;
DROP FIELD opened_at ON accounts;
DROP INDEX by_email ON users;
DROP TABLE users;
DROP SPACE sessions;
DROP BUCKET media;
DROP ANALYZER simple;
DROP REPLICA warsaw;
DROP DATABASE staging;
DROP NAMESPACE acme;

ALTER TABLE notes SET SCHEMAFULL;
ALTER TABLE notes SET SCHEMALESS;

ALTER USER grace SET ROLE editor;
ALTER USER grace SET PASSWORD 'a longer one';

DEFINE CONSUMER orders_in
    FROM 'broker-1:9092', 'broker-2:9092'
    TOPIC 'orders'
    GROUP 'shop-orders'
    FORMAT json
    INTO orders
    IDENTITY order_id
    MAP amount AS total, placed.at AS placed_at
    ON FAILURE quarantine
    PARALLELISM 2;

DROP CONSUMER orders_in;
```

**`ALTER USER` is how a user changes after it exists**, and it changes one thing
per statement: `SET ROLE` moves what the user may do, `SET PASSWORD` replaces the
credential. Two statements rather than one with two optional halves, because a
statement that changed only what it named would make *leave the role alone* and
*reset the role* the same sentence — the reason `ALTER FIELD` replaces a
declaration whole is the reason this one does not.

**A consumer is ingestion the catalog holds rather than a script somebody
remembered to start.** One `DEFINE CONSUMER` says what to read (`FROM` brokers,
`TOPIC`), under which group, in what `FORMAT`, where it lands (`INTO`), which
field is the record's `IDENTITY`, how incoming fields `MAP` onto stored ones,
what happens `ON FAILURE`, and how many workers run it. The destination is
resolved **when the declaration is made**, so there is no window in which a
consumer is consuming into a table that does not exist. `DROP CONSUMER` stops it
and removes the declaration; the records it already wrote stay, because they are
records like any others.

**A store with no users is open**, and declaring the first one closes it —
requiring a signin against an empty store locks everybody out of it with no way
in to fix that. Once closed it stays closed: `DEFINE USER` is not an exception an
anonymous session keeps, or it would be a back door anyone could walk through by
declaring themselves an owner, and `DROP USER` is refused for the same reason. A
lost owner password is therefore a restore from backup rather than a recovery.

There are three roles. A `viewer` reads; an `editor` also writes records and
defines structure; an `owner` also declares users and grants. `USE` and the
transaction verbs count as reading, because a viewer that cannot say which
database it is reading cannot read.

A role says which **verbs** a user may use. A grant says which **tables** they
may use them on:

```
GRANT read ON users TO ada;
GRANT read, write ON orders TO ada;
REVOKE write ON orders FROM ada;
```

The rule is one sentence: **a user's grants, if they have any, are the whole
story, and a user with none is governed by their role.** So the first grant is
also a restriction — which is the point, because a role can only widen, and a
permission system that cannot narrow is decoration. A grant is identified by the
pair it names, so granting twice is one grant and granting again replaces what
was there; narrowing is done by re-granting.

**`REVOKE` will not take away the last one.** Going from one grant to none would
widen a user from a named table to every table their role allows, which is the
opposite of what somebody running a revocation is thinking about. Widening is
done by granting, which is a statement whose name says what it does; undoing
scoping entirely is `DROP USER` and a fresh declaration, which is deliberate and
visible in the log as what it is. Dropping a user takes their grants with them,
so a later user allocated the same id inherits nothing.

A grant may narrow what is **read** to named fields:

```
GRANT read ON staff FIELDS name, title TO ada;
```

Naming no fields covers the whole record — the same rule one level down: what is
named is the whole story, and naming nothing names no limit.

**The field is unreadable to the evaluator, not to the printer**, and that is the
whole design rather than an implementation note. `SELECT count(*) FROM staff
WHERE salary > 100000` never shows `salary` and asks about it precisely, so
editing the *answer* would leave the count intact and the field readable one bit
at a time. Instead the record the condition runs against does not contain the
field: the path resolves to `NONE`, the comparison is false by the missing-field
rule the language already has, and the count is zero. The projection then omits
it for the same reason rather than for a second one. An index on a hidden field
changes nothing, because the candidates it offers are re-tested against that same
record — an index narrows and never answers. A join hides it on whichever side
declared it, `FETCH` hides it in the table it lands on, and the change feed hides
it too.

**`FIELDS` may not accompany `write`.** A user who cannot see `salary` but may
write the record would overwrite it whole and destroy what they cannot see — a
hole the permission system would have created rather than closed.

**What grants do not govern**, said plainly rather than left to be assumed: they
are a property of a **session**. A backup reads the log directly and a caller
holding the store through the embedded facade reads whatever they like — both
require the store's own files, and no permission system defends against somebody
who has those. Grants govern who may ask this database a question, not who may
pick up the disk.

**A grant-governed user cannot declare a table.** A grant names a table that
already exists, so there is no grant that could permit `DEFINE TABLE` — the
statement is unreachable rather than refused by a rule, and the refusal says so.
Every table a statement names is checked, not only the first: `RELATE` needs the
two endpoints as well as the edge table, and a join needs both sides.

`ON prod.orders` scopes a user to one namespace and database, and a scoped user
cannot reach another — not through `USE`, and not by naming a database directly
in a statement. The refusal names the tenancy and never says whether the table or
the record exists, because a refusal that leaks that has answered the question it
declined. A user declared without `ON` belongs to the store.

### Authority: a kind and a reach

A role is a rank, and a rank can only say *more* or *less*. The rules an operator
actually has are not ranked: the person who runs the cluster has no business
reading the records, the ingestion identity writes and should not be able to drop
a table, and — the case this exists for — **reading or writing inside a namespace
must not confer creating and dropping databases in it.** No position on a ladder
of three says any of that, because each of them is *some of one rung and none of
the next*.

So an authority is a pair: a **kind**, and the **reach** it holds over.

| Kind | What it is |
|---|---|
| `read` | read records |
| `write` | write records |
| `manage` | create and drop the container's children — databases in a namespace, tables in a database — and define structure on them |
| `govern` | declare users and move authority around |
| `operate` | topology, replicas and the backup file: running the thing rather than using it |

The reach is `STORE`, `NAMESPACE <name>` or `DATABASE <namespace>.<name>`, and
it is written as a keyword so that no table name can be read as a reach. A kind
held at a container is held over everything inside it and over nothing outside.

A user may be declared with the set directly:

```
DEFINE USER ops AUTHORITIES operate PASSWORD 'a long one';
DEFINE USER ingest ON prod.shop AUTHORITIES write PASSWORD 'a long one';
DEFINE USER nadia ON NAMESPACE prod AUTHORITIES manage PASSWORD 'a long one';
```

`ops` runs the node and reads no records. `ingest` writes into one database and
cannot define a table there. `nadia` creates and drops databases in `prod` and
reads nothing in them — the headline rule, said in one statement.

Authority moves the way a grant does, and the statements are the same two words:

```
GRANT manage ON DATABASE prod.shop TO kim;
GRANT read ON NAMESPACE prod TO kim;
REVOKE manage ON DATABASE prod.shop FROM kim;
REVOKE operate ON STORE FROM kim;
```

**Nobody hands out what they do not hold.** `GRANT` is refused when the kind or
the reach exceeds the granter's own — an owner of one database cannot mint an
authority over the store, and cannot widen somebody sideways into a kind they do
not hold themselves. Without that, every other rule here would be one statement
away from being decoration.

**The three roles are still spellings of sets, and they still mean what they
meant.** `ROLE viewer` is `read`, `ROLE editor` is `read, write, manage`, and
`ROLE owner` is all five, each at the reach the user was declared at. That
mapping is deliberately unchanged for users who already exist: `editor` bundles
`write` with `manage`, which is exactly the combination this model was built to
make refusable, and narrowing it on upgrade would be an outage delivered as a
migration. What the model changes is what can now be **said**, not what was
already promised.

**A declared tenancy is a second confinement, and it is asked first.** The `ON`
is not a default that a grant overrides; it is the boundary the session is
inside, and it is checked before the held set is consulted. Otherwise `ON` would
be decoration, and the escalation refused at the declaring side would simply move
to the granting side. The confinement is enforced where a session **selects** a
container — `USE` — so that is also the first thing that has to succeed.

Because of that, `GRANT read ON NAMESPACE staging TO nina`, where `nina` was
declared `ON NAMESPACE prod`, is **refused**. The two tenancies miss each other,
so the holding could never be consulted, and a statement that returns `ok` and
has no effect is worse than either honest answer: the operator's only evidence
that a grant landed is the statement not complaining, and the next person reads
the holding back out of `INFO FOR USER` and believes it. The test is **overlap in
either direction**, not containment in one — `manage ON STORE` granted to a user
of `prod` is usable inside `prod`, because containment runs downward. `REVOKE` is
not refused this way, because taking away a holding that could never be used is
how one stored before this rule existed gets cleaned up.

**Taking authority away reaches a connection that is already open.** A session
re-reads its own user from the catalog on every statement, so `ALTER USER`,
`DROP USER` and `REVOKE` take effect on that session's **next statement**; a
running subscription re-asks on every poll round, so it ends within **one round,
at most 250 ms**. Neither waits for the connection to close, which is the wait a
revocation is least able to afford.

To see what one user reaches, ask `INFO FOR USER`. To see who reaches one table,
ask `INFO FOR ACCESS TO TABLE` (§7c) — both answer from the same check the
statements themselves pass through.

**Signing in is not a statement.** A script is text a caller composes, logs,
pastes into an issue and sends through a proxy, and a password in one is a
password in all of those. It is a method on a session, and over HTTP an
`Authorization: Basic` header. What reaches the catalog — and therefore the log,
and therefore every replica and every backup — is an Argon2 hash.

An index projects one value or several, in order; without `UNIQUE` two records
may share an entry. Each is a path (§3), so an index may project a value nested
inside the record — `address.city` is as indexable as `email`, and a record the
path does not reach is in that index no more than a record missing a top-level
field is. `DROP SPACE` and `DROP TABLE` do the same thing — a space is a
table (ADR-0010) — and both spellings exist so that a script reads the way its
author thinks about what it removes.

Each writes one catalog entry, and the catalog is records (`docs/key-grammar.md`
§9), so a definition takes part in the transaction that issued it: a script may
define a table and write to it, and either both land or neither does.

`IF NOT EXISTS` is written **before the name** — `DEFINE TABLE IF NOT EXISTS
users` — which is where the same clause sits in SQL and therefore where it will
be typed. It is not the same as re-running the statement: a name is unique
within its parent, and a plain `DEFINE` over an existing name is refused.

`DEFINE INDEX` **builds the index in the commit that defines it.** An index
declared on a table that already holds rows indexes those rows, including rows
written earlier in the same transaction, and a `UNIQUE` index constrains them
from the moment it exists — declaring one over data that already violates it is
refused, and the refusal writes nothing at all, not even the definition.

There is no separate backfill to remember, because an index that is visible and
empty would answer a query with *fewer* records and raise nothing.

**An index over several fields serves a condition on its first one.** `FIELDS
last, first` answers `last = 'lovelace'` and `last > 'l'`, because the key
encoding puts `last` first and byte order is value order, so the entries for one
surname are contiguous. It does **not** answer `first = 'ada'` on its own: the
entries for one forename are scattered across every surname, and an index offered
for that would be answering about the wrong column.

The order of the fields is therefore a decision about which reads the index can
serve, not a spelling.

**An index over a route holding `[*]` is a multikey index**: a record contributes
**one entry per value the route reaches**, so `DEFINE INDEX by_tag ON notes
FIELDS tags[*]` is what lets `WHERE tags[*] = 'urgent'` be an index read instead
of a scan. An element added to an array gains an entry, an element removed takes
exactly its own entry with it, and a repeated element is one entry rather than
two. A route with no `[*]` is unchanged, and may sit beside one that has it —
`FIELDS tags[*], city` keeps one entry per tag per record.

**`SPATIAL` indexes a geometry by the cells covering it.** `DEFINE INDEX
by_where ON places FIELDS location SPATIAL` gives each record one entry per cell
of its geometry's covering, and each entry carries the record's own bounding box
— computed in the same commit as the geometry, never afterwards, because a box
that can lag its geometry excludes rows that should have matched with nothing
raised. A record whose `location` is absent or is not a geometry is not in the
index, the same answer every other kind gives for a field it cannot project.

The index is maintained **and read**: the planner covers the query shape with
cells of its own, scans the entries below each of them and looks up the ones
above, rejects what the stored bounding boxes settle, and lets the exact
predicate decide the rest. **Seven of the eight predicates are served this way.**
`geo::disjoint` is the complement of a region, has no sound set of cells, and
stays an exact scan by design.

A box match is a candidate and never a result, which is what keeps the index from
changing an answer: an index may change what a read costs, never what it says.

Three shapes are refused, each because it has no single meaning rather than
because it is hard:

| Refused | Why |
|---|---|
| `FIELDS tags[*] UNIQUE` | it could mean *no two records share an element* or *a record's own elements are distinct*. They refuse different writes, so a caller who meant one and got the other finds out from a rejected write months later |
| `FIELDS tags[*] SEARCH` or `VECTOR` | both already decide their own multiplicity — a search index makes as many postings as the text has terms, and a vector index needs exactly one vector per record to place a node |
| `FIELDS tags[*], aliases[*]` | the entries would be one per *pair* of elements, paid on every write, and "one entry per element" stops having one meaning when there are two sets of elements to be per |

The cost is stated rather than hidden: defining an index reads the whole table
inside the commit. On a table large enough that the pass outlasts the gap between
concurrent writes, the statement fails with a contention error instead of
half-building the index.

`REBUILD INDEX by_embedding ON notes` makes an index's entries exactly what its
table's rows imply, discarding whatever churn left behind:

```
REBUILD INDEX by_embedding ON notes;
```

It exists for the one index that gets **worse on its own**. A vector index's
recall decays as records are removed, because the edges into a removed node are
left where they are (§5), and nothing in an answer says so. Every other index
here is repaired as the records change; this one is the reason the statement is
in the language.

**It is a statement, not something the store decides.** A store that rebuilt on
its own reckoning would rebuild on each replica at a different moment, and from
that moment two replicas would answer the same approximate question differently
— silently, because a slightly different neighbour list looks exactly like a
right one. Written down, the rebuild is one record in the log that every replica
applies at one sequence. It is the same reason a retention policy is a statement
here rather than a job (§8).

A rebuild **cannot change an answer**, for the same reason an index cannot: it is
the entries being made to say what the rows already say. What changes is how well
the approximate read does, and what it costs. It reads the whole table inside the
commit, exactly as `DEFINE INDEX` does, and fails the same way on a table whose
pass outlasts the gap between writes.

`DROP TABLE` removes the definition. It does not delete the table's records,
because that is bulk work whose cost belongs where a caller can see it. Dropping
a **bucket** is the one exception, and it is not really one: a bucket's bytes
live in a companion table created alongside it whose name carries a byte no
identifier can hold, so nothing could ever name it to drop it — it goes with the
bucket or it is orphaned permanently.

### A type that is a set of strings

A field's declared type may be a union of string literals:

```
DEFINE FIELD status ON articles TYPE 'draft' | 'published' | 'archived';
DEFINE TABLE posts (status 'draft' | 'published', title string);
```

Both spellings take it, because it is a type rather than a spelling. What it
buys over `TYPE string` is that the declaration is *about* the column:

```
CREATE articles:1 = { status: 'draft' };      -- accepted
CREATE articles:2 = { status: 'deleted' };    -- refused, naming the set
```

`ASSERT $value IN ['draft', 'published']` expresses the same constraint and puts
it in a worse place. A reader of the schema does not look at assertions, and a
reader of the refusal gets a condition to evaluate instead of a list to choose
from. `INFO FOR TABLE` reports a union as the field's **type**, which is where
somebody goes to find out what a column holds.

The union is a **set**. Members are sorted and deduplicated when the type is
built, so `'b' | 'a'` and `'a' | 'b' | 'b'` are one declaration — a type that
remembered the order it was typed in would be two values for one fact.

A `DEFAULT` is checked against the union when the field is declared, not when a
write first takes it:

```
DEFINE FIELD stage ON articles TYPE 'open' | 'closed' DEFAULT 'gone';
```

is refused, because the alternative leaves the catalog holding a default that no
write of that field could ever accept, and the failure then arrives looking like
the write's fault.

`|` separates the members of a union and nothing else. It is not a boolean or —
that is the word `OR` — and this language has no bitwise operators, so the
character appears in exactly one position and costs nothing elsewhere.

### A table and its columns in one statement

A table's fields can be declared with it, in parentheses after the name:

```
DEFINE TABLE people (
    name   string REQUIRED,
    rank   string DEFAULT 'viewer',
    joined datetime DEFAULT time::now(),
    level  int assert $value > 0
);
```

That is the same declaration as the long spelling — a `DEFINE TABLE` and one
`DEFINE FIELD` per column — written the way it is usually thought. The type
follows the column's name without `TYPE`, because nothing but a type can stand
there; everything after it is what a field declaration always takes, in any
order: `REQUIRED`, `DEFAULT`, `ANALYZER`, `ASSERT`.

It is a **desugaring**, and that is load-bearing rather than an implementation
note. Each column runs through the same code a `DEFINE FIELD` reaches, so
whatever a field does, a column does — including the part that only shows up
later:

```
CREATE readings:1 = { level: 'high' };
DEFINE TABLE IF NOT EXISTS readings (level int);
```

is refused, because the row already there is not an `int`, and the declaration
would otherwise leave the catalog claiming something about the table that the
table does not do. A refusal at any column takes the columns before it and the
table with it: the statement is one unit, and a half-declared table is not a
state it can leave behind.

**Columns do not make the table `SCHEMAFULL`.** A table declared this way still
accepts a field nobody named:

```
DEFINE TABLE loose (name string);
CREATE loose:1 = { name: 'ada', extra: 1 };   -- accepted
DEFINE TABLE tight (name string) SCHEMAFULL;
CREATE tight:1 = { name: 'ada', extra: 1 };   -- refused
```

Of the two readings that is the one the other can be written from: strictness is
one word away, while a lenient table with declared columns would have no
spelling at all if the parentheses implied it.

The flags stand after the column list, never before it — `DEFINE TABLE t
SCHEMAFULL (…)` reads as though the parentheses qualified `SCHEMAFULL`, so it is
refused. Empty parentheses are refused too: writing nothing already says *no
columns*, so `()` can only be a list somebody meant to fill in.

`IF NOT EXISTS` covers the whole declaration, the table and every column. The
alternative tolerates the table and then refuses on the first column, which
makes the statement impossible to re-run — the opposite of what the words ask
for.

### Undeclaring, and what refuses

Every catalog object this language can declare can be undeclared, except one:

| Declared with | Undeclared with |
|---|---|
| `DEFINE NAMESPACE` | `DROP NAMESPACE` |
| `DEFINE DATABASE` | `DROP DATABASE` |
| `DEFINE TABLE` · `DEFINE SPACE` · `DEFINE BUCKET` | `DROP TABLE` · `DROP SPACE` · `DROP BUCKET` |
| `DEFINE INDEX` | `DROP INDEX` |
| `DEFINE FIELD` | `DROP FIELD` |
| `DEFINE ANALYZER` | `DROP ANALYZER` |
| `DEFINE USER` | `DROP USER` |
| `DEFINE REPLICA` | `DROP REPLICA` |
| `DEFINE CONSUMER` | `DROP CONSUMER` |
| `DEFINE NODE` | **nothing — see below** |

**A drop removes its own definition and nothing beneath it**, and refuses while
anything still points at it:

```tessariql
DROP ANALYZER simple;
-- analyzer `simple` still is named by 1 fields, the first being `body`
--   — remove fields first

DROP DATABASE shop;
-- database `shop` still holds 3 tables, the first being `accounts`
--   — remove tables first
```

The refusal **counts and names**, so acting on it does not need a second query.

**There is no `CASCADE`, deliberately.** A destructive statement carrying no
predicate at all is the widest thing this language can be asked to run, and the
person writing `DROP DATABASE staging` is thinking about one name rather than
about everything under it. This is the same rule that made `DELETE FROM t WHERE
c` require a bound — `CASCADE` is that unbounded form under another spelling.

`DROP ANALYZER` is worth its own sentence. A field attaches an analyzer **by
name**, so nothing in the catalog enforces the link and nothing would notice it
break; what a dangling one produces is a search that quietly stops matching,
which is a wrong answer indistinguishable from a right one.

**`DROP NODE` does not exist, and the refusal says why.** `DEFINE NODE` writes
this process's own configuration outside the transaction, so its inverse is an
edit to a configuration file and a restart — and a store where one node could
undeclare another's identity over the wire would be answering a question nobody
asked it. `DROP REPLICA` is the statement that stops counting another endpoint
as a peer.

### Changing a table after it exists

```tessariql
ALTER TABLE notes SET SCHEMAFULL;
ALTER TABLE notes SET SCHEMALESS;
```

**`SET SCHEMAFULL` holds the rows already in the table to the declaration, in
the same commit**, and is refused when any of them carries a field nobody
declared — writing nothing at all, not even the flag. That is the same stance
`DEFINE FIELD` takes one paragraph below, and for the same reason: a constraint
that could be declared over data violating it is a comment.

So the way to tighten a populated table is to declare the fields its rows
actually carry, or remove the rows that do not fit, and then alter it:

```tessariql
DEFINE FIELD extra ON notes TYPE string;
ALTER TABLE notes SET SCHEMAFULL;
```

`SET SCHEMALESS` is never refused: it only widens what is admissible, so no
stored row can contradict it.

`edge` and `bucket` do not move, because both describe what the records already
**are** rather than what may be written next.

A table's fields are also reachable from the table, in the order somebody
thinking about the table writes them:

```tessariql
ALTER TABLE people ADD FIELD name TYPE string;
ALTER TABLE people ALTER FIELD name TYPE string REQUIRED;
ALTER TABLE people DROP FIELD name;
```

`ADD FIELD` and `DROP FIELD` say exactly what `DEFINE FIELD … ON people` and
`DROP FIELD … ON people` say — one function parses the declaration, so the two
spellings cannot drift into accepting different options.

**`ALTER FIELD` is the one that is not a second spelling.** A second
`DEFINE FIELD` is refused because the catalog reserves the name, so redeclaring
needs a statement of its own. It **replaces the declaration whole** rather than
patching the parts it mentions: a statement that changed only what it named
would make *leave the default alone* and *remove the default* the same sentence.

The drop and the declaration land in one commit, so the rows answer for the
**new** declaration — altering a field to a type its rows do not satisfy is
refused, writing neither the removal nor the replacement.

### What a table declares about its fields

A table is schemaless until something is declared on it, and stays schemaless
about everything nobody declared. `DEFINE FIELD` names one field and what it may
hold:

Every one of the fifteen literal types of §3 is a spelling, plus `any`,
`number`, and a union of string literals. **This is the whole set — there are no
others**, and the list is held to that by a test that reads this document and
fails when the engine grows a kind the prose does not name:

| Written | A present, non-null value in the field must be |
|---|---|
| `TYPE any` | anything — a declaration that constrains nothing, so a field can be *declared*, and so appear in a `SCHEMAFULL` table, without being narrowed |
| `TYPE bool` | `true` or `false` |
| `TYPE number` | any of the three numeric forms |
| `TYPE int` | a signed integer |
| `TYPE float` | a binary floating-point number |
| `TYPE decimal` | an exact decimal — the kind money is kept in |
| `TYPE string` | text |
| `TYPE bytes` | opaque bytes |
| `TYPE duration` | a span of time, `1h30m` |
| `TYPE datetime` | a point in time |
| `TYPE uuid` | a universally unique identifier |
| `TYPE table` | a reference to a table |
| `TYPE record` | a reference to one record, `users:ada` |
| `TYPE array` | an ordered sequence |
| `TYPE object` | a map from field name to value |
| `TYPE range` | a span between two values |
| `TYPE set` | a collection with no duplicates |
| `TYPE geometry` | a shape on the sphere |
| `TYPE regex` | a pattern, held rather than executed |
| `TYPE 'draft' \| 'published'` | one of a fixed set of strings, and nothing else |

Written out, so that each of them is shown being declared rather than only
listed:

```
DEFINE FIELD anything ON samples TYPE any;
DEFINE FIELD active ON samples TYPE bool;
DEFINE FIELD quantity ON samples TYPE number;
DEFINE FIELD attempts ON samples TYPE int;
DEFINE FIELD ratio ON samples TYPE float;
DEFINE FIELD balance ON samples TYPE decimal;
DEFINE FIELD title ON samples TYPE string;
DEFINE FIELD blob ON samples TYPE bytes;
DEFINE FIELD took ON samples TYPE duration;
DEFINE FIELD at ON samples TYPE datetime;
DEFINE FIELD trace ON samples TYPE uuid;
DEFINE FIELD source ON samples TYPE table;
DEFINE FIELD author ON samples TYPE record;
DEFINE FIELD tags ON samples TYPE array;
DEFINE FIELD meta ON samples TYPE object;
DEFINE FIELD window ON samples TYPE range;
DEFINE FIELD labels ON samples TYPE set;
DEFINE FIELD where_at ON samples TYPE geometry;
DEFINE FIELD pattern ON samples TYPE regex;
DEFINE FIELD status ON samples TYPE 'draft' | 'published';
```

Five of the names — `table`, `set`, `range`, `datetime`, `uuid` — are reserved
words elsewhere, and are read as type names here for the same reason a field name
inside an object literal may be reserved: after `TYPE`, nothing but a type name
can appear.

**A union of string literals is the one kind that is not a type of the value
system but a subset of one.** `TYPE string` is true of a status column and says
nothing; an `ASSERT` says it but says it where a reader of the schema does not
look. The members are held sorted and deduplicated, because a declared type is a
set and a set that remembers the order somebody typed it in is two values for one
fact.

**Two values satisfy every declaration**, and both are deliberate:

- `none` — the field is not there, so there is nothing to check. This is the rule
  an index already applies to a record missing an indexed field. A declared type
  therefore does **not** make a field mandatory.
- `null` — the field is there and holds nothing, which is what SQL lets a typed
  column hold. Requiring a value is a separate constraint, and this milestone
  does not have one.

**`SCHEMAFULL` is what turns declarations into a schema.** Without it a table
still accepts a field nobody declared — which is the mistake worth catching,
because writing `stauts` where `status` was meant creates a field, raises
nothing, and quietly drops the record out of every query that filters on the name
that was meant. A `SCHEMAFULL` table refuses it instead. The flag is set at
definition or changed later with `ALTER TABLE … SET SCHEMAFULL`, which holds the
rows already there to it in the same commit and is refused if any of them does
not fit.

**A declaration constrains the rows that predate it.** `DEFINE FIELD` holds every
row already in the table to what it declares, in the same commit — and declaring
one over data that already violates it is refused, writing nothing at all, not
even the declaration. A constraint that could be declared over data violating it
is a constraint the store does not have, and every reader afterwards would
believe it did. The cost is the same as `DEFINE INDEX`'s and is stated the same
way: the statement reads the whole table inside the commit.

`DROP FIELD` removes the rule and not the data. The rows keep the field; the
store simply stops having an opinion about it.

**`REQUIRED` means the field must hold a value** — present, and not `null`. One
marker covering both, deliberately: it is what a caller means by "required", and
a field that must be present but may hold nothing is a constraint that
constrains almost nothing. The `NONE`/`NULL` distinction stays fully available on
every field that is not required, and two separate markers remain sayable later.

Like every other declaration, requiring a field holds the rows already in the
table to it: `REQUIRED` over data that violates it is refused, writing nothing at
all — not even the declaration.

**`DEFAULT <expr>` fills the field in when a write leaves it out.** The
expression is evaluated once per write, in the session, so the value that reaches
the store is a value like any other and a replica applies what was written rather
than evaluating anything again.

Four things about it, each stated rather than discovered:

- **A supplied value is left alone**, including `NULL`. A default that replaced
  `null` would make `null` unwritable on any field that has one.
- **It does not reach backwards.** Rows written before the declaration keep
  whatever they had; a retroactive default would be a bulk rewrite hiding inside
  a `DEFINE`.
- **It is a value-position expression**, so a bare name is a *table* and not the
  record's field. A default cannot read the record it is filling in, which would
  be a rule about evaluation order nobody would guess.
- **It is checked when it is declared**, not when it first bites: the expression
  is evaluated once at `DEFINE FIELD` and checked against the type the field
  declares. `TYPE int DEFAULT 'open'` is refused there, rather than accepted and
  then failing on somebody's first write.

`REQUIRED` and `DEFAULT` compose, and together they mean "this field always holds
a value".

**`ASSERT` is what the value must satisfy beyond its type**:

```
DEFINE FIELD balance ON accounts TYPE int ASSERT $value >= 0;
DEFINE FIELD age ON people TYPE int ASSERT $value > 0 AND $value < 150;
DEFINE FIELD status ON orders TYPE string ASSERT $value IN ['new', 'paid'];
```

`$value` is the value being checked, and it is the **only** parameter an
assertion may name. That is the `DEFAULT` rule read forwards: a declaration
belongs to no call, so nothing can bind a parameter in one — and `$value` is the
exception precisely because the *store* binds it, once per record.

It follows the rules `TYPE` already follows, for the same reasons:

- it constrains a **present, non-null** value, so an absent field and a `null`
  one pass. `REQUIRED` is the one constraint about absence, and an assertion that
  also implied presence would make `REQUIRED` mean two things depending on what
  stood beside it;
- **declaring one binds the rows already there**: an assertion over data that
  violates it is refused, writing nothing at all, not even the declaration;
- a violation **fails the whole commit**, so a transaction never lands half
  constrained.

**The comparison may name another field of the same record**:

```
DEFINE FIELD ends_at ON bookings TYPE datetime ASSERT $value > starts_at;
DEFINE FIELD low ON ledgers TYPE int ASSERT $value < high;
```

A bare name on the right is the same thing it is in a `WHERE` — a value read out
of the record being tested — so `ASSERT $value > starts_at` and
`WHERE ends_at > starts_at` read the record through one function and cannot
disagree about it. It is still a closed constraint: the route is **data the
store resolves**, not an expression it evaluates, and the record it resolves
against is the one already being written. Nothing is read from any other record,
so the verdict remains a pure function of the record and the catalog and a
replica reaches it without consulting anything.

The route may reach into a nested value (`window.opens`), and it composes with
`AND`, `OR` and `NOT` like any other comparison. A route holding `[*]` reaches
**several** values and a comparison wants one, so it is refused where it is
written rather than resolving to nothing at the write.

The field it names does **not** have to be declared. The route reads the record,
not the catalog, which is what keeps a lenient table lenient. If that field is
absent or `null`, the comparison is false and the write is refused — the same
answer an ordered comparison against a non-value gives anywhere else in the
language, from the same function.

Refusing names both fields: *"record 21 of table 6 holds a low its declaration
refuses (it is compared with high)"*. Both came from the statement the writer
just sent, so naming the second discloses nothing they did not supply — and
without it, "`low` is refused" leaves them guessing which constraint they broke.
A refusal never names a value, and never names a second **record**.

An assertion is a **closed vocabulary** — `$value` compared against a written
value or a route into the record being checked, combined with `AND`, `OR` and
`NOT` — and anything outside it is refused
where it is written. That is not a limit for its own sake. The check runs on the
store's apply path, where validation has to live so a replica reaches the same
verdict from the record alone; an arbitrary expression is not a pure function of
the record (`time::now()` is not, a read is not), so a store holding one would
have to keep proving that the expression it was given happens to be pure. A
closed vocabulary is pure, total and cheap by construction.

The comparison an assertion makes is **the same function a `WHERE` makes** —
literally the same, in a module both sit above — so `ASSERT $value > 0` and
`WHERE balance > 0` cannot disagree about a value, and a write one node refuses
cannot be one another node accepts.

### Searching text

`MATCHES` asks whether text holds a **word**:

```
SELECT * FROM notes WHERE body MATCHES 'lovelace';
SELECT * FROM notes WHERE body MATCHES 'ada program';
```

Three questions, three operators, and none a special case of another: `LIKE` is
a pattern over the whole value, `CONTAINS` is membership in a collection, and
`MATCHES` is a term in text. `body LIKE '%ovelace%'` finds `lovelacex`;
`body MATCHES 'lovelace'` does not.

**The analyzer belongs to the field.** `DEFINE ANALYZER` names one and a field
declaration attaches it:

```
DEFINE ANALYZER simple FILTERS lowercase, ascii;
DEFINE FIELD body ON notes TYPE string ANALYZER simple;
```

Every search engine puts the analyzer on the *index*. Here that would break the
rule this store applies everywhere else — **which access path runs is decided by
what exists; the answer is not.** An analyzer on an index would make
`body MATCHES 'Lovelace'` find nothing before an index existed and something
after, or different things under two indexes. On the field, an index can only
make the same question faster.

The query is analyzed by the same analyzer as the field, so searching for
`Lovelace` finds a document that stored `lovelace` — which is the entire point
of having one. **Several terms mean all of them**: "find me documents about X Y"
means both.

The tokenizer splits on anything that is not a letter or a digit, and is not
named because there is one to choose from; a knob with one setting is a knob
nobody should have to read about. The filters are the part that differs:

| Filter | What it does |
|---|---|
| `lowercase` | folds case, so `Lovelace` and `lovelace` are one term |
| `ascii` | folds the common accented Latin letters, so `café` and `cafe` are one term |

A letter the fold does not know passes through rather than being dropped — a
letter it has no opinion about is still a letter.

**An index makes it fast and cannot make it different:**

```
DEFINE INDEX by_body ON notes FIELDS body SEARCH;
```

A `SEARCH` index holds one posting per term the field's analyzer finds, so a
`MATCHES` becomes a read of those postings intersected — and the analyzer it
uses is the field's, which is the same one the scan used. That is why adding or
dropping the index cannot change a single answer.

A search index answers a term and nothing else, and an ordered index answers an
equality or a prefix and nothing else. Asking the wrong one would return the
wrong rows rather than none, so the shape of the test is checked against the
index before either is used.

**A field with no analyzer holds no terms**, so `MATCHES` over it finds nothing
rather than failing. A schemaless table is allowed to hold text nobody has
declared anything about, and refusing the query would make that a mistake. So
does a value that is not text, the way `LIKE` already does.

**A declaration names a top-level field, never a path.** `TYPE object` says the
field holds an object and nothing about what is inside it, and `SCHEMAFULL`
refuses an undeclared *field* rather than an undeclared path. Indexing goes
deeper than declaring, which is deliberate: an index is a statement about how a
value is found, and a declaration is a statement about what a record may be.
Making declarations reach into a path needs a rule for what declaring a leaf says
about its parents, and §8 keeps that as its own row.

### Edge tables

`DEFINE TABLE follows EDGE` declares a table that holds **edges**: ordinary
records carrying `out` and `in`, which are record references. Declaring it
creates an index **and** a `TYPE record` declaration on each of those two fields,
in the same commit.

That is the whole of the graph engine, and the indexes are why. Traversal is an
index read, so an edge table whose indexes the caller had to remember to declare
would traverse for some callers and scan for others — the same statement
answering at two different costs depending on something nobody wrote down. The
declaration creates them, so it cannot happen.

The declarations are what let the two markers combine: `DEFINE TABLE follows EDGE
SCHEMAFULL` works because `out` and `in` are already declared, and nobody should
have to declare fields the store itself fills in. Both markers may appear in
either order.

## 4a. Edges

```
RELATE users:1->follows->users:2;
RELATE users:1->follows->users:2 = { since: datetime '2026-01-01T00:00:00Z' };
```

`RELATE` writes one record into the edge table, carrying `out: users:1` and
`in: users:2` plus whatever the optional `= { … }` gives it. Relating through a
table that was not declared `EDGE` is refused rather than accommodated: the
indexes would not be there, so the relation would be written where traversal
cannot find it, and a write nothing can read back is worse than a refusal.

**An edge is identified by its endpoints**, so `RELATE` is idempotent —
re-asserting a link that is already there replaces it rather than adding a second
copy. That is the right default for a caller that re-states what it knows, and it
means two edges between the same pair in the same table are one edge with
properties rather than two records. A caller who needs several distinct edges
between one pair uses several edge tables, one per relation.

### Walking further than one hop

```
SELECT * FROM users:1->follows;
SELECT * FROM users:1->follows->users;
SELECT * FROM users:1->follows->users->follows->users;
SELECT * FROM users:1->follows->users->wrote->posts;
```

A chain is steps written out, and each step is the one-hop read applied to
everything the previous step landed on. The last step may stop on the **edges**
rather than their far side, exactly as a single hop may.

**A step that continues must name the node it continues from**, which is what
keeps `a->e1->e2` unambiguous — that is one step landing on `e2`, and never two
steps with a gap where a node belongs.

**Every arrow points the same way.** Within a step a mixed pair would read as
"the edges out of `a`, then whichever record their `out` names", which is `a`
again for every edge. Across steps a mixed pair asks something real — "who
follows somebody ada follows" — and §8 keeps it as its own row rather than
letting it in as a relaxed rule.

**The answer is deduplicated by record.** Two paths reaching one person answer
with that person once. This store's answers are keyed by record, so twice would
be a wrong answer rather than a verbose one — a caller counting rows would count
two followers-of-followers where there is one.

**A cycle is data, not a mistake.** The number of steps is written in the
statement, so a walk cannot run away, and a walk that arrives back where it
started answers with where it started. Filtering that out would be the store
deciding the question was not meant.

**Every table in the chain is asked about.** A grant on the first table does not
carry a caller into the second: reaching a record through an edge is not a way
around its table's grant, and that holds at the fourth hop as much as the first.

**What it costs.** Each step reads the edge index once per record the previous
step landed on, so the work multiplies by the branching factor at each hop. A
three-hop walk from a well-connected record is not a cheap query, and nothing
here pretends otherwise.

## 5. Record statements

```
CREATE users:1 = { name: 'ada', email: 'ada@example.com' };

SELECT * FROM users:1;
SELECT * FROM users;
SELECT * FROM users WHERE email = 'ada@example.com';
SELECT name, address.city FROM users;
SELECT address.city AS home, tags[0] AS first_tag FROM users;
SELECT price * quantity AS total, string::upper(name) AS shout FROM users;
SELECT *, price * quantity AS total FROM users;
SELECT * OMIT embedding FROM users;
SELECT * OMIT address.postcode FROM users;
SELECT * FROM ONLY users:1;
SELECT * FROM ONLY users WHERE email = 'ada@example.com';
SELECT * FROM users ORDER BY name;
SELECT * FROM users ORDER BY city, joined DESC START 20 LIMIT 10;
SELECT count(*) AS n FROM users;
SELECT city, count(*) AS n, mean(age) AS average FROM users GROUP BY city;

UPDATE users:1 = { name: 'ada', email: 'ada2@example.com' };
UPDATE users:1 SET email = 'ada2@example.com';
UPDATE users:1 SET visits = visits + 1, seen = time::now();
UPDATE users:1 SET address.city = 'Lyon';
UPDATE users:1 MERGE { address: { city: 'Lyon' } };
UPSERT users:1 = { name: 'ada' };
UPSERT users:1 SET visits = 1;
DELETE users:1;
```

**`*` composes.** It may stand among the values written out —
`SELECT *, price * quantity AS total FROM users` answers with the record **and**
the computed column, which is the shape a hand-written list breaks on the moment
a field is added. Where both halves offer a name, the one written out **wins**:
`SELECT *, string::upper(name) AS name` answers with the computed one, the same
rule an alias already follows over the field it shadows. Where the star stands
among the values cannot be observed, because the answer is ordered by name.

**`OMIT <route>` subtracts from what the star put there**, and from nothing
else — a value written out by name was asked for on purpose, so the clause is
refused where there is no star to subtract from rather than accepted and quietly
doing nothing. It takes a **route** and not a name, so
`OMIT address.postcode` keeps the address; it cannot leave out a *position*
(`OMIT tags[0]`), because the rest would renumber and what the answer held at
position one would then depend on what was left out.

The clause this store needs it for is the vector field: without `OMIT`, every
`SELECT *` over a table holding an embedding ships a wall of floats on every row,
and the only escape is to enumerate every other field — the fragile list again.
`omit` is contextual like every other clause word here, so a field or a table
called `omit` still works.

**`FROM ONLY <source>` says at most one record answers**, and the answer is
shaped to match: the record itself rather than a list holding it, so a caller
reading one thing does not unwrap a list of one everywhere. It is an assertion
the author makes, not one the store can check in advance —
`FROM ONLY users WHERE email = $e` rests on a uniqueness that lives in the schema
and in the data — so it is tested **when the read has run**:

- **more than one answered** → the read is **refused**, and the refusal says how
  many, because two is a duplicate and four thousand is the wrong `WHERE`. It
  does not hand back the first one: the records are already correct, so a prefix
  of them costs nothing and looks exactly like success;
- **none answered** → `NONE`. `ONLY` says *at most* one, and an absence is a
  legitimate answer to a question about one thing — which is what lets
  `SELECT * FROM ONLY users:99 ?? { }` mean something;
- **`LIMIT` is applied first**, so `FROM ONLY users LIMIT 1` is the author saying
  which one they want rather than a contradiction.

**`SPLIT ON <route>` opens an array into one record per element**, each carrying
the element where the array stood — which is what makes *"each tag, and how many
notes carry it"* sayable:

```
SELECT tags, count(*) AS n FROM notes SPLIT ON tags GROUP BY tags;
SELECT * FROM people SPLIT ON address.tags;
```

It is applied **after `FETCH`** and before anything that groups, projects or
sorts. After the fetch because a reference resolved once and then opened is the
same answer as one opened and then resolved *n* times, and cheaper; before the
rest because every one of them counts records and this is the stage that decides
how many there are — so an `ORDER BY` sorts the rows and a `LIMIT` bounds them,
not the records they came from.

The identity rides onto every row, so an answer may hold one record id more than
once. That is what "one row per element" means, and it is why the clause is
written rather than implied.

What the route reaches decides the rest, and the four shapes are not one rule
with exceptions:

| At the route | Rows |
|---|---|
| an array of *n* | *n*, each holding one element |
| an **empty** array | **none** — zero elements is zero rows, and any other rule would make the count depend on a special case |
| **nothing** — the field is absent | **one**, unchanged. An array says what the elements are; an absence says nothing about elements at all, so it is not an empty one |
| a scalar or an object | **one**, unchanged. A field's kind is per record here, not per table, so refusing would let one record in ten thousand decide the whole read |

One route and not a list: two would be a cartesian product, which is a different
question and should have to say so. `ON` is required, because `SPLIT tags` reads
as a verb taking an object and what the clause names is the route the rows come
*from*. `split` is contextual, so a field or table of that name still works.

`ONLY` is the one clause word here that is **reserved** rather than contextual, so
a table or field called `only` is not addressable. The reason is where it
stands — exactly where a table name goes. `FROM only limit 1` cannot be told
apart by any amount of lookahead: it is either the table `only` bounded to one
row or this marker in front of a table called `limit`, and `limit` lexes as a
plain name because *it* is contextual. A word whose meaning is settled by
guessing is worse than a name that cannot be used.

`CREATE`, `UPDATE` and `UPSERT` are not three spellings of one verb. They differ
in what each asserts about the record **before** the write: `CREATE` says it is
absent, `UPDATE` says it is present, and `UPSERT` says neither. **`CREATE` over a
record that already exists is refused**, and **`UPDATE` over one that does not
exist is refused** — the alternative, either verb quietly doing the other's job,
loses a record with nothing anywhere to notice.

`UPSERT` is what to reach for when the caller genuinely does not know, and
keeping the other two is what makes it safe to have: a caller who *does* know
keeps the refusal that tells them when they were wrong. Over a record that is not
there, `UPSERT` starts from an empty object — so `SET` and `MERGE` need no case
of their own, and produce exactly what they name.

**`UPDATE` and `UPSERT` have three shapes**, and each is one statement because
all three change exactly one record. Giving a value **replaces** it; `SET`
changes the routes it names; `MERGE` folds an object in. All three are read,
applied and written in one transaction, so nothing lands between the read and the
write.

**`MERGE` is deep where both sides hold an object, and the incoming value wins
whole everywhere else.** An array replaces an array rather than concatenating,
because there is no reading of "merge these two lists" that is right more often
than it is surprising. A field the incoming object does not name is left exactly
as it was, which is the point of the verb. An explicit `NULL` is written, because
`NULL` is a value here — removing a field is `SET route = NONE`, which says
removal out loud rather than hiding it inside a fold.

`MERGE` and `SET` differ in one more way, and it is worth knowing before it
surprises you. `MERGE` takes an object in the **value position**, as every object
literal in this language does, so a bare name inside it is a *table* rather than
a route into the record: `MERGE { visits: visits + 1 }` is refused, and
`SET visits = visits + 1` is how that is said. The rule bought by this is that
`{ a: b }` cannot mean two different things depending on the verb in front of it.
What `MERGE` is for is a whole object arriving from outside — usually
`UPDATE users:1 MERGE $patch`, which is exactly what an HTTP `PATCH` handler
holds and what every client would otherwise fold by hand.

Three rules, each here because the alternative is a surprise:

- **Every right-hand side sees the record as it was**, so
  `SET a = b, b = a` swaps rather than assigning `b` to both. Left to right, the
  meaning of a statement would depend on the order somebody happened to type its
  clauses in.
- **Assigning `none` removes the field.** `none` means the field is not there, so
  writing it in would say the field is there and holds not-being-there — the
  contradiction the value system spends its rules avoiding. `null` is a value and
  stays, which is the difference `= NONE` and `= NULL` already draw in a filter.
- **A route the record does not have is refused, never created.**
  `SET meta.source = 'x'` on a record with no `meta` names the route and stops.
  Creating the objects on the way would be the store writing structure nobody
  asked for.

A right-hand side reads in the **condition position**, so a bare name is a route
into the record — `visits + 1` is the record's `visits`, the same reading a
`WHERE` and a projection give it. The result is an ordinary record write, so the
schema, the defaults, the indexes, the change feed and the grants all apply to it
without knowing which shape produced it.

### Several records at once, at identities the store produces

```
INSERT INTO users (name, email) VALUES ('ada', 'ada@example.com');

INSERT INTO users (name, email) VALUES
  ('grace', 'grace@example.com'),
  ('alan', 'alan@example.com'),
  ('edsger', 'edsger@example.com');
```

`CREATE` is handed an identity and asserts that no record holds it. This one asks
the store for identities it has never used, and answers with them. They are two
words rather than one word with a flag because they differ in what the caller may
do: a caller holding a natural key writes `CREATE`, and a caller loading records
that carry none writes this.

**It always answers with the identities it produced, in the order the rows were
written.** There is no clause to ask for that and none to switch it off. A caller
who supplied no identity has no other way to name what they just wrote, so a
statement answering `done` would force exactly the read this exists to avoid —
and one answering only sometimes would make the shape of the answer depend on a
clause. For the same reason there is no `RETURN` on it: it already answers.

**Every row of a statement lands, or none of them does.** A refusal on the
fourth row leaves the first three unwritten, so a batch cannot be half-applied by
a value the store would not have taken anyway.

**A row holding a different number of values than the field list names is
refused where it is read**, naming both counts and the row that carries them.
That is a property of the text, so nothing is written before it is found — the
statement never begins.

**The field list is grammar, not data.** A parameter is legal in a value
position and refused in a name position, so a caller's text cannot arrive where
a field name belongs.

The rest is the ordinary write path — the schema, the defaults, the indexes, the
change feed and the grants all apply. A bucket refuses it, as it refuses every
write by hand: the records of a bucket arrive through `PUT`.

### A write answering with what it wrote

```
CREATE users:2 = { name: 'grace' } RETURN AFTER;
UPDATE users:1 SET visits = visits + 1 RETURN AFTER;
UPDATE users:1 SET plan = 'pro' RETURN BEFORE;
DELETE users:1 RETURN BEFORE;
```

Every write used to be followed by a read — a second statement, and over the wire
a second round trip, to learn a value the store had in hand a moment earlier.
`RETURN AFTER` answers with the record as it now stands; `RETURN BEFORE` answers
with the record as it stood, which is the only chance to see what a write
replaced.

**The clause is absent by default**, and a write without it still answers `done`.
A store that shipped the changed record back on every write would make the common
case pay for the rare one.

Two pairings are **refused** rather than answered: `CREATE … RETURN BEFORE` and
`DELETE … RETURN AFTER`. Each could only ever answer `NONE`, and answering
`NONE` to a question somebody plainly meant is exactly the kind of quiet wrong
answer this language spends its rules removing. `UPSERT … RETURN BEFORE` does
answer `NONE` when the record was not there — that one is the *true* answer, and
it is what distinguishes an upsert that created from one that replaced.

`RETURN DIFF` is not built. It is refused rather than accepted and ignored,
because what a diff of an array should look like is a design question rather than
a missing line.

### Refusing on purpose

```
THROW 'this order is already paid';

BEGIN;
UPDATE orders:1 SET total = 40;
THROW IF total > 100 THEN 'over the limit' ELSE 'fine' END;
COMMIT;
```

`IF` made a decision **computable**; `THROW` makes it **enforceable**. Before it,
every refusal had to be a condition the store itself happened to check — so a
rule the store does not know, like "this order is already paid", could be worked
out in the language and then not acted on.

The statement never answers: it fails. That matters more than the message,
because **a failure inside a transaction discards the work above it** — a guard
clause that let the writes before it stand would be a comment. The value is
evaluated, so it may name what went wrong; a string is used as it was typed,
because the message is for a person and the quotes are syntax.

### Reading the node itself

```
SELECT * FROM $node;
```

The one source that is not a table. It answers a single record — the node's id,
its roles, its membership, the build it is running, and where peers reach it —
through the ordinary read path, so every clause a `SELECT` has works over it and
nothing new is added to the surface.

It is spelled with a sigil rather than reserved as a word, so `node` stays an
ordinary table and field name for data that already uses one. A parameter is not
legal where a table name belongs, so this reading takes nothing away from a
caller: a parameter they supply *called* `node` is still theirs everywhere a
value belongs, including in the `WHERE` of a read from `$node`'s own neighbours.

It is **not** a system table, and the distinction is not pedantry. A system table
is records in the log, replicated to every replica; a node's identity is in the
store's local metadata precisely so that it does **not** replicate — otherwise
restoring a backup onto a second machine would hand it the first machine's
identity. So it has no tenancy, needs no `USE`, and only an **owner** is
answered: unlike a table read, there is no grant that could narrow it, and roles
and endpoints are a topology rather than data.

### What a read answers with

`SELECT *` answers with the record as it is stored. A named list answers with
only the values it asks for, each read by a path (§3):

```
SELECT name FROM users;
SELECT address.city FROM users;
SELECT address.city AS home FROM users;
SELECT tags[0] AS first_tag FROM users;
```

**A projection is named by the last step of its path.** `address.city` answers
under `city`. Naming it `address.city` would put a `.` inside a field name, and a
field name carrying a delimiter is exactly what a path cannot address — so the
default would produce answers the grammar that produced them could not read back.

A path ending in a **position** therefore has no name, and `AS` is required:
`SELECT tags[0]` is refused, `SELECT tags[0] AS first_tag` is not. So does
anything **computed** — `price * quantity`, `string::len(name)` — for the same
reason: every invented spelling (`tags_0`, `price_times_quantity`) is a
convention the author learns from a surprise.

A projection is read in the same position a condition is, so a bare name is a
route into the record and the whole operator and function surface below is
available in it.

**Two projections that answer under one name are refused**, when the statement is
read rather than when it runs. `SELECT address.city, work.city` would write one
field twice into a name-ordered object and keep whichever came last, so the read
would quietly return half of what it asked for.

**A projected path that reaches nothing leaves its field out**; it does not
answer `none`. `NONE` means the field is not there, so writing it *into* an
object would say the field is there and holds not-being-there. The consequence is
that projected records keep differing shapes, which is the same property that
lets one table hold documents at all — a caller building a fixed-width table out
of the answer has to say what an absence should look like, because the store will
not guess.

The projection does not change the access path. A read whose projection an index
could answer without touching the record is a *covering* read, and choosing to
run one is a planner's decision about how to execute the statement rather than a
change to what the statement says.

`SELECT` resolves to exactly one of the three access paths, and which **kind** is
decided by the target rather than by a cost model. Which index serves it, when
more than one could, is the planner's decision and is described below.

| Form | Access path |
|---|---|
| `FROM users:1` | the record by its identity |
| `FROM users WHERE <indexed path> = <value>` | the index |
| `FROM users WHERE <indexed path> LIKE '<literal>%'` | the index, as a range |
| `FROM users WHERE <indexed path> > <value>` (and `<`, `<=`, `>=`) | the index, as a bounded scan |
| `FROM users WHERE <path> = <value>` (no index) | the table, testing each record |
| `FROM users WHERE <path> LIKE <any other pattern>` | the table, testing each record |
| `FROM users:1->follows` | the index, on the edge table's `out` |
| `FROM users:1->follows->users` | the same index, then each far endpoint by its identity |
| `FROM users:2<-follows<-users` | the index, on the edge table's `in` |
| `FROM users ORDER BY <indexed path> DESC LIMIT <n>` | the index, walked backwards to the bound (§5) |
| `FROM users` | every record of the table |

### Asking for an approximate ordering

Every index in this store may change what a read **costs** and none may change
what it **answers** — except one. A vector index is a navigable graph, and a walk
through it returns the neighbours it found; showing that it missed none would
mean doing the scan the index exists to avoid.

So the exception is written in the statement:

```
DEFINE INDEX by_embedding ON notes FIELDS embedding VECTOR cosine;

SELECT * FROM notes
 ORDER BY vector::cosine(embedding, [0.1, 0.9])
 LIMIT 10
 APPROXIMATE;
```

**`APPROXIMATE` is permission, not a demand.** A read that does not say it gets
the exact scan, whatever indexes exist. A read that says it gets the graph *if*
there is one that answers it — and otherwise still gets the exact scan, which is
better than what was asked for. The access path an answer reports says which
happened.

**The index declares its distance, and there is no default.** A graph whose edges
were chosen by one measure approximates that measure and no other: cosine ranks
by angle and euclidean by separation, and for vectors nobody normalised they
disagree. A read using the other distance is not served, and a default would have
decided that silently. `vector::dot` is served by neither, because the inner
product grows with similarity — ordering by it ascending asks for the *least*
similar.

Several other shapes fall back to the scan rather than being served with a guess:
a read with no `LIMIT` (a walk has nothing to cut), a `DESC` ordering (that asks
for the furthest), a second sort key (it orders records the graph never ranked),
and a `GROUP BY` (it folds the records a walk would have chosen between).

**What it buys, measured rather than claimed:** on two thousand clustered
thirty-two-dimensional vectors, a read of the ten nearest goes from 3.7 ms to
0.62 ms — about six times — while returning the same ten. Recall is measured by
the benchmark harness on every run and by a test with a floor, because it is the
one property here that cannot be argued into existence.

**Deletion decays recall.** Removing a record removes its node and the edges out
of it; the edges *into* it are left, because finding them means reading every node
that might point there. Correctness is unaffected — an index read is a candidate
set and each candidate is resolved at the reader's own snapshot, so a removed
record never reaches an answer — but a graph that has churned heavily walks
through holes. Measured on two thousand clustered thirty-two-dimensional vectors,
removing half of them takes recall of the true ten from **100% to 42%**, and
nothing in the answer says so.

The remedy is `REBUILD INDEX` — see §4.

### Following a reference

A record reference is a value — `posts:1` may hold `author: users:1` — so a
record already says where the other record is. `FETCH` reads it:

```
SELECT title, author.name AS by FROM posts FETCH author;
SELECT * FROM posts FETCH author, reviewer ORDER BY author.name;
SELECT * FROM posts FETCH meta.editor;
```

**It happens before the projection and before the ordering**, which is why
`SELECT author.name` and `ORDER BY author.name` both see the record rather than
the reference. The clause is written where it is applied, the same rule that puts
`START` before `LIMIT`.

**`fetch` is contextual, not reserved.** A field called `fetch` keeps working.

Four rules, each of which is a decision rather than an omission:

- **A reference to a record that is gone stays a reference.** The field still
  holds the name — the one piece of information the caller has — and `NONE` would
  throw it away and make "deleted" indistinguishable from "empty". It also gives
  the answer an unambiguous reading: an object means the record was there, a
  reference means it was not. A traversal answers a dangling endpoint the same
  way, because that is a state of the data and not a failure of the query.
- **An array of references is followed element by element**, since a list of
  references is how a to-many relation is stored here. An element that is not a
  reference is left alone, as is a field that is not one.
- **One level.** The fetched record's own references stay references. That bounds
  the work at one point read per reference and makes a cycle impossible rather
  than handled. `FETCH author.manager` — a route *through* something fetched — is
  not read; write two statements.
- **A route that reaches nothing is not an error**, the missing-field rule one
  level down.

**What it costs, stated rather than measured later:** **one request**, for every
distinct reference the read holds. Distinct, because a read resolves at one
snapshot and two reads of one address at one snapshot must answer the same
thing — so a hundred posts by three authors reads three records, and asks for
them once. The set is gathered from the records already in hand before anything
is read, so the batch is the distinct set rather than a batch's worth of whatever
is nearby; and the addresses travel together whatever tables they name, because
each is asked for as its own bounded range. That is a change to *when* the
records are asked for and not to *which* — the answer is the answer a read
resolving them one at a time gives.

### Matching two tables on a value

`FETCH` is the join a *stored* relationship has: a reference is an address, so
following one is a point read. When nobody wrote an address down, the value is
all the two sides share, and `JOIN` matches on it:

```
SELECT * FROM users JOIN orders ON users.name = orders.who;

SELECT users.name AS who, orders.total AS spent
FROM users JOIN orders ON users.name = orders.who
WHERE orders.total > 5
ORDER BY orders.total DESC;
```

**A row is a record with two named sides**, not the two records merged — a row
above answers as `{ users: <the user>, orders: <the order> }`.

That is not a shape preference, it is the answer to "what happens when both
sides carry `name`". Merging needs a rule for that, and every candidate rule —
an alias syntax, a prefixing convention, last-wins — is something a reader has
to learn from a surprise. Nested, `users.name` and `orders.name` were never in
danger of colliding, and every path, projection, `WHERE`, `ORDER BY` and
`GROUP BY` works over the row unchanged, because it is an ordinary object. It is
also why the two sides of `ON` are written with the table in front: they are
routes into the row, exactly as they look. They may be written either way round.

**A bare `JOIN` is inner** — a row appears only where both sides match. Decided
now rather than later, because it cannot be decided later: if it meant *outer*,
adding `LEFT` afterwards would change what already-written statements answer.

**The join matches exactly what `=` matches**, which across number kinds means
`3` matches `3.0` — see [What a comparison means](#what-a-comparison-means),
where equality is the value system's order. A join that used a different rule
would answer a different question from the operator it is spelled with.

**A join whose two sides hold different kinds of value is refused**, rather than
answering with no rows:

```
CREATE users:5  = { tag: 'users:5' };      -- the identity, typed out
CREATE orders:5 = { by: users:5 };         -- the identity, as a reference

SELECT * FROM users JOIN orders ON users.tag = orders.by;
-- the join matched tag (string) against by (record), and no value of one kind
-- equals a value of the other, so this could only answer no rows
```

Equality across two kinds is false, so such a join can only ever be empty — and
*empty* is also the honest answer to a join over data that simply does not
match. The two are the same answer, and only one of them is a mistake, so the
store says which one it is. Storing an identity as text on one side and as a
reference on the other is the commonest way a join is written wrong, and it is
invisible in the result.

The refusal fires when the answer is empty **and** both sides held something at
the key **and** their kinds share nothing. A join that produced any row is never
refused, and a join over a table with no records yet is never refused — there is
nothing to reconcile. It is deliberately not "refuse as soon as one pair
differs": records carry no declared type, so a single stray value among a
thousand would refuse a join that works.

**What it costs, stated rather than measured later.** An index on the right
side's key means the left side drives and each of its records probes the index;
otherwise the right side is read once into an ordered map and the left side
probes memory. Either way the work is `n + m` rather than `n × m`, and the price
of the second case is that the right side is held in memory. Which one ran is
reported as the access path. There is no cap on the answer and there will not be
one: a cap would let the presence of an index change *what a statement answers*.

**The row's identity is the left record's.** A left record matching two right
records answers as two rows carrying one id, because this store's answers are
keyed by record and a row is not a record. A shape for rows is a change to the
wire, the JSON surface and the console, which is a milestone rather than a
clause.

### Naming a side, and joining a table to itself

`AS` gives a join side the name the row files it under:

```
SELECT * FROM users AS u JOIN orders AS o ON u.name = o.who;
```

The row is `{ u: <the user>, o: <the order> }`, and `ON`, `WHERE`, `ORDER BY` and
the projection all read through the names the statement gave. Once a name is
separable from a table, **a table can be joined to itself**:

```
SELECT * FROM users AS person JOIN users AS boss ON person.boss = boss.code;
```

`users JOIN users` is still refused, and so is `users AS x JOIN orders AS x` —
the check is on the two **names**, because two sides answering under one name
make a row with one half. A name given where there is no join is refused too:
`FROM users AS u` on its own has nothing to file under `u`, and accepting a name
a reader cannot use would be the quieter choice and the wrong one.

### Reading what another read answered

A `FROM` may name a read instead of a table:

```
SELECT * FROM (SELECT * FROM orders WHERE total > 5 LIMIT 100);

SELECT * FROM users AS u
JOIN (SELECT * FROM orders WHERE total > 5 LIMIT 100) AS o ON u.name = o.who;
```

The answer is the inner records themselves — not wrapped, not renamed — so the
outer statement reads them exactly as it would read a table. On a join side it
needs `AS`, because a read has no name of its own.

**The inner read must state a `LIMIT`.** A materialised source has no index to
walk and no bound to push into it, so every record it answers with is held at
once. A source that could grow without limit is therefore refused, rather than
cut at a number nobody wrote: a silently truncated source answers a different
question from the one that was asked and looks exactly like a complete one. It
is the second required `LIMIT` in the language; the first bounds a conditional
delete, and both exist because the quiet answer is the dangerous one.

**A `WHERE` after it asks about what the read produced.** `WHERE` belongs to the
table position — `FROM t WHERE c` — so a grouped read and a traversal had
nowhere to put one. Wrapping either gives it one:

```
SELECT * FROM (SELECT who, count(*) AS n FROM orders GROUP BY who LIMIT 100)
WHERE n > 1;

SELECT * FROM (SELECT * FROM users:1->follows->users LIMIT 100) WHERE city = 'london';
```

`n` is the fold's answer and does not exist until the group is folded, so no
condition inside that read could have named it. The condition here narrows
records that are already in hand; there is no access path left for it to choose,
which is exactly why it can ask a question the inner read could not.

### Reading a range

The four orderings are served by an ordered index, as a bounded scan:

```
SELECT * FROM readings WHERE at >= datetime '2026-01-01T00:00:00Z'
                         AND at <  datetime '2026-02-01T00:00:00Z';
SELECT * FROM people WHERE age > 18;
SELECT * FROM people WHERE name >= 'a' AND name < 'b';
```

**Two bounds on one field are one scan.** `at >= x AND at < y` is a range written
as two conjuncts, and serving only one of them would read half a table to find a
day. Two bounds in the same direction keep the tighter.

**It is not a time feature.** A datetime is a value like any other and is
order-encoded like any other, so the same statement works over numbers, strings
and instants. A range read that only worked for time would be a narrower thing
with a date in its name.

Why it is safe is already stated elsewhere: byte order **is** value order
(`docs/key-grammar.md` §1), so the entries between two bounds are the entries in
the byte range between their encodings.

One subtlety, and the store's own rule dissolves it. The encoding **normalises** —
`1`, `1.0` and `dec 1.00` become one byte string — so an exclusive end cannot be
said in bytes: the bytes equal to the bound are the bound. That would matter in a
store where an index read is an answer. Here an index read is a **candidate set**
and the condition decides, so the scan takes both ends inclusive, over-fetches by
at most the entries exactly equal to a bound, and `> x` discards them the way it
discards everything else. Measured on two thousand records: 845 µs to 107 µs.

### Which index runs

Where a condition offers several conjuncts an index could serve, the one that
promises to narrow the most runs. Not the one written first — that was the rule
until a planner existed, and it meant that

```
SELECT * FROM users WHERE city = 'london' AND email = 'ada@example.com';
```

read the `city` index although `email` is unique and selects exactly one record.

The ranking uses a real number **only where knowing it is free**: an equality on
a unique index produces at most one record, because that is what unique means,
and a `MATCHES` produces at most the smallest term's document frequency, which
is a count of index keys. Everywhere else the shape decides — a single value
beats a range, because `LIKE 'a%'` can be most of the table and an equality
cannot be more than the records holding one value. Ties keep source order, so two
runs of one statement cannot plan differently and an author can predict the plan
from the condition they wrote.

This is deliberately **not** a cost model. One needs to know how many records
hold `city = 'london'` as against `city = 'tromsø'`, which means maintained
histograms — and a stale histogram changes plans silently.

**The plan can only change the cost.** Whichever candidate narrows, the whole
condition is still tested against every record it produced, which is what makes
adding an index — or reordering a condition — unable to change an answer. The
access path an answer reports (`record`, `index`, `ordered` or `scan`) says which
kind of read ran; `EXPLAIN` (§7b) says *which index* served it and on what shape.

A `WHERE` takes a **condition**: an expression that answers with a boolean. In a
condition a bare name is a path (§3) into the record being tested, so every
filter that could be written before still reads the same:

```
SELECT * FROM users WHERE email = 'ada@example.com';
SELECT * FROM users WHERE address.city = 'Paris';
SELECT * FROM users WHERE tags[0] = 'urgent';
SELECT * FROM users WHERE age >= 18 AND city = 'Paris';
SELECT * FROM users WHERE city = 'Paris' AND (age = 17 OR name = 'grace');
SELECT * FROM users WHERE NOT (city = 'Lyon');
SELECT * FROM notes WHERE body LIKE '%lovelace%';
SELECT * FROM notes WHERE body ILIKE 'ada%';
SELECT * FROM notes WHERE tags CONTAINS 'urgent';
SELECT * FROM notes WHERE 'urgent' IN tags;
```

The operators, and how tightly they bind — loosest first:

| Level | Operators |
|---|---|
| loosest | `OR` |
| | `AND` |
| | `NOT` |
| | `=` `!=` `<` `<=` `>` `>=` `IN` `CONTAINS` `LIKE` `ILIKE` |
| | `+` `-` |
| | `*` `/` `%` |
| tightest | unary `-` |

Parentheses override, and **two comparisons cannot be written in a row**:
`1 < age < 100` means "between" to a person and `(1 < age) < 100` to a parser, so
a grammar that picked one would answer a question nobody asked.

### Removing a range

`DELETE` takes an identity; `DELETE FROM … WHERE` takes a condition:

```
DELETE readings:1;
DELETE FROM readings WHERE at < datetime '2026-01-01T00:00:00Z' LIMIT ALL;
DELETE FROM readings WHERE at < datetime '2026-01-01T00:00:00Z' LIMIT 1000;
```

**`FROM` is what tells the two apart, and it is required.** `DELETE readings
WHERE …` would read as a table name where an identity belongs, and a statement
that removes rows should not be one word away from a typo.

**The bound is required too.** A conditional delete carries either `LIMIT n` or
`LIMIT ALL`, and a statement carrying neither is refused before it runs. One of the
two `LIMIT`s in the language that are not optional — the other bounds a
materialised source — and the asymmetry against an ordinary read is deliberate:
a read that omits a bound answers with more rows than the caller expected, while
a delete that omits one empties a table. `LIMIT ALL` costs one
word and is how a retention policy says the whole matched set is what it meant.

`LIMIT n` bounds **what is removed**, never what is examined. The condition
decides first and the bound applies to the records that satisfied it, so a
statement means the same thing whichever index answered it — a bound on
candidates would remove a different set depending on the order an index happened
to be walked in, which is not something the author of the statement chose.

It answers with **how many it removed**, because that is the whole point of a
retention statement: "removed 12 043 readings" is an operator checking their
policy did what they meant, and `done` is that operator running a `count(*)`
before and after to find out.

**It finds its records the way a read does**, so an index serves the condition
when one exists — a policy over an indexed timestamp is a bounded scan rather
than a walk of the table. And the candidates an index offers are still tested
against the whole condition, so a delete cannot remove a record the statement did
not name.

**Everything it removes is in one transaction.** A retention run removes all of it
or none, and a reader at a snapshot sees the table either before or after. What
that costs is worth knowing: the whole matched set is committed at once, so a
statement matching a very large table is a very large commit.

**There is no declared retention policy and no background job.** A policy is this
statement, run by an operator or a schedule — which keeps the decision about when
it runs somewhere a person can see it, rather than in a table nobody reads. The
space comes back through the store's ordinary reclamation once no reader still
needs the versions.

### Counting per window

`GROUP BY` takes an **expression**, so a window is a key like any other:

```
SELECT count(*) AS held, time::bucket(at, 1h) AS window
  FROM readings
 GROUP BY time::bucket(at, 1h);

SELECT mean(level) AS average, time::bucket(at, 1d) AS day
  FROM readings
 WHERE at >= datetime '2026-03-01T00:00:00Z'
 GROUP BY time::bucket(at, 1d)
 ORDER BY day;
```

A bare name in `GROUP BY` still reads as a route into the record — the same
reading `WHERE` and `ORDER BY` give it — so `GROUP BY city` means what it always
did.

**Windows are anchored at the epoch, not at the data.** The same instant lands in
the same window in every query, in every process and on every replica; one
anchored at whatever record happened to arrive first would give two callers
different answers to one question, and neither would notice. Truncation is toward
negative infinity, so an instant before the epoch lands in the window that
*contains* it rather than the one after it.

**A window with no records has no row.** Grouping answers with the groups the
data has, and filling a gap means knowing the range the caller meant — which the
statement does not say. A row nobody wrote is worse than a row nobody sees.

**A window is a whole number of seconds.** A sub-second one needs the nanosecond
remainder in the arithmetic, which is a different function from the one anybody
asks for; it is refused rather than rounded, because rounding would answer a
question nobody put.

### One answer per group

Everything above answers **one row per record**. A fold does not:

```
SELECT count(*) AS n FROM users;
SELECT city, count(*) AS n FROM users GROUP BY city;
SELECT city, sum(spend) AS total, mean(age) AS average FROM users GROUP BY city;
SELECT city, count(*) AS n FROM users GROUP BY city ORDER BY n DESC LIMIT 3;
```

| Fold | Answers with |
|---|---|
| `count(*)` | how many records |
| `count(<expr>)` | how many records where that is present and not `NULL` |
| `sum(<expr>)` | the total; over nothing, `0` |
| `mean(<expr>)` | the average; over nothing, `NONE` |
| `min(<expr>)` / `max(<expr>)` | the smallest and largest, in the value system's order |
| `variance(<expr>)` | the **sample** variance, dividing by `n − 1`; over fewer than two numbers, `NONE` |
| `stddev(<expr>)` | its square root, sample for the same reason |
| `median(<expr>)` | the middle number, or the mean of the two middles; over no numbers, `NONE` |
| `collect(<expr>)` | every present value, as an array, in the order the records arrived; over nothing, `[]` |

**`variance` and `stddev` are the sample forms**, dividing by `n − 1`, because a
table's rows are usually a sample of something — the same reason the SQL
standard's bare `VARIANCE` is `VAR_SAMP`. There is one name rather than two
because the population form is already sayable:

```
SELECT variance(x) * (count(*) - 1) / count(*) AS population FROM readings;
```

Over a single number both answer `NONE` rather than `0`, by the rule `mean`
follows over an empty group: the spread of one observation is not zero, it is a
question nobody has enough data to answer, and zero would be a claim.

**`median` is numeric like `mean`** and refuses the kinds `min` and `max` accept.
It answers exactly, and normalised — not the middle value as it was written —
because `3`, `3.0` and the decimal `3.0` are three *equal* values and three
different answers on the wire, so "the value as written" would be decided by
where a sort happened to leave them rather than by the data. An even count
answers the mean of the two middles, which is a number that was never in the
data; that is tolerable only because the fold is numeric.

**`collect` is the identity fold.** It computes nothing from the values, so it
has no reason to demand they be numbers, and it keeps every kind. Like every
other fold it passes over absent values.

**Two of these folds cost what they read.** `count`, `sum`, `mean`, `min`,
`max`, `variance` and `stddev` each reduce to a running value as the records go
past, so what such a read holds is set by its folds and not by its records.
`median` and `collect` cannot: an exact median has to see every value before it
knows which is the middle, and `collect`'s answer *is* the collection. Where such
a read stands **inside another statement**, the ten-thousand-record ceiling on a
held read applies to it, and a `LIMIT` will not lift it — a `LIMIT` bounds what a
fold answers with, not what it reads. Bound the source instead:

```
SELECT collect(price) AS every FROM (SELECT price FROM sales LIMIT 100);
```

**`GROUP BY` is not required.** `SELECT count(*) AS n FROM users` answers with
one row, because the commonest question the language can be asked should not
need a clause that means nothing.

**A fold is an expression**, so it composes with everything an expression can do:

```
SELECT sum(price) * 1.2 AS with_tax FROM sales;
SELECT sum(price) / count(*) AS per_sale FROM sales;
SELECT city, string::upper(max(clerk)) AS last FROM sales GROUP BY city;
```

A fold's value is **constant within its group**, and that is what makes the
composition mean something: the fold is computed once per group and the result
stands in the expression as a literal, so `sum(price) * 1.2` is arithmetic over a
number and needs no rule of its own.

**A grouped read may answer only with its group keys, its folds, and expressions
built out of those.** `SELECT name, count(*) AS n … GROUP BY city` is refused
when the statement is read, because `name` has as many values as the group has
records and picking one silently is how a wrong number reaches a report.
`SELECT *` over a group is refused for the same reason. `SELECT city, count(*) *
10 … GROUP BY city` is accepted, because every part of it has one value per
group.

**A fold cannot fold over a fold.** `mean(sum(price))` has nothing left to
average: the inner fold has already collapsed the records the outer one would
fold over.

**A fold stands in a projection and nowhere else.** In a `WHERE`, an `ORDER BY`
or a `GROUP BY` key it is refused, and the refusal says what it would be rather
than reporting a stray token: a filter over *groups* is `HAVING`, which is a
second filter position with its own scoping rule and is listed in §8.

**Every fold but `count(*)` passes over absent and `NULL`**, so `count(*)` and
`count(email)` are two questions worth having both of. **Sum over nothing is
`0`** — a sum answering `NONE` would make every caller write the same fallback —
and **mean over nothing is `NONE`**, because an average of no numbers is not a
number. A non-number reaching `sum` or `mean` **fails**, naming the type: the
rule arithmetic already follows, and a silent skip would make a wrong total look
like a right one.

Sums promote the way arithmetic does: a group of integers totals to an integer,
anything touching a float totals to a float. A mean is exact where the division
allows, so it answers as a decimal.

`ORDER BY` and `LIMIT` over a grouped read shape the **groups**, because
ordering runs after projection and a group *is* the projected record by then.
Groups themselves come out in the value system's order of their keys.

`count`, `sum`, `mean`, `min` and `max` are written **without a namespace**,
where every function has one. That is the namespacing rule earning its keep
rather than being broken by it: `array::len` counts one record's array and
`count` counts records, and the spelling says which arity you are looking at.
Only `count(` is a fold — a bare `count` is a route into the record, so a field
named `count` stays readable.

### Order, and how much of it

```
SELECT * FROM users ORDER BY name;
SELECT * FROM users ORDER BY city, joined DESC;
SELECT * FROM users ORDER BY address.city LIMIT 10;
SELECT * FROM users ORDER BY name START 20 LIMIT 10;
```

A sort key is read the way a `WHERE` reads one — a bare name is a route into the
record — and it may also name a **projected** name, so
`SELECT address.city AS home … ORDER BY home` works and answers the same as
ordering by the route.

**A key may name a field the projection dropped**, which is what makes a bounded
nearest-first read writable without projecting the field it measures:

```
SELECT name FROM places ORDER BY geo::distance(shape, $here) LIMIT 10;
SELECT title FROM notes ORDER BY vector::cosine(embedding, $q) LIMIT 10;
```

Where a projected name **shadows** a field of the record, the projected one wins:
in `SELECT rank AS label … ORDER BY label` the key means the rank, because
`label` is the name the answer carries.

**The order is the value system's** (`docs/value-system.md` §3), the same one an
index is stored in, including across types. With one addition that comparison
does not make: **`NONE` sorts below `NULL` sorts below every present value.** A
comparison against a non-value has no answer, so `age < 18` is false for a
record with no age; a *sort* has to put every row somewhere, and where is better
stated than left to whichever row the scan happened to reach first.

**Ties are broken by the record's identity**, which is unique — so the same query
over the same data answers in the same order every time, whatever access path
ran. Without that, adding an index would reorder equal rows, which is an answer
changing when an index appears.

`START` passes over that many records and `LIMIT` keeps at most that many, and
**both are applied after ordering** — including when no `ORDER BY` was written,
because otherwise `LIMIT 10` would mean "the first ten the read happened to
reach", which is a different answer on another node. A `START` past the end
answers with nothing rather than failing: asking for page nine of an eight-page
result is a state, not a mistake.

**A bounded order is taken from an index that is already in it.**
`SELECT * FROM users ORDER BY joined DESC LIMIT 10`, with an index on `joined`,
walks that index backwards and stops — no scan, no sort. The index and the sort
use one order, so there is nothing to compute; the plan reports `ordered` and
names the index (§7b).

**The two directions are admitted on different terms, because of the absences.**
A record with no `joined` has no entry in an index on `joined`, and the sort
still places it — below every value. Descending, those come last, so a bound
reaches them only once the index has run out, and the direction is admitted
unconditionally. Ascending, they come *first*: the records an ascending bounded
read answers with are exactly the ones the index does not hold, so an ascending
read served by the index would answer **short** — real records, fewer of them,
nothing raised.

So ascending is admitted only where there are no absences, and that is a
declaration the planner can read: a **`REQUIRED`** field. The requirement is
refused against a table that already breaks it and enforced on every write
after, so it holds in both directions in time, which is what the read needs.
`ORDER BY joined LIMIT 10` over a `REQUIRED` `joined` is served by the index;
over an optional one it is refused and takes the scan. `REQUIRED` is declared on
a field and promises nothing about what lives inside one, so a route below it —
`ORDER BY address.city` under a required `address` — is refused too.

The permission stops at the read with no condition. Under a `WHERE`, an order is
taken from the index **descending only**, whatever the field declares: the walk
that fills a bound with survivors rests on absences sorting last, and it passes
its own direction rather than the statement's so that argument cannot be handed a
different one. `WHERE city = 'Paris' ORDER BY joined LIMIT 10` over a `REQUIRED`
`joined` narrows on `city` and then sorts the survivors; the order does not come
from `joined`'s index, and the `REQUIRED` on it changes nothing. Not an oversight
and not a soundness limit — the door `REQUIRED` opens is simply not used there,
and no read has asked for it.

One consequence is worth naming because it is not symmetric: ascending needs no
tie-group drain. A forward walk yields a tie group with identities **ascending**,
which is already the order the answer wants, while walking backwards reverses
that inner order and has to drain past the bound to correct it. And under
`REQUIRED` an index that runs out has answered the whole table, so a short answer
is a complete one rather than a handover to the scan.

Everything else is the scan, and answers identically: a
second sort key, a computed key, no `LIMIT`, a `GROUP BY`, a projection (the sort
runs after it and may name what it produced), a `FETCH`, a composite index, and a
field the caller's grant does not include — an order taken from an index would
sort by values a field permission has already removed from the record. An index
that cannot fill the bound hands the read back to the scan, and **the path
reported is always the one that ran**.

`ORDER`, `BY`, `ASC`, `DESC`, `LIMIT`, `START` and `AFTER` are **not reserved
words**. They shape a clause where nothing else can stand, so nothing is
ambiguous, and reserving them would take seven perfectly good names away from
data that already exists — `SELECT * FROM order ORDER BY by LIMIT 1` is a legal
statement.

### Resuming a page from a record

```
SELECT * FROM users AFTER users:1042 LIMIT 20;
SELECT * FROM users ORDER BY joined AFTER users:1042 LIMIT 20;
SELECT * FROM users WHERE active = true AFTER users:1042 LIMIT 20;
```

**`AFTER <record>` answers with the records that sort strictly after that one**,
in the answer's own order. It is written after the order it resumes and before
the bound it fills.

It exists because `START` is the wrong tool for a page walk twice over. It costs
what it passes over, so `START 100000 LIMIT 20` reads a hundred thousand records
to answer with twenty and does it again, one record deeper, on every page. And it
is not correct under concurrent writes: an insert behind the cursor shifts every
later page by one, so a walk to the end skips a record for every insert behind it
and repeats one for every delete.

**The anchor is a record identity and not an opaque token**, because the answer
already carries it — every record comes back under its identity, so the clause
needs no new return channel, no token format and no version of one. Resuming
after the *pair* `(the anchor's key, the anchor's identity)` also breaks ties
deterministically, which a bare key value cannot.

**The clause supplies the order it resumes.** With an `ORDER BY` that is the
order written. With none it is the store's own key order — so a cursor read that
names no order answers identity-ascending rather than in whatever order the
source happened to produce, because a page resumed on an order nobody promised is
a page that moves when an index appears.

**Where the order is the store's own, the read seeks.** A record's key is its
table prefix followed by its identity, so `SELECT * FROM users AFTER users:1042`
begins at a position in the keyspace and the records before the anchor are never
read: a page at the end of a table costs what a page at the start costs. That is
the entire difference between a cursor and an offset, and it is why the clause is
worth having.

**Where it is not, the read walks and says so.** A read that named an `ORDER BY`
is answered in the key the author wrote, which is not a position in the store, so
the records are reached and the ones after the anchor kept. The answer is the
same; the cost is what the offset's was, and the note `cursor-walked` (§7b′) is
how the read says so rather than getting quietly slower page by page.

| Written | Reached by | Says |
|---|---|---|
| `FROM users AFTER users:1042` | a seek | nothing — this is the cheap page |
| `FROM users WHERE … AFTER users:1042` | a walk in identity order | `cursor-walked` |
| `FROM users ORDER BY joined AFTER users:1042` | a walk in the named order | `cursor-walked` |

**A walked page buys correctness and not speed, and the note says so precisely
because the two are easy to confuse.** What every cursor gives — sought or
walked — is a page that does not shift when a record is inserted behind it. What
only a *sought* page gives is a cost that does not grow with depth.

Measured over a hundred thousand records in memory
(`benchmarks/2026-08-28-macos-aarch64-memory-paging.md`), pages of twenty, p50:

| depth | `START n LIMIT 20` | `AFTER … LIMIT 20` | `ORDER BY name START n` | `ORDER BY name AFTER …` |
|---|---|---|---|---|
| 0 | 10 µs | 13 µs | 45 ms | 45 ms |
| 1 000 | 362 µs | 14 µs | 46 ms | 42 ms |
| 10 000 | 3.9 ms | 14 µs | 48 ms | 44 ms |
| 99 000 | 41 ms | **13 µs** | 51 ms | 44 ms |

The sought column is **flat**: a page at the end of the table costs what a page
at the start costs, and at ninety-nine thousand it is some three thousand times
cheaper than the offset it replaces. At depth zero it is marginally *dearer* —
one record read to learn where to start, on a page that begins where the table
does — which is the shape of a cost that does not compound.

The two ordered columns are the honest pair for a walked page, and they are the
same: a read that names an order sorts the table whether it pages by offset or by
cursor, so the cursor neither adds cost nor removes it there. Comparing a walked
cursor against the *unordered* offset would credit it with a sort it did not do,
or blame it for one; the note exists so the reader does not have to guess which.

**What it refuses, and where.** A `START` beside it, because both say where the
page begins and applying both silently skips one. A `GROUP BY`, a `FETCH` or a
`SPLIT ON` beside it, because each answers with something that is not a record —
a group, a record whose references have been opened, a row per element — and the
anchor is a record. An anchor naming another table, because an identity carries
no table once it is compared: `notes:1` and `people:1` compare identically, so a
cursor pasted from the wrong page would answer with real records and no
complaint. All three are properties of the statement, so all three are refused
when the statement is read.

**A deleted anchor ends an ordered walk and not an unordered one.** Without an
`ORDER BY` the identity is the whole key, so the position outlives the record
standing on it and the walk continues — which matters, because the record a
caller last saw is exactly the one most likely to be gone. With an `ORDER BY` the
position is a *value the anchor held*, and with the record gone there is nothing
to resume from; the read is refused rather than guessing, because every guess
picks a page.

### What a comparison means

`=` is exact equality on the whole value. `<` and `>` are the value system's
**declared order across types** (`docs/value-system.md` §3) — the same order
every index range read already uses, because a comparison that disagreed with
the order its own index is stored in is the failure this store keeps refusing: an
answer that changes when an index appears.

Three consequences, stated rather than discovered:

- **There is no three-valued logic.** A comparison answers true or false and
  never "unknown", so `NOT (age = 17)` holds for a record with no `age`.
- **A comparison can cross types.** `age > 18` holds for a record whose `age` is
  the text `'nineteen'`, because a string ranks above a number. `SCHEMAFULL`
  with `TYPE int` is how a table stops holding both.
- **`NONE` and `NULL` are not small values.** An *ordered* comparison against
  either is false, so `age <= 17` does not find every record with no age
  recorded. Equality still sees them as themselves — which is the whole reason
  the language needs no `IS NULL`:

```
SELECT * FROM users WHERE age = NONE;
SELECT * FROM users WHERE age = NULL;
```

Those are two different questions and both are already sayable. A path that
reaches nothing evaluates to `NONE`, because that is exactly what `NONE` means.

`LIKE` is SQL's pattern match, spelled the way SQL spells it because that is what
a person or a tool writes without thinking: the pattern covers the **whole**
value, `%` stands for any run of characters, `_` for exactly one, and `\`
escapes either. That whole-value anchoring is why a substring search is written
`'%text%'`. `ILIKE` is the same test ignoring case.

`CONTAINS` is a different question again: **membership**, not text. `tags
CONTAINS 'urgent'` asks whether an array or a set holds that element, where
`body LIKE '%urgent%'` asks whether text holds those characters. Both exist
because both are asked, and neither is a spelling of the other. `IN` is the same
question from the other end — `'urgent' IN tags` — because both read naturally
in different sentences.

### A value that depends on a test

```
SELECT name, IF age >= 40 THEN 'senior' ELSE 'junior' END AS band FROM users;
```

`IF … THEN … ELSE … END` is an **expression**, not a statement, and that is the
whole of the decision: what was missing was never control flow, it was the
ability to work a value out conditionally in the four places a value stands — a
projection, an assignment, a filter, an ordering. A statement form would have
served none of them.

`ELSE IF` chains, and **one** `END` closes the chain:

```
SELECT IF age >= 60 THEN 'a' ELSE IF age >= 40 THEN 'b' ELSE 'c' END AS band FROM users;
```

Three rules, each stated rather than discovered:

- **`END` is required.** Without it `IF a THEN b ELSE c + 1` has two readings,
  and which one the grammar picked is not something a reader should have to
  know.
- **Only the arm that is taken is evaluated.** That is not only a saving. It is
  what lets `IF qty > 0 THEN total / qty ELSE 0 END` be written at all: the arm
  that is skipped need not be meaningful for the record it is skipped on.
- **No `ELSE` answers with an absence** — not `null`, and not a key holding
  `none`. The answer simply does not carry the field, which is exactly what a
  route into a field the record does not have already does. The two absences
  compose rather than needing a rule apiece.

The test must answer with a boolean, like every other test in the language.

### A fallback for a value that holds nothing

```
SELECT nickname ?? name AS shown FROM users;
```

`a ?? b` answers with `a` unless `a` holds nothing, in which case it answers with
`b`.

**"Holds nothing" means `NONE` or `NULL`, and this is the only place the two are
alike.** It is the right place: the question `??` asks is *is there a value here
for me to use*, and the answer is no in both cases. Everywhere else they stay
apart — `= NONE` finds the records missing the field and `= NULL` finds the ones
holding nothing in it, and they are still different questions.

Only the emptiness of the value counts, not its truth: `false ?? 'x'` is `false`
and `0 ?? 1` is `0`.

Precedence sits where it has to. **Tighter than a comparison**, so
`nickname ?? name = 'ada'` asks what it looks like it asks — `(nickname ?? name)
= 'ada'`. **Looser than arithmetic**, so `price ?? 0 * 2` does not quietly
multiply the fallback. It chains left to right, and the right side is evaluated
**only** when the left holds nothing — so `cached ?? (SELECT …)` does not pay for
a read it does not need.

A lone `?` is refused where it is written. A value in this language is `$name`,
so a single question mark is a typo, and reading it as the start of something
would give a worse error further along.

### Arithmetic

`+ - * / %` and a unary `-`, over numbers only — concatenation is
`string::concat`, so one operator never means two things.

The three numeric kinds **promote**: `int` → `decimal` → `float`. The result is
the wider of the two, where wider means "can hold what the other one holds", so
`int + decimal` is exact and anything touching a float is a float and says so.

**Division always produces at least a decimal.** `7 / 2` is `3.5`, not `3`.
Truncating integer division is the classic silent wrong answer — the query looks
right, the number is wrong, and nothing raises. The cost is stated rather than
hidden: `1 / 3` is a decimal rounded to the type's precision, because no decimal
holds a third.

**Overflow and division by zero are failures, not values.** Integer arithmetic
is checked, and a division by zero fails whatever the kinds — including for
floats, where the hardware would happily produce an infinity. A wrapped integer
or an infinity written into a record is a number nobody meant, and by the time
anyone notices it is stored.

### Functions

Written `group::name(…)`, so a function name can never collide with a field
name and the set stays groupable:

```
SELECT string::upper(address.city) AS shout FROM users;
SELECT string::len(name) AS letters FROM users;
SELECT array::last(tags) AS newest FROM users;
SELECT * FROM users WHERE string::len(name) = 3;
```

Every group, shown being called rather than only named — the table below says
what each one is for, and these say what one looks like:

```
SELECT string::lower(name) AS folded FROM users;
SELECT string::trim(name) AS tidy FROM users;
SELECT string::concat(name, '!') AS shouted FROM users;
SELECT math::abs(balance) AS size FROM accounts;
SELECT math::floor(ratio) AS down, math::ceil(ratio) AS up FROM samples;
SELECT type::of(name) AS what FROM users;
SELECT vector::euclidean(embedding, $probe) AS apart FROM documents;
SELECT vector::dot(embedding, $probe) AS aligned FROM documents;
```

| Group | Functions |
|---|---|
| `string` | `len` (characters, not bytes) · `lower` · `upper` · `trim` · `concat(a, b)` · `split(text, separator)` · `slice(text, start, count)` · `replace(text, from, to)` — the [collections](#collections) |
| `array` | `len` · `first` · `last` · `distinct` · `sort` · `reverse` · `flatten` · `join(items, separator)` · `slice(items, start, count)` — the [collections](#collections) |
| `object` | `keys` · `values` · `len` — the [collections](#collections) |
| `math` | `abs` · `floor` · `ceil` · `round` (half away from zero) · `sqrt` · `pow(base, exponent)` |
| `time` | `now()` · `bucket(instant, width)` — the start of the window an instant is in · `year` · `month` · `day` · `hour` · `minute` · `second` · `unix` · `from_unix(seconds)` — the [calendar](#the-calendar) |
| `type` | `of(value)` — the type's name, as §3 spells it · `bool` · `int` · `float` · `string` · `datetime` · `uuid` — the [casts](#casts) |
| `vector` | `cosine(a, b)` · `euclidean(a, b)` · `dot(a, b)` |
| `rand` | `uuid()` — see [Generated identifiers](#generated-identifiers) |
| `crypto` | `sha256(text)` · `sha512(text)` — lowercase hex; see [Digests](#digests) |
| `search` | `score(field, 'query')` — see [Ranking](#ranking) |
| `geo` | `intersects` · `disjoint` · `covers` · `covered_by` · `contains` · `within` · `equals` · `touches` · `distance(a, b)` · `area(shape)` — see [Shapes](#shapes) |

**What earns a place: a function is here when it cannot be expressed by what the
language already has.** That is why there is no `array::contains` (`CONTAINS`
says it), no `string::contains` (`LIKE '%x%'` says it), and no `is_none`
(`= NONE` says it). `array::last` is the clearest case *for* the rule: a path
takes a literal position and there is no length to subtract from, so "the last
element" is otherwise unsayable.

**The number of arguments is checked when the statement is read**, because the
set of functions is known then. What each argument holds is checked when it runs,
and a wrong one names the function, the position, what was wanted and what was
there.

**A function of an absence is an absence.** `string::len(name)` on a record with
no `name` answers `NONE` rather than failing, which is what lets a read over
records of differing shapes narrow instead of stopping. In a condition that
absence is a **no**: the record did not answer the question, so it is not one of
the records that answered it yes.

That is the only kind of non-boolean a condition accepts. `WHERE tags` is still
refused by the type it found, because a bare path in that position is a question
somebody did not finish writing, and an empty result would hide it where an
error does not.

The exceptions, which have a real answer for an absence rather than a
propagated one, are `type::of` — the type of an absence is `none` — the three
`vector` distances, which answer `+∞` because a distance to something that is not
there is unbounded, and `search::score`, which answers `0` because a record
holding none of the query's words scores zero.

### Casts

Six functions turn a value into a kind, and they are **named after the kinds
themselves**, so a cast and a field declaration say the same word:

```
SELECT type::int(count) AS count FROM arrivals;
SELECT type::datetime(seen) AS seen, type::uuid(tag) AS tag FROM arrivals;
DEFINE FIELD reading ON readings TYPE float;
CREATE readings:1 = { reading: type::float('2.5') };
```

| Written | Reads |
|---|---|
| `type::bool(v)` | a boolean, or the text `'true'` / `'false'` |
| `type::int(v)` | a whole number of any numeric kind, or text spelling one |
| `type::float(v)` | a number a float can stand for, or text spelling one |
| `type::string(v)` | a value that has text it reads back from |
| `type::datetime(v)` | an instant, or RFC 3339 text |
| `type::uuid(v)` | a UUID, or either of its two written forms |

**A cast is an assertion, not a projection.** It produces the kind it names or
it refuses; it never produces something *near* it. So `type::int('2.5')` is
refused rather than truncated, `type::bool(1)` is refused rather than read as
`true`, and `type::string([1, 2])` is refused rather than answered with
`<array of 2>`. Every one of those alternatives returns a value a caller cannot
tell from a correct one, which is the failure this language spends its refusals
on.

**There are three outcomes and they mean three different things**:

| The argument | The answer |
|---|---|
| not there | `none` — the general rule for a function of an absence |
| there, and convertible | the value |
| there, and not convertible | a refusal naming the value and the kind |

The middle row is why a cast narrows a read rather than breaking it —
`type::int(count)` over a table where some records have no `count` answers for
the ones that do. The last row is why a record whose `count` is `'ada'` **fails
the read** instead of quietly dropping out of it: a filter that silently
returned fewer rows than the question asked for would be the one kind of wrong
answer nothing downstream can detect.

**`math::round` is why `type::int` can afford to refuse a fraction.** The
language already says which whole number was meant, three ways, so a cast that
picked one would be answering a question nobody asked:

```
SELECT type::int(math::round(part)) AS whole FROM arrivals;
```

**`type::float` is deliberately not symmetric with that.** A decimal takes its
*nearest* float, because `dec 19.99` has no exact float and a rule demanding one
would refuse nearly every price anybody stores — and unlike `type::int`, there
is no second function to say the conversion instead. An integer past 2^53 still
refuses, because there the nearest float is a **different integer**, and an
integer here is a count or an identity where off by one is a wrong answer rather
than a rounding.

**Two casts were not admitted.** `type::number` names three numeric kinds
without choosing one, so it could only mean "whichever kind it already was",
which is not a conversion — `TYPE number` on a *declaration* is useful for the
opposite reason, that it accepts all three. `type::decimal` is deferred rather
than refused: an exact decimal is the kind money is kept in, so a cast producing
one from a float has to say what it does with a value no decimal holds exactly,
and that deserves its own answer.

### Collections

A path takes a literal name or a literal position and no range. So there is no
way to reach an object's field names, to reorder an array, or to take a run out
of the middle of one — which is what these fourteen are for:

```
SELECT object::keys(address) AS names FROM people;
SELECT array::sort(array::distinct(tags)) AS tags FROM people;
SELECT array::first(string::split(email, '@')) AS handle FROM people;
```

| Written | Answers |
|---|---|
| `object::keys(o)` · `object::values(o)` | the names and the values, in the same order |
| `object::len(o)` | how many fields |
| `array::distinct(items)` | each value once, first occurrence kept |
| `array::sort(items)` | ascending, in the value system's declared order |
| `array::reverse(items)` | the same values, back to front |
| `array::flatten(items)` | one level of nesting removed |
| `array::join(items, sep)` | the elements as one text |
| `array::slice(items, start, count)` | a run of elements |
| `string::split(text, sep)` | the parts between occurrences |
| `string::slice(text, start, count)` | a run of characters |
| `string::replace(text, from, to)` | every occurrence replaced |
| `math::sqrt(n)` · `math::pow(base, exp)` | a root and a power |

**Of two candidate behaviours, the one the other can be written from wins.**
That single rule settles most of the questions above, and it is why several
obvious neighbours do not exist:

- `array::sort` sorts **ascending**, and there is no `array::sort_desc`, because
  `array::reverse(array::sort(x))` is it.
- `array::distinct` keeps the **first** occurrence, so the given order survives.
  `array::sort(array::distinct(x))` recovers the sorted form; nothing recovers
  an order already thrown away.
- `array::flatten` removes **one** level. Two levels is the function written
  twice; a caller handed a deep flatten has no way back.

**`object::keys` and `object::values` correspond position by position.** Nothing
in the language zips two arrays, so a caller reading them separately would have
no way to pair them if they ever disagreed. Both walk the object's own order,
which is name order, because an object is stored keyed by name so that two
objects with the same content encode to the same bytes.

**`array::sort` uses the value system's declared order, which spans types.** A
mixed array sorts rather than failing, and sorts the way the same values sort in
an index — a comparison here that disagreed with the stored order would be an
answer that changes when an index appears. `array::distinct` uses the same
system's equality, so `1`, `1.0` and `dec 1` are one value, exactly as they are
one member of a set.

**`array::join` reads each element by `type::string`'s rule**, so a number joins
and an array is refused. It shares the rule rather than having one of its own
because the language cannot convert an array's elements one by one — a stricter
`join` would leave a caller holding `[1, 2]` with no sentence to write. That is
the same test `type::float` had to pass.

**Positions count characters, not bytes**, in `string::slice` as in
`string::len`. A byte position can land inside a character, and the answer is
then a broken string. Both `slice` functions share one bounds rule: a start past
the end is empty, a count reaching past it takes what is there, and a **negative**
bound is refused — an empty answer would hide a caller who meant "from the end",
which neither function does.

**An empty separator is refused** by `string::split` and `string::replace`.
Each has two defensible readings and no obvious one, so it is named as a
mistake rather than guessed at.

**`math::sqrt` always answers a float**, because most roots are not exact in any
of the three numeric kinds — keeping the argument's kind would round
`math::sqrt(2)` to `1`. A negative root is **refused rather than answered with a
NaN**: a NaN compares false against everything including itself, so it would
travel through a filter and an ordering silently. `math::abs` says the magnitude.

**`math::pow` keeps a whole number whole**, so `math::pow(2, 10)` is `1024` and
not `1024.0`; a fractional base or a negative exponent answers a float. A result
outside the integer range is refused rather than saturated — a saturated power
is a wrong number that looks right, and it is the largest number in the store,
which is the value most likely to pass a check unnoticed.

### The calendar

An instant is a count of seconds. A year, a month and a day are a **reading** of
that count, and eight functions do the reading:

```
SELECT time::year(at) AS year, time::month(at) AS month FROM events;
SELECT label FROM events WHERE time::year(at) = 2026;
SELECT count(*) AS held, time::year(at) AS year FROM events GROUP BY time::year(at);
```

| Written | Answers |
|---|---|
| `time::year(at)` | the calendar year, negative before year 1 |
| `time::month(at)` | 1 through 12 |
| `time::day(at)` | the day of the month, 1 through 31 |
| `time::hour(at)` | 0 through 23 |
| `time::minute(at)` | 0 through 59 |
| `time::second(at)` | the second **of the minute**, 0 through 59 |
| `time::unix(at)` | whole seconds since the epoch |
| `time::from_unix(n)` | the instant a second count names |

**Everything is UTC**, because an instant has no zone. A zone is a rendering
choice made where a value is displayed, and storing one would make two instants
naming the same moment compare unequal.

**`time::second` and `time::unix` are different questions.** The first is the
second of the minute — `9` in `14:37:09`. The second is 1,787,927,829. Both
answer an integer, so nothing but the name distinguishes them at a glance.

**A reading is a value like any other.** It filters, it orders, it groups, and it
sits in a projection. That is what makes a calendar report sayable without
storing the year beside the instant and keeping the two in step.

**A reading of a field that is not there is `none`,** like any other function of
an absence — there is no year that a missing field has. So a read over records
of differing shapes narrows rather than failing.

**`time::from_unix` refuses a fraction rather than truncating it**, on the same
rule the casts follow: `math::round` already says which whole second was meant.

```
RETURN time::from_unix(math::round(1.5));
```

**`time::unix` drops the sub-second remainder, and that is a deliberate
asymmetry with the rule above.** Refusing is affordable only where the language
already holds the sentence a caller should write instead — `math::round` is that
sentence for `from_unix`, and there is no `time::unix_millis` to be the one
here. `time::now()` carries a remainder nearly always, so a strict `time::unix`
would fail the pairing everybody writes. The remainder is still on the instant
when the whole value is kept.

### Generated identifiers

| Written | What it answers |
|---|---|
| `rand::uuid()` | a version-4 UUID that has never been answered before |

The case it is for is a field that gives every record an identity of its own:

```
DEFINE FIELD id ON sessions TYPE uuid DEFAULT rand::uuid();
```

**It is asked again for every record, and that is a property of the planner
rather than of the function.** Every expression that reads no record is
evaluated once above the records and the answer reused — which is what makes a
read over two thousand records affordable, and what makes one statement observe
one instant. A generator reads no record either, so under that rule alone
`SELECT rand::uuid() AS id FROM users` would write **one** identifier into every
row: no error, no failing test, and nothing to see until two records that should
differ do not. Reading no record and being safe to evaluate once are two
properties, and the language now asks both.

**`time::now()` is the reason the question is asked about the function rather
than about impurity.** It is impure too and it *must* be evaluated once, or one
statement observes several moments and `ORDER BY time::now()` sorts by a key
that regenerates under its own comparator. The two impure functions in the
language want opposite treatment, which is why each one says which it wants.

**What comes out is a real UUID**, version and variant bits included, not
sixteen random bytes. `type::string` renders it in the canonical
`8-4-4-4-12` form and that text goes on to be stored, sent and parsed by
something else, which is entitled to the six bits that say what it is.

**A machine that cannot read its randomness source refuses the call** rather
than falling back to a clock, a process id or a counter. Those fallbacks are how
"this cannot happen" becomes two identical identifiers on the day two containers
start from one image in the same millisecond — and a colliding identifier fails
silently, with the second record simply overwriting the first.

### Digests

| Written | What it answers |
|---|---|
| `crypto::sha256(text)` | the SHA-256 digest, as 64 lowercase hexadecimal characters |
| `crypto::sha512(text)` | the SHA-512 digest, as 128 of them |

```
SELECT crypto::sha256(email) AS pseudonym FROM subscribers;
SELECT * FROM uploads WHERE crypto::sha256(body) = $expected;
```

**Text in and text out.** A digest is compared against a stored one, written
beside a record and read in a log, and all three want the form every other tool
prints — an array of thirty-two numbers would make the comparison above
unwritable, and that comparison is the reason the function is here.

**The argument must be text**, and `crypto::sha256(type::string(x))` is the
sentence for anything else. Hashing "the canonical text of any value" would
promise that `3`, `3.0` and the decimal `3.0` — which are *one* value in this
store, since they compare equal — have one digest. They could not, and every
digest ever stored would be tied to today's rendering of every kind.

**These are not for passwords.** SHA-2 is fast by design, and speed is the one
property a stored credential must not have. `DEFINE USER` already hashes with
per-user salt and pinned cost parameters, and there is deliberately no function
that exposes that from a query: a credential primitive behind a grant check is a
credential primitive in the wrong place.

### Shapes

A geometry is a value like any other, so a spatial question is an ordinary
expression:

```
SELECT * FROM places WHERE geo::intersects(area, $search_box);
SELECT name, geo::within(area, $district) AS local FROM places;
```

Eight predicates, and they are the standard ones rather than invented ones:

| Written | True when |
|---|---|
| `geo::intersects(a, b)` | they share any position at all, edges and corners included |
| `geo::disjoint(a, b)` | they share none |
| `geo::covers(a, b)` | every position of `b` is in `a` |
| `geo::covered_by(a, b)` | every position of `a` is in `b` |
| `geo::contains(a, b)` | `a` covers `b` **and** `b` is not only on `a`'s edge |
| `geo::within(a, b)` | `b` contains `a` |
| `geo::equals(a, b)` | they cover exactly the same positions |
| `geo::touches(a, b)` | they meet, and their **interiors** do not |

**`contains` and `covers` differ on the boundary, and that is the point.** A
position sitting exactly on a polygon's edge is *covered by* the polygon and is
not *contained in* it. Both questions get asked in practice — "is this address in
the delivery zone" and "is this address strictly inside it" are different
questions — so the language says both rather than picking one and calling it
containment.

**`touches` is the one that is about interiors.** Two shapes touch when they
meet only along their edges: two districts sharing a border touch, and two that
overlap do not. The interior of a position is the position itself, so **two
positions never touch** — if they meet at all they meet on the inside. The
interior of a path is the path minus its two ends, so **a position touches a path
only at an end**, and a path drawn as a closed loop has no ends and so is touched
nowhere along its length. The interior of an area is the area minus its rings, so
a square exactly filling another shape's hole touches it.

`equals` is about positions, not about text. A square written with a redundant
vertex halfway along one side equals the same square written without it.

**Every answer is exact.** Positions are held on a fixed integer grid, so a
predicate is an integer comparison and there is no tolerance anywhere: two
positions are the same position or they are not. What that costs is stated in
[the value system](value-system.md) — a coordinate finer than the grid is snapped
when it is stored, once, visibly.

**A shape that is not on the planet is refused**, in a query argument as much as
in a record. A longitude of 181 is a mistake upstream, and answering a question
about it would be answering about somewhere that is not there.

#### Measuring

Two functions, and both answer in **SI units**:

```
SELECT name FROM places ORDER BY geo::distance(shape, $me) LIMIT 10;
SELECT name, geo::area(zone) AS square_metres FROM districts;
```

`geo::distance(a, b)` is the distance along the ellipsoid between two
**positions**, in metres. Both arguments must be positions: the distance from a
position to a *larger* shape is the distance to the nearest part of it, which is
a different computation and is not written yet — so a polygon is refused by name
rather than answered about from one of its corners.

**There is no distance in degrees, anywhere.** Not exposed, not labelled, not
behind a flag. A function returning degrees is a function somebody reads as
metres, and the mistake is invisible because the number looks reasonable at every
latitude except the ones where it matters.

A distance to something that is not there is `+∞` rather than `NONE`, for the
same reason the vector distances answer that way: `NONE` sorts below every value,
so a bounded nearest-first read would otherwise answer with exactly the records
that have no shape, in first place.

Two positions on **opposite sides of the world** answer `NONE`. The solution
does not converge there, and the number it would otherwise return is wrong by an
amount nobody can bound.

`geo::area(shape)` is how much ground a shape covers, in square metres. Zero for
anything with no interior. Holes are subtracted; the members of a multi-polygon
add up, which is why the store refuses a multi-polygon whose members overlap —
the shared ground would otherwise be counted twice with nothing to say so.

Areas are computed on the sphere with the same total surface as the ellipsoid,
and edges are the lon–lat straight lines the geometry actually says rather than
great circles. So a box from 0°N to 60°N is the ground between two **parallels**,
which is what a reader of the coordinates expects.

#### Writing one

A shape is written the way RFC 7946 writes one, behind a marker:

```
CREATE places:1 = { name: 'the office',
  at: geometry { type: 'Point', coordinates: [2.35, 48.85] } };

SELECT * FROM places
  WHERE geo::intersects(at, geometry { type: 'Polygon', coordinates:
    [[[2.2, 48.8], [2.4, 48.8], [2.4, 48.9], [2.2, 48.9], [2.2, 48.8]]] });
```

The seven names are RFC 7946's — `Point`, `LineString`, `Polygon`, `MultiPoint`,
`MultiLineString`, `MultiPolygon`, `GeometryCollection` — and a collection's
members are written as plain objects inside `geometries`, as that document does.

**`geometry` is a contextual word, not a reserved one.** A table may be called
`geometry` and so may a field; what tells the two apart is the brace, since a
table name is never followed by an object. Reserving the word would have taken a
usable name away from data that already exists.

**A literal is written out in full.** A parameter, a field or a call inside one
is refused, because a literal is read when the statement is parsed and a shape
that could differ per record is not a literal. Such a shape is supplied as a
**bound parameter** instead, which is the complete path and is what a client
uses.

**Validity is not judged when the literal is read.** An unclosed ring parses; it
is refused when it reaches a record, after snapping, because that is the only
place the shape being judged is the shape that will be stored.

**A shape crossing the date line is written as two.** An edge more than half the
world wide in longitude can be joined two ways — the short way across ±180, or
the long way round everything else — and the coordinates do not say which. Since
the two are each other's complement, the store keeps neither and says so, rather
than picking one and being wrong about it in silence. Write the short way as two
shapes meeting at the meridian, which is what RFC 7946 asks producers to do
anyway:

```
CREATE runs:1 = { at: geometry { type: 'MultiPolygon', coordinates: [
  [[[179, 0], [180, 0], [180, 1], [179, 1], [179, 0]]],
  [[[-180, 0], [-179, 0], [-179, 1], [-180, 1], [-180, 0]]]] } };
```

and write the long way by putting a position between the two ends, after which
no edge reaches half the world and the shape can only mean the one thing. A
`MultiPoint` is unaffected: a set of positions has no edges, so there is nothing
in it to read one way or the other.

The console prints a shape in exactly this form, so what comes out of a query can
be pasted back into the next one.

#### The index, and which questions it serves

`DEFINE INDEX … SPATIAL` makes seven of the eight predicates a narrowed read
instead of a scan:

```
DEFINE INDEX by_where ON places FIELDS area SPATIAL;
```

The index keys each record by the cells covering its geometry and carries the
record's bounding box alongside. A read covers the **query** shape with cells of
its own, reads the entries under and above them, and rejects what the stored
boxes already settle — then the condition tests the survivors against the real
geometry, exactly as it tests the candidates of every other index here. **A cell
match is a candidate and never a result**, so declaring the index cannot change
what a query answers; it changes only what the answer costs.

The field may be either argument. `geo::contains(area, $box)` and
`geo::within($box, area)` ask the same question and both are served.

`EXPLAIN` reports such a read as shape `region`, with `cells` — how many cells
the query was covered by, which is how much of the key space the read touches.

**`geo::disjoint` is deliberately not served** and stays an exact scan. It is the
complement of a region, and a complement has no set of cells: every record whose
box misses the query is disjoint, and so is every record whose box meets it but
whose shape does not. Serving it from cells would answer with a fraction of the
true set and raise nothing.

Under `NOT` or on one side of an `OR`, a geometry filter is a scan for the same
reasons every other filter is.

#### The nearest few

The same index answers "the closest ones", which is an order and a bound and
needs no new syntax:

```
SELECT * FROM stops
 ORDER BY geo::distance(at, geometry { type: 'Point', coordinates: [2.35, 48.85] })
 LIMIT 10;
```

The read walks cells **cheapest first**, keyed by a distance nothing inside the
cell can be nearer than, and stops as soon as the best cell left is further away
than the worst answer it already holds. That is exact rather than approximate:
`APPROXIMATE` is not asked for and is refused here, because a floor that is
really a floor means no record the walk skipped could have ranked. The answer is
the scan's answer, in the scan's order, including every record tied at the
bound.

The place may be either argument, and `START` counts towards what the walk asks
for. `EXPLAIN` reports the read as access `ordered` with shape `nearest`.

An ordering has nothing to re-test — the entry's position *is* the answer — so
this read is more careful about when it declines than the filters above are. It
falls back to the exact scan when the sort is not one a walk produces (a second
key, a descending one, no `LIMIT`, a projection, a `FETCH`, a `GROUP BY`), when
the field is not visible to the caller, when this transaction has written to the
table, when the read is at an older snapshot than the committed tail, and when
the index runs out before the bound is filled — which is the case where the
answer needs records with no geometry, since those have no entry and sort last.

`geo::distance` takes positions, so a record holding an area is an error in the
statement. The walk reports the same error the scan does rather than answering
around it.

#### What is not there yet

There is no measured tuning of how finely a query is covered — the budget is a
declared constant, and the candidate-to-result ratio the store measures is what
will move it. A nearest-first read under a `WHERE` is still a scan. And there is
no distance between shapes larger than positions, which is also why the
nearest-few read is over positions.

### Ranking

Relevance is an ordinary expression too, so a ranked search is the same shape as
a nearest-neighbour read — an order and a bound:

```
SELECT title, search::score(body, 'lock contention') AS relevance
  FROM notes
 WHERE body MATCHES 'lock contention'
 ORDER BY relevance DESC
 LIMIT 10;
```

`search::score` computes **BM25**: a word occurring here counts for more when
fewer other documents hold it, repeating it helps less each time, and a long
document is not rewarded for holding a word by accident.

**A score is measured against a collection, and that is the whole difference
from `MATCHES`.** `MATCHES` asks whether *this* text holds *these* words, which
is a property of the record — so a scan and an index answer it identically, and
adding an index cannot change what a query returns. A score asks how much that
matters, which cannot be read off any one record: it needs how many documents
there are, how long a typical one is, and how many hold each word. Those are
maintained by the search index alongside its postings.

So **a score over a field with no search index is refused**, naming the field.
Answering zero instead, or scoring against whatever records happened to be read,
would produce an ordering that looks exactly like a ranking and is not one — and
nobody checks an order that looks right. A refusal is a statement that did not
run; a plausible wrong order is a statement that did. For the same reason a
single-record read (`FROM notes:9`) has no collection in scope and is refused
too.

**A record holding none of the query's words scores `0`**, which is the computed
answer rather than an absence standing in for one, and sorts where it belongs
under the `DESC` a ranked read is written with.

`k1 = 1.2` and `b = 0.75` — how fast repetition stops helping, and how much
length is held against a document — are **constants of this implementation**, not
options on the index. That has a cost worth stating plainly: changing them in a
release changes the order results come back in, without any statement changing.
They become part of `DEFINE INDEX` when there is a measurement to justify a
different value.

### Nearest neighbours

This language needs no operator for k-nearest-neighbour, because **"the ten most
similar" is an order and a bound**, and it has both:

```
SELECT * FROM notes ORDER BY vector::cosine(embedding, [0.1, 0.9]) LIMIT 10;
```

A vector is an **array of numbers** — not a sixteenth type. The value system's
set is fixed, an array of numbers is a vector, and a new type would need an
encoding, an ordering, a literal syntax and a migration to buy nothing.

Three distances, because three questions: `cosine` measures the angle and
ignores magnitude, `euclidean` measures distance in space, `dot` is the inner
product. Embedding models are trained for one or another, and using the wrong
one returns plausible neighbours that are not the nearest — silently — which is
why all three exist rather than one being picked.

**Cosine answers a distance, not a similarity** (`1 - cos θ`), so smaller is
nearer and `ORDER BY` reads the way every other ordering reads. A similarity
would sort backwards and every query would carry a `DESC` nobody could explain.

**What has no distance is infinitely far, not absent.** Vectors of different
lengths, an empty one, a value that is not an array, a record with no embedding
at all: each answers `+∞`. `NONE` would sort *below* every value, so a bounded
read would answer with the records that have no vector, in first place, looking
exactly like results. The two are still told apart by the thing that tells
values apart — `WHERE embedding = NONE` — because sorting is the only thing a
distance is for.

The statement above is **final**: an index over the vectors would serve it
faster and cannot change an answer, because a distance is a function of the two
values and of nothing else.

**An absent or null argument answers `none`**, without the function being run —
so `array::len(tags)` over a table where some records have no `tags` narrows
rather than failing. The exceptions are the functions that **have** an answer for one: `type::of`
asks about a value rather than computing from one, and a `vector` distance to
something that is not there is unbounded rather than unknown.

`time::now()` is read once, in the session, so the instant that reaches the log
is a value like any other — a replica applies what was written rather than
asking its own clock and reaching a different answer.

**A condition must be a boolean.** Every operator above answers with one, so this
only bites when a bare path or literal stands where a question was meant:
`WHERE tags` is refused, naming the type it found. It is not a question with a
false answer; it is a question that was not finished, and an empty result would
hide that.

Only text satisfies a text pattern, and only a collection satisfies membership —
a number in that field is not an error, the record simply does not match. A field
holding a single value is **not** a one-element collection, so `name CONTAINS
'ada'` finds nothing rather than quietly meaning `name = 'ada'`: a mistake in the
query should show as no match, not as a right-looking answer.

Deliberately nothing cleverer: no tokenising, no stemming, no ranking. Those
belong to an analyzer, and a scan-shaped approximation of one now would give
answers that a real text index later disagrees with. A query whose answer changes
when an index is added is worse than a slow one.

**Which access path runs is decided by what exists, not by how the query is
written.** An index read serves two shapes: an equality on an indexed path, and
a `LIKE` pattern that is a literal followed by a trailing `%`, which asks for the
values beginning with that literal and is a range over the same index. Either may
sit inside a conjunction: `city = 'Paris' AND age = 17` uses an index on `city`
if there is one.

**The records an index offers are then tested against the whole condition.** The
index answered one conjunct and the statement asked for all of them, which is
what makes an index a narrowing device rather than an answer — and is why adding
one still cannot change what a query returns.

Neither side of an `OR` may narrow by itself, because a record satisfying the
other half would be missed; under a `NOT` an index finding the matching records
is exactly the wrong set. Both keep the scan. So does a comparison whose
right-hand side reads the record — `left = right` has no one value to seek to.

Everything else reads the table and tests each record. The statement is identical
either way, so adding an index later makes existing queries faster without
rewriting any of them. The path taken is reported with the result, so a scan is
visible rather than folklore.

| Condition | With an index on that exact path |
|---|---|
| `path = 'ada'` | index read |
| `path = 'ada' AND <anything>` | index read, then the rest applied |
| `path LIKE 'ada%'` | index read — a range over the values beginning with `ada` |
| `path LIKE '%ada'`, `'%ada%'`, `'a_a%'`, `'ada%lace'` | scan |
| `path ILIKE 'ada%'` | scan |
| `path CONTAINS 'ada'`, `'ada' IN path` | scan |
| `path < 'ada'`, `path > 'ada'` | scan — an ordered index could serve this as a range, and that is not built yet |
| `path = 'ada' OR <anything>` | scan |
| `NOT (path = 'ada')` | scan |
| `path = <another path>` | scan — no single value to seek to |

**On that exact path**, and no other. An index on `address.city` serves a filter
on `address.city` and not one on `address`, for the same reason an index on
`(a, b)` does not serve a question about `a` alone: an index answers what it
projects.

`ILIKE` keeps the scan in every shape, because the index holds one case and
folding at read time is not what it stores. An infix or suffix pattern keeps it
because an ordered index answers "begins with" and not "contains". Both are
reported as scans rather than served as a narrower answer quickly — a statement
whose answer changes when an index appears is worse than a slow one.

That property holds on populated data too, and it is the reason `DEFINE INDEX`
builds its entries in the same commit (§4). An index that existed while empty
would answer the same statement with *fewer* records and raise nothing — a change
of access path that is also a change of answer.

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

## 6a. Files

A **bucket** is a table whose records are files.

```
DEFINE BUCKET media;
PUT media:'/logo.png' = 0x89504e47;
READ media:'/logo.png';
```

`PUT` writes a file's whole content in one commit, so a half-written file is not
a state this store can be in. `READ` answers its bytes, or `NONE` where there is
no file. Bytes or text may be written — a file is very often text, and making a
caller spell out `0x…` for a document would be ceremony with no property behind
it — and what comes back is bytes either way, because that is what a file is.

**Part of a file, read or written**, with the two words a bounded read of rows
already uses meaning the same two things over bytes:

```
READ media:'/big.bin' START 1048576 LIMIT 4096;
PUT media:'/big.bin' START 1048576 = 0x00ff00ff;
```

Only the chunks a range touches are read, so asking for a kilobyte of a large
file costs a kilobyte's worth of chunks. A range beginning past the end answers
**empty** rather than failing — the rule a `START` past the last row already
follows.

A ranged write is **one commit**, exactly as a whole-file write is, which is what
lets it exist without a rule for what a reader sees midway: there is no midway.
Its bound is the bound a whole-file write already has — what one transaction can
hold — and building a file across *many* commits is a different feature with its
own visibility rule, named in §8.

Leaving `START` out replaces the file; giving an offset — **any** offset,
including zero — writes at it and keeps whatever lies beyond the bytes given. One
spelling per thing. A write that would begin past the end of the file is
**refused**: zero-filling the gap would be the store inventing bytes nobody
wrote, and a real hole is a sparse-file feature nobody has asked for.

**A file is a record, and its bytes are records too.** That sentence is the whole
design, and everything below it follows rather than being built:

```
SELECT * FROM media;
SELECT * FROM media WHERE size > 1000000 ORDER BY updated DESC;
CREATE users:1 = { name: 'ada', avatar: media:'/logo.png' };
RELATE users:1->attached->media:'/logo.png';
DELETE media:'/logo.png';
```

Listing a bucket is a query. Pointing a record at a file is a record reference,
which is one of the fifteen value types, so `FETCH` follows it. Relating a record
to a file is an ordinary edge. Removing a file is `DELETE`, and the bytes go with
it in the same commit. A grant on the bucket governs the metadata and the bytes
together, because there is one table to grant on. A backup carries files, because
a backup carries the log.

What a bucket's records hold is what the store knows: `size`, `chunks` and
`updated`. **They are not written by hand** — `CREATE`, `UPDATE` and `SET`
against a bucket are refused, because metadata a caller can write is metadata
that can disagree with the bytes, and nothing would ever catch it.

A file is named by a **path**, so its identity is text: `media:1` is refused.
That is not an aesthetic rule. A chunk's identity is the path followed by its
ordinal, and an integer identity and the text of that integer would produce the
same chunk — two files sharing bytes, which is not a defect anybody finds twice.

## 7. Transactions

```
BEGIN;
  CREATE users:1 = { name: 'ada' };
  SET sessions:'abc' = users:1;
COMMIT;
```

`CANCEL` discards what the transaction has done so far:

```
BEGIN;
  CREATE users:2 = { name: 'grace' };
CANCEL;
```

A statement outside `BEGIN` is its own transaction.

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

## 7a. Backing up a node that is serving

```
BACKUP;
BACKUP FROM 4096;
```

`BACKUP` answers with the store's **log**, as the file the backup reader and the
verifier already read. `FROM` makes it incremental — the records at or after that
sequence — and a base plus its increments answers what the original answers.

**It is a statement because a serving node is the only thing that can take one.**
This store is single-writer, so a node that is up holds the store and no second
process can open it. The node has to be asked, and the language is how this store
is asked — so the HTTP route below is a surface over this statement rather than a
second implementation, and the CLI and the wire protocol get it without one
either.

**A concurrent write does not tear it.** The log's tail is fixed before the first
record is read, and the file stops there. A write that lands mid-backup is
outside the file rather than half inside it, and the tail written into the header
is what makes "outside" checkable rather than a claim.

**It needs an owner, and a grant can never permit it.** A backup is every record
in the store, past every grant and every tenancy boundary — so it is the one
statement whose scope is the *store* rather than the selected namespace, and
there is no permission smaller than "may see all of it". A grant names a table
and a backup names none, so a user who holds grants is refused by name rather
than let through by an empty list that reads as permission.

Over HTTP:

```text
GET /backup
GET /backup?from=4096
```

Same identity rules, because they are the statement's. The body is the file.

The cost is stated rather than discovered: the whole file is materialised,
because a statement answers with a value. `FROM` is what bounds it, and a
streaming answer is named in §8.

## 7b. Looking at a plan

```
EXPLAIN SELECT * FROM users WHERE city = 'Paris' AND email = 'ada@example.com';
```

answers with the plan the read would take, without taking it:

```json
{"access": "index", "index": "by_email", "shape": "equality", "at_most": 1, "table": "users"}
```

`access` is one of `record`, `index`, `ordered`, `scan`, `approximate`, `graph`,
`join` or `materialised`. An index-served read also names the index and the
**shape** that served it — `equality`, `prefix`, `range` or `terms` — and carries
`at_most` when a ceiling was free to learn, which today means an equality on a
`UNIQUE` index.

**The answer carries the same structure**, for the read that actually ran. Over
the embedded API it is `Outcome::plan()`; over HTTP it is the `plan` object on a
records response, beside the `path` word that response has always carried. The
two are one type filled by one set of functions, so a plan cannot describe a
choice the read did not make.

They agree everywhere except one case, and that case is the point of reporting
both. The planner cannot know whether a **descending ordered** index will fill
the statement's bound — that question *is* the read — so `EXPLAIN` reports the
order it chose while a read whose index ran out reports the `scan` it settled
for, and the answer carries a `fell-back` note (§7b′) naming both. Making them
agree by running the read inside `EXPLAIN` was considered and rejected: it would
cost `EXPLAIN` the property that makes it worth having.

A materialised source reports `materialised` and not the inner read's own path:
the outer statement performed no access of its own, and saying `index` there
claimed an index this statement never touched. The inner read's plan is a plan of
its own, and is not folded into one field.

`ordered` is a bounded read taken from an index already in that order (§5), and
it names the index. **Descending** it is the one plan with a condition it cannot
check: whether the index holds enough records to fill the bound is the read
itself, and an index that runs out hands the read to the scan — which is then
what the read reports, and says so in a note (§7b′). **Ascending** the plan
carries no such gap, because the direction is admitted only over a `REQUIRED`
field, where an index that runs out has already answered the whole table.

**A number this store cannot know is a number it will not print.** There is no
estimated row count and no cost, because producing one needs statistics about
value distribution and this store keeps none (§8). A plan carrying a made-up
estimate is how somebody comes to trust one.

**It explains what actually runs.** The same enumeration and the same choice, not
a second planner that agrees today and disagrees after the next change.

**It needs exactly the permission the read needs**, over exactly the same tables.
An `EXPLAIN` that named the index serving a table the caller may not read would
be a metadata disclosure wearing a diagnostic's clothes.

Only a read has a plan to describe. A write's cost is its index maintenance,
which is a different report rather than this one wearing the same word.

## 7b″. Saying which path you expect

```
SELECT * FROM users WHERE city = 'Paris' USING index;
SELECT * FROM users WHERE city = 'Paris' USING INDEX by_city;
SELECT * FROM events USING scan;               -- yes, I mean the scan
```

`USING` is **optional** and is a **refusal, never a router**. It does not choose
a path and cannot make a read faster; it fails the statement when the path taken
is not the one named. That converts the worst failure mode an indexed store has —
the query that quietly stops using its index and starts scanning — from something
you find out from a latency graph into something the statement says out loud. It
also makes a read self-documenting without duplicating anything, because the
assertion is *checked*.

`USING <path>` takes one of the access-path words (§7b): `record`, `index`,
`ordered`, `scan`, `approximate`, `graph`, `join`, `materialised`. A word that is
none of them is refused before the read runs, listing the ones that exist.

`USING INDEX <name>` asks the question the path word cannot: `index` says *an*
index answered, this says **which**. A read served by the wrong index is a plan
regression the path word alone cannot see.

**It is checked against what the read did, not against what the planner chose**,
and that is the whole design. A descending ordered index that cannot fill the
bound hands the read to the scan (§7b); an assertion satisfied by the planner's
intention would pass in exactly that case — the one it was written to catch. So
`USING ordered` is refused there and `USING scan` is permitted, because the scan
is what honestly happened.

The cost of a refused statement is the read it already did. That follows from the
same rule and is not an oversight: the assertion is about what happened, so it
cannot be settled before anything has.

An assertion inside a materialised source is about the **inner** read. The outer
statement is `materialised` whatever the inner one did, so the two are about
different reads and both hold at once.

## 7b‴. Saying how long a read may take

```
SELECT * FROM events WHERE at > 0 TIMEOUT 200ms;
SELECT * FROM huge ORDER BY at DESC LIMIT 10 TIMEOUT 5s;
```

`TIMEOUT` is **optional** and puts a wall-clock ceiling on the read. A read that
passes it is **refused, not truncated** — it answers nothing, and the refusal
says how far it got.

That is the whole decision. When the ceiling passes, the records already found
are correct and handing them back would cost nothing and look like success; a
caller counting them, summing them or writing them somewhere would be wrong and
would have no way to find out. A partial answer that looks whole is the failure
this language spends its rules removing, and a timeout is the cheapest place to
introduce one. The count that a shortened answer would have carried is in the
refusal instead, where it cannot be mistaken for a result.

**What it bounds, exactly.** The ceiling is checked once per record, as the read
produces it. That covers where a long read spends its time — decoding a record,
testing it, projecting it, offering it to a sort — and it is stated rather than
implied: it does not interrupt a single call to the storage layer, and it does
not reach a read standing in an *expression*, which has no channel to carry a
budget into. So a read that goes on producing records stops; a read blocked
below the language does not, and no clause here can make it.

**Nesting narrows and never widens.** A subquery may set a tighter ceiling than
the read holding it, and may not set a looser one — whichever budget expires
first refuses. An inner clause able to raise its caller's budget would make the
outer ceiling a suggestion, which is not what a ceiling is.

**A ceiling that could only refuse is refused when the statement is read.**
`TIMEOUT 0s`, and any negative span, name no budget a statement could satisfy, so
they are caught before a scan runs to be refused by them.

`timeout` is **not** a reserved word — an index may still be called `timeout`,
and `SELECT … USING INDEX timeout` still names it. Which reading is meant is
settled by whether a duration follows, the same way every other contextual word
in this grammar is settled.

## 7b‴′. Reading the store as it stood

```
SELECT * FROM docs VERSION 4102;
SELECT * FROM orders WHERE status = 'open' VERSION 4102;
```

`VERSION` is **optional** and moves the whole read to an earlier point in the
store's history. Records are versioned by a suffix on their own key, so this is
the read the store already performs with a different sequence rather than a
second mechanism bolted beside it.

### Why it is a sequence and not a timestamp

The number is a **log sequence** — the same one a transaction's snapshot is, and
the same one the store reports as its committed tail. It is not a wall clock,
and the clause deliberately does not accept one.

No log record carries a timestamp, so a time would have to be resolved through a
mapping. More importantly it would be a spelling that looks more precise than
what it addresses: two commits within the same millisecond are ordered by
sequence and by nothing else, so no timestamp can name a point between them.
Naming the number the store actually orders by is the honest version, and it is
the number a caller already has.

### The schema is read at that version too

A historical read answers *what did this store look like then*, and the schema is
part of what it looked like. So a table, a database or a field defined after the
version named is **not known** at it, and the read refuses rather than resolving
today's name against yesterday's records — which would be a third state that
never existed.

### Indexes do not answer a historical read

An index entry carries no version. Entries are derived at commit, so an index
describes the present and nothing else, and consulting one for a read of the past
gives two different wrong answers from one cause:

- a record that matched then and has been updated since has no entry under its
  old value, so it goes **missing** from the answer;
- a record that matches now but did not then has an entry, is resolved at the old
  snapshot, and comes back **not satisfying the condition it was selected by**.

Neither raises anything, which is why a versioned read is served from the scan
instead. The clause therefore has a cost, and it is the honest one: a read at an
earlier version is a scan even where the same read in the present is not. The
reported access path says so.

**A graph traversal is refused rather than scanned.** Edges are followed through
the edge table's direction indexes; that is the mechanism and not a shortcut past
it, so there is nothing to fall back to. A traversal's answer is also the least
inspectable shape this language produces, so one assembled from today's edges
over yesterday's records would simply be believed.

### The two refusals

A version **ahead of the committed tail** is refused: there is no state there,
and answering with the present would let the same statement return one answer now
and a different one later while naming the same version.

A version **below the reclaim floor** is refused: reclamation removed the
versions that stood there, so the read would resolve to something older, or to
nothing, and call that the past. The floor is raised only by a pass that actually
removed something, and it is the store's only durable record of that boundary —
without it a historical read could not tell *this record did not exist then* from
*the version that said so has been removed*.

### It cannot be used inside a transaction

A transaction **is** a point in the store's history: one snapshot, held for as
long as it runs, which is what makes its reads agree with one another. A
statement inside one asking for a different point is asking for something a
transaction cannot be, so it is refused rather than quietly answered at the
transaction's own snapshot.

`version` is **not** a reserved word — a field may still be called `version`, and
`SELECT version FROM releases` still reads it. Which reading is meant is settled
by whether a sequence follows, the same way every other contextual word in this
grammar is settled.

## 7b′. What the answer says without being asked

`EXPLAIN` answers a question you have to know to ask. A **note** is the other
half: the read says what it did, on the answer, without being asked.

```json
{
  "kind": "records",
  "path": "scan",
  "plan": { "access": "scan", "table": "events" },
  "notes": [
    {
      "kind": "fell-back",
      "message": "the ordered path could not fill the bound, so the read took the scan path instead"
    }
  ],
  "records": []
}
```

A read has had two channels since it existed: the records and the refusal.
Neither can carry *this answer is correct, and there is something about it you
would want to know* — an error refuses an answer that is right, and the records
say nothing about how they were reached. So the third case has been silence, and
silence is how an operator finds out an index stopped serving a read by noticing
the read got slow.

**A note never changes what a statement answers.** A caller that ignores every
note gets exactly the records it would have got before notes existed. The `notes`
key is absent when there is nothing to say, which is almost always — a note is
worth reading because it is rare.

There are five today:

| kind | what happened |
|---|---|
| `fell-back` | an index held the order and could **not** fill the bound, so the read took the path it names instead |
| `approximate` | the answer is the best the graph found, not provably the best there is (§5, *Asking for an approximate ordering*) |
| `subquery-ceiling` | a materialised source reached the `LIMIT` it stated, so the outer statement asked its question of a prefix |
| `compared-across-kinds` | the read compared values of two different kinds — a number against the text of one, say — so it answered about the records whose kinds happened to line up |
| `cursor-walked` | an `AFTER` page was reached by reading the records rather than seeking to the anchor, so it cost what the read costs and not what the page costs (§5, *Resuming a page from a record*) |

**`fell-back` fires on an index that declined, never on a table that has none.**
A bounded ordered read over an unindexed table is the most ordinary read in the
language and gave nothing up; a note on it would fire so often that nobody would
read the ones that matter.

**`compared-across-kinds` fires on a real crossing and never on an absence.**
A schemaless store lets one record hold `age: 30` and the next `age: '30'`, and
`WHERE age = 30` then matches some of them — correctly, and narrower than the
author meant. A record with **no** `age` compares `none` instead, and that is the
ordinary case a schemaless read is built for: it is how a read over records of
differing shapes narrows rather than failing. A note there would fire on nearly
every read in the language, which is worse than no note because it looks like a
feature. `null` is left out from the other side, being a value deliberately
written rather than a mistake.

The note names a **pair of kinds, once**. A comparison runs per record, so a read
over a million mixed records has one thing to say and not a million; and the pair
reads the same way whichever side of the `=` each half was written on.

**`cursor-walked` is about cost and never about the records.** A page that
sought and a page that walked are the same records in the same order; only the
first one costs the same at any depth, which is the failure `AFTER` exists to
remove. The note is not a fallback in the `fell-back` sense — nothing declined —
it is the read saying that on this statement the clause bought correctness rather
than cost. Without it the difference is invisible until somebody times it.

**`subquery-ceiling` is not a truncation.** The bound is the caller's own word
and a materialised source is required to state it (§5, *Reading what another read
answered*). What the note adds is that the bound was *reached*: a prefix of an
answer and a whole one are the same shape, so the outer statement's question was
asked of less than the inner read could have given.

Notes reach every surface: the embedded API (`Outcome::notes()`), the HTTP body
above, and the binary protocol, where a `Records` answer carries each note's kind
and message. The `tessaridb` shell prints them under the answer.

They cross the wire without a version gate, because an outcome is
length-prefixed and a reader advances by the declared length rather than by what
it consumed — so a client built before the notes existed steps over them, exactly
as it steps over an outcome kind it has never heard of. The other direction is
the one that needed saying: a body that **ends** after the records is a node with
nothing to say, not a truncation.

## 7c. Asking the catalog what it holds

```
INFO FOR STORE;
INFO FOR NAMESPACE;
INFO FOR DATABASE;
INFO FOR TABLE users;
INFO FOR USER ada;
INFO FOR ACCESS TO TABLE users;
INFO FOR NODE;
```

Each answers with an object read **from the catalog**, not from a description
kept beside it — so a report cannot describe a schema the store no longer has:

```json
{"tables": ["orders", "users"]}
```

```json
{"table": "users", "schemafull": true, "edge": false, "bucket": false,
 "fields": [{"name": "email", "type": "string", "required": true}],
 "indexes": [{"name": "by_email", "fields": ["email"], "unique": true, "search": false}]}
```

Names come back in **name order** rather than in the order they were declared,
so two stores built from the same schema by differently ordered scripts describe
themselves identically.

A namespace and a database are the **selected** ones. A caller asking about
another says `USE`, which is where this store already answers the tenancy
question — a second way to name one would be a second place for that check to be
got wrong.

### What a caller may see of it

**A report says only what the caller could have found out anyway.** That is the
rule a grant-governed subscription already follows on the change feed, applied
to a description instead of to records.

So four of the six subjects **narrow** rather than refuse:

- `INFO FOR STORE` and `INFO FOR NAMESPACE` show a scoped user their own
  tenancy and no other.
- `INFO FOR DATABASE` lists the tables the caller may read. A table they were
  never granted is absent, exactly as its records are.
- `INFO FOR TABLE` names its table, so a caller without that grant is **refused**
  — the same refusal a `SELECT` from it gives. What is left is the field grant,
  which edits rather than refuses (§4): a caller granted `FIELDS name` is not
  told that `salary` is declared, and is not told that `by_salary` indexes it.
  An index is named after the values it projects, so listing one names them.

`INFO FOR USER` is the fifth, and it **refuses**: it needs an owner, the same
permission that declares a user. Its content is the permission system itself,
and there is no smaller truthful answer about who may do what — a grant list
with rows quietly removed reads as the whole of what that user can reach. The
report never carries the password hash, which the stored definition does hold.

`INFO FOR ACCESS TO TABLE` asks the same question from the other end — *who
reaches this object*, rather than *what does this person reach* — and refuses for
`INFO FOR USER`'s reason, since it is made of the same material. It answers with
one row per user the caller administers, each saying whether that user may read
the table and whether they may write it:

```json
{"table": "orders",
 "access": [{"user": "ada", "read": true, "write": true},
            {"user": "vic", "read": true, "write": false}]}
```

**Every answer is obtained by asking, never by deriving.** For each user the
store signs a throwaway session in as them and puts a real `USE`, a real `SELECT`
and a real `DELETE` to the ordinary authorization path — the same function every
statement goes through. A report that worked out reachability from grants and
roles would be a second opinion about a rule that already has one, and two
opinions agree until they do not; the moment they stop, nothing fails and the
report simply becomes fiction, read by the one person who cannot check it.

The `USE` is not a formality. A declared tenancy is enforced where a session
*selects* a container and nowhere afterwards, so a user of another namespace
answers `false` because they could not have got there — which is the same reason
their `SELECT` is refused, rather than a second rule that remembers to exclude
them. Users who reach nothing are still listed: an absent row would say *cannot
reach this* and *the caller cannot see this person* with the same silence.

`INFO FOR NODE` is the sixth, and it refuses for the same reason in a different
key: it names no table, so a grant check would pass over it for reasons unrelated
to permission, and roles and endpoints are this machine's position in a topology
with no smaller truthful version to hand a viewer. See §7d.

**The catalog's own tenancy is not listed, and not because it is filtered out.**
It was never created through the language, so it has no definition record to
find — the same property that makes `USE NAMESPACE` unable to select it. A
listing that had to remember to exclude it would be one somebody could forget to.
A bucket's chunk table is absent for the neighbouring reason: its name carries a
byte no identifier can hold, so nothing can name it and nothing lists it.

## 7d. Configuring the node

There is no configuration file and no environment variable for what a node is
for or which peers it has. Both are statements, and what they write lives in the
store:

```
DEFINE NODE ROLES serving, writable ENDPOINTS 'db-1.internal:9000';
DEFINE REPLICA second AT 'db-2.internal:9000' ROLES serving, writable;
INFO FOR NODE;
```

`DEFINE REPLICA`'s `ROLES` is optional and is spelled exactly as `DEFINE NODE`'s
is, because it is the same membership field seen from the other side — one
written about a peer, one about this node, and two spellings for one set of words
would be two things to keep in step. It is what a forwarded write is routed by: a
node that may not write sends the statement to the peer whose roles carry
`writable`. Left out, the peer is declared with no roles and takes no writes,
which is the safe absence — the operator who forgot the clause gets a refusal
naming it, where the opposite default would send a write to a node nobody said
could take one. `INFO FOR NODE` reports each peer's roles for the same reason: a
setting that decides routing but cannot be read back is one nobody can check
before the bad day.

A node configured by a file beside a store configured by statements is **two
sources of truth for one node** — they agree until the first restore and then do
not. All three need an owner.

### The two halves, and why the answer names them apart

The two `DEFINE`s write to two different places, and the test deciding which is
sharp: *what happens when this setting is replayed on another machine — does that
machine become confused about which one it is?*

| Setting | Where it lives | What a replay of it would do |
|---|---|---|
| `ROLES` | this node's local metadata | a replica that inherited `writable` would accept writes it must forward |
| `ENDPOINTS` | this node's local metadata | peers would be told to reach this machine at the original's address |
| a peer | the catalog, replicated | every node learns the peer exists, which is the point of declaring one |

So `INFO FOR NODE` reads both and answers them as **two named groups** rather
than one flat object:

```json
{"id": "9f2c…", "roles": ["serving", "writable"], "membership": "alone",
 "version": "0.0.2", "build": "0.0.2-alpha", "endpoints": ["db-1.internal:9000"],
 "cluster": {"peers": [{"name": "second", "endpoint": "db-2.internal:9000"}]}}
```

`version` and `build` are both here because they answer different questions.
`version` is three ordered numbers: it is what the node **stored** and what an
upgrade compares, and comparing is why it has no room for a pre-release suffix.
`build` is what this binary actually is, suffix included. On a final release the
two read the same, and the difference only appears when there is one — which is
exactly when somebody needs to see it. `tessaridb --version` prints the second
of the two, because a binary is asked what it is before any store is opened.

The flat fields would **not** follow a backup; everything under `cluster` would.
Flattening the two would make that a thing you have to remember, and the day it
is remembered wrongly is the day last night's backup goes onto a fresh machine
and two processes claim one identity.

### What each clause does, and what there is no spelling for

Either clause of `DEFINE NODE` may stand alone, and a clause left out **leaves
its field alone** — so setting the endpoints is not a silent way to strip a node
of its roles. What a clause does name **replaces** what was there, the same rule
a grant's field list follows. There is no spelling for removing a single role,
because that would need a spelling for removing the last one, and that question
is worth answering when there is a second node to answer it against.

`membership` is reported and cannot be set. It reads `alone` because that is a
fact about this process; the moment a node joins a cluster, the *name* of that
cluster is topology and belongs on the other side of the line.

**There is no statement that configures a *remote* node.** You configure a node
**on** it. A statement that reached across would be a second mechanism for
something already decided elsewhere, and the two would disagree the first time a
node was unreachable while its row said otherwise.

`NODE` and `REPLICA` are read as contextual words, so `DEFINE TABLE node` is
still an ordinary table for data that already uses the name.

## 8. What is deliberately absent from this milestone

Named here rather than merely missing, so each absence reads as a decision.

**A row leaves this table when the thing it names is built**, with the statement
that disproves it recorded — because a table of absences that keeps rows for
features that exist is worse than no table: it is a document that has stopped
being read. Two rows left on 2026-08-22: a full-text **index**, disproved by
`DEFINE INDEX by_body ON notes FIELDS body SEARCH` (§4), and a **grant matrix**,
disproved by `GRANT read, write ON orders TO ada` and its `FIELDS` form (§4).
Row-level security, which the second row also named, genuinely is absent and now
says so on its own. A third left the same day: **rebuilding a vector index that
has churned**, disproved by `REBUILD INDEX by_embedding ON notes` (§4); a fourth,
**an expression over a fold**, disproved by `SELECT sum(price) * 1.2 AS with_tax`
(§5); a fifth, **traversals longer than one hop**, disproved by
`SELECT * FROM users:1->follows->users->follows->users` (§4a) — which left three
narrower rows behind it, because building the half that was asked for showed
exactly what the other half would take; and a sixth, a **projection over `[*]`**,
disproved by `SELECT tags[*] AS all_tags FROM people` (§3) — that row asked what
an empty reach projects and treated it as an open question, and the answer fell
out of the denotation rather than being chosen, which is why the row is gone
rather than answered in place; and a seventh, a **multikey index**, disproved by
`DEFINE INDEX by_tag ON people FIELDS tags[*]` (§4), which left two narrower rows
where it stood — the `UNIQUE` reading and the two-route product, each of which is
a decision somebody has to make rather than work somebody has to do. An eighth left with this wave: **`ASSERT`**, disproved by
`DEFINE FIELD balance ON accounts TYPE int ASSERT $value >= 0` (§4). Its row had
named the constraint correctly and the fix wrongly — "a guard the store calls and
the session implements" cannot work, because a replica applying a log record has
no session. What was actually needed was already in the tree: the comparison
module was **already pure**, and only lived above the store by accident of where
it was first wanted. A ninth left with this wave: **a descending bounded
scan**, disproved by `SELECT * FROM users ORDER BY joined DESC LIMIT 10` (§5).
Its row read "a saving that needs the `LIMIT` pushed into it", which was true and
was the smallest part: what the walk actually needed was a rule for the tie group
straddling the bound, since a key is stored as its value followed by the record's
identity and the answer breaks ties by identity *ascending*. It left two rows
behind it — the ascending case and the composite one — each of which is a
property of the key order rather than work left undone.

A tenth left differently from the nine before it. **A range over the first column
of a composite index** was disproved by
`SELECT * FROM users WHERE last > 'l'` on `DEFINE INDEX by_name ON users FIELDS
last, first` (§4) — and the wave that built it did not notice. Every earlier
departure was found by the wave that caused it, which is the cheap case: the
author knows what they just built. This one was found by reading the whole table
against the tree at once, three sections away from where the change was made.
That is the argument for auditing the table rather than trusting each wave to
police itself, and it is why the audit found two more things this list cannot
show: a row whose *reason* was false the day it was written, and a second copy of
these absences kept in code that had drifted from this one.

An eleventh is the first of the two the ninth left behind: **an ascending bounded
read served by an index**, disproved by `SELECT * FROM events ORDER BY at LIMIT
10` over a `REQUIRED` `at` (§5). Its row named the door in advance — a `REQUIRED`
field, where there are no absences — so what is worth recording is not that the
door opened but that it had to be **measured** rather than read. Reading the
schema code said the requirement was checked on write, which would leave a record
written *before* the declaration invisible to it and the read quietly short. Run
instead of inferred, `DEFINE FIELD … REQUIRED` is refused against a table that
already breaks it, so the invariant holds at declaration as well as at every
write, and that is the half the read actually rests on. A row that names its own
door still owes the door a test.

A twelfth is the **other** row the ninth left behind: **an order served from a
composite index**, disproved by `SELECT * FROM users ORDER BY joined DESC LIMIT
3` on `DEFINE INDEX by_joined_name ON users FIELDS joined, name` (§5). This one
is not a door that opened — it is a **reason that did not survive being read
twice**. The row argued that the tie group at the bound is a group of leading
values *"and a leading value cannot be read back out of a key"*, which is true:
the index encoding normalises and offers no way back. The conclusion does not
follow, because a tie test never asks what an entry **holds**. It asks whether
two entries **agree**, and agreement is byte equality over a prefix the encoding
already guarantees is self-delimiting. The property the row cited as the
obstacle — normalisation — is in fact what makes byte equality the *right* test:
`1` and `1.0` are one value and belong in one tie group.

What the composite genuinely costs is the opposite of what the row claimed, and
was not written down anywhere: **it takes the tie-group drain back**. Ascending
over a single-field index needs none, because a forward walk yields the group
already ordered by identity. A composite's group is ordered by the *next* indexed
field instead, so a bound cut inside it answers with the wrong members in either
direction. The eleventh departure's cheerful asymmetry lasted exactly one wave.

**The thirteenth departure** removed this row's sibling — a range on a composite's
second field under an equality on its first — and its interest is where the
obstacle turned out to be. Not in the key: `IndexValues::leading` already encodes
a *slice* of values and the existing call site passed a one-element one, so the
generalisation was one argument, and with the fixed run empty every byte it
builds is identical to what it built before. The obstacle was in the **ranking**.
`plan::better` compared shape before the count of columns a candidate narrows,
and `Equality` sorts before `Range` — so a candidate fixing one column beat one
fixing a column *and* bounding the next, which is a strict subset of it. Shape's
own doc says what it is: what a candidate is *trusted* to narrow when nothing
exact is known. That is a heuristic, and it was sitting above a proof. Moving the
proof up changes no decision this store made before, because every candidate that
narrowed more than one column was an equality, which the shape order already
preferred.

Worth recording beside the twelfth, because the two are the same mistake wearing
different clothes: there a true statement about the key encoding answered a
question the code never asked, and here a true statement about ranges in general
— *a range can be the whole table* — was applied to a range that provably cannot
be, because it is confined to the run its fixed values name.

| Absent | Why |
|---|---|
| `OFFSET` as a second spelling for `START` | one spelling for one thing |
| a **staged upload** — many commits building one file | this is what the ranged write in §6a is *not*: that one lands in a single commit and is bounded by what a transaction can hold. Building a large file across several needs a rule for what a reader sees between them, which is a visibility feature rather than a byte-offset one |
| a **streaming** backup answer | `BACKUP` answers with a value, so the file is materialised. `FROM` bounds it, and the real fix is an answer shape that streams — which is the wall a **whole-file** `READ` still meets even now that a ranged one exists, and worth crossing once for both. §7a |
| a backup of **one namespace** | the file is the log, and the log is the store; selecting part of it means replaying with a filter, which is a different reader and a different restore story. §7a |
| an index on a **later** field of a composite index, with nothing fixing the fields before it | the entries for one value of a later field are scattered across every value of the fields ahead of it, so reaching them means visiting each leading run's slice in turn. A different traversal of the same key order, and worth building when a read wants it rather than in anticipation. Its sibling — a range on a later field **under equalities fixing every field before it** — is no longer here: it walks one contiguous run and is served. §4 |
| `INFO FOR` on a **named** namespace, database or user's own account | the tenancy subjects report the **selected** namespace and database, because `USE` is where this store already answers "which tenancy", and a second way to name one is a second place for that check to be got wrong. A caller wanting another says `USE` and asks again. `INFO FOR USER` needs an owner, so a non-owner cannot read even their own grants — the smaller, safe rule while nothing has asked for the other; a self-form is a different permission question and would be built as one. §7c |
| an analyzer's own **definition** in a report — its name and the filters it applies | a field's report already names the analyzer attached to it, so a caller can see *which* one is used; what no subject holds is the analyzer itself. It is declared store-wide rather than under a namespace, a database or a table, so there is nowhere in these five subjects for its filter list to appear. A real gap and a small one: the catalog reader exists, and what is missing is the decision about where it belongs. §7c |
| a **schema dump** — a report rendered as the statements that would recreate it | `INFO FOR` answers with what the catalog holds, deliberately, because a renderer is a second description of the schema kept beside the first and the two drift. A dump wants that renderer plus a guarantee that replaying its output reproduces the store, which is a round-trip property worth testing rather than assuming. §7c |
| a range read whose **answer** is bounded, rather than only its fetching | the entries a range read holds at once are bounded (§4), and the records it answers with are not: every one is resolved and held before the caller sees the first. Measured, that is the larger half by far — bounding the entries took about four per cent off the peak of a fifty-thousand-record read, and what remains is roughly 1.4 KiB of resident memory for each record answered, against a stored record of a couple of hundred bytes. Where that goes has since been counted rather than inferred, and it splits in a way that matters: **887 bytes per record is the answer the caller holds, 72 bytes is everything the read builds and discards, and 865 of the 887 is the decoded record built with no store involved at all.** So the read does not copy the answer, and the cost is not in the reading — it is the decoded form, on both engines, to the byte. Of that 865, **793 appears the moment a record holds its first field** and the next seven fields add twelve bytes between them: a fixed allocation per record whose size is set by the value type rather than by the record. Two consequences, both load-bearing. Bounding the answer bounds how *many* of those are held at once and does not touch what one costs, so it is worth doing and is not the whole of this row. And an answer that streamed would pay the same constant on every record in flight, which makes the constant the one part of this that no answer shape fixes. Bounding the answer is not a storage change either — for **this** read. A plain `SELECT … FROM t LIMIT n` now hands its bound to the source, which stops early; a **conditioned** one cannot, because the condition that asked is re-tested above this layer, so the records the source produces are not yet the records the answer holds. Serving that case needs the planner and the executor consuming the answer as it arrives — the same answer-shape wall the streaming backup meets. §4 |
| a read that answers with **more records than fit in memory** | **a stated limit, not an unfinished feature.** An answer is a materialised value, so a read asking for fifty thousand records holds fifty thousand records: measured at 45.8 MiB, and its peak *is* its answer. No bound helps, because there is nothing to bound — a collector limits what is retained and this read retains everything, and there is no intermediate to spill: what the pipeline builds and discards was counted at 72 bytes a record, one vector spine and no second copy. The only thing that changes this number is an answer that streams, which is the same wall as the two rows above and is worth crossing once for all three. Until then a caller reading more than fits asks for it in pages, and the store's job is to make a page cheap — which `LIMIT` reaching the source now does. §4 |
| a **page of an order no index serves**, costing less than the table it pages | the sort no longer holds more than the page — `ORDER BY <unindexed> LIMIT 10` over fifty thousand records fell from 53 371 KiB to **45 822 KiB**, which is to the kibibyte what reading the whole table costs. That equality is the whole of what is left: the ordering stage now adds nothing, and the remaining cost is the **source**, which hands the executor a vector of every record it read before anything above it may look at one. So an ordered page is no longer worse than the read it is a page of, and it is not yet better. Making it better means the source yielding records as it finds them rather than collecting them — a change to what a read *is* rather than to what the order does with it, and the same answer-shape wall as the three rows above. Where an index holds the order there is nothing to page: the bounded descending read takes its bound from the index and costs 16 KiB. §5 |
| an **estimated row count** or a cost in a plan | it needs statistics about value distribution — how many records hold `city = 'london'` against `city = 'tromsø'` — which is maintained state whose staleness silently changes plans. A much larger decision than a selection rule, and one that wants a benchmark harness to justify it rather than an intuition. §7b |
| a digest on a file's metadata | worth having, and it is a *verification* feature: it belongs with the backup verifier rather than half here and half there |
| a content type on a file | the store holds bytes and has no opinion about them. It becomes worth carrying when something serves them over HTTP, which is where a content type is actually read |
| listing a bucket by prefix (`/photos/…`) | `SELECT * FROM media WHERE path LIKE '/photos/%'` is the question, and it needs the record's identity addressable as a value in a filter — which is a language feature about identities, not about files |
| a bucket whose bytes live somewhere else | the point of a bucket here is that everything is in one database — one backup, one identity, one feed. Pointing one at another service is additive and has its own credential and failure story |
| sharing identical chunks between files | deduplication needs a reference count, and a reference count is a derived value that must be exactly right or space leaks or data vanishes |
| a parameter where a **name** stands — a table, a field, an index, a namespace, a user, a role | a parameter is a value, and the rule that it is legal exactly where a literal is has one job: a caller who can supply a value must not thereby choose which column is read. A parameterised *name* is a second feature with a permission story of its own |
| a prepared statement — a parse kept under a name and bound many times | parameters make it *possible*: the parsed tree no longer holds any caller's values. What it needs beyond that is a catalog object with a lifetime and an invalidation rule for when the schema under it changes, which is a feature and not an optimisation |
| a parameter inside a stored expression — a field's `DEFAULT` | a stored expression is evaluated on every write that omits the field, so it belongs to no call and there is nobody to bind it. Refused where it is declared rather than surprising a write months later |
| a field grant on a **nested** route | a grant names a field of a table; `address.city` is a route into a value, and hiding one means rebuilding the object around it rather than dropping a key. Top-level only, so that a half-answer does not look like a whole one |
| a field grant that limits **writing** | `FIELDS` narrows reading, and a write replaces a whole record — limiting which fields a write may set is a merge semantic the language does not have |
| `LEFT`, `RIGHT` and `FULL` joins | a bare `JOIN` is inner, chosen so that these stay purely additive: an outer qualifier added later changes no statement already written |
| a join on anything but an equality, or on more than one pair | `ON a.x = b.y` is what an index can serve and what a map can be keyed by; a join predicate that is neither is a nested loop with a filter, which is the shape the equality was chosen to avoid |
| a join of more than two tables | the row is `{ left: …, right: … }`, so a third side is a shape decision (nest or flatten) and an order decision, and neither is worth taking before something needs it |
| `FETCH` through something already fetched, and cycles | one level, so the work is bounded by the references the answer already holds — one request, whatever their number — and a cycle is impossible rather than handled |
| a variable-length traversal (`->{1..3}`), a filter inside a traversal, shortest path | a written-out chain is a fixed number of steps the reader can count. A bound turns the walk into a search with a termination rule, a frontier and an answer that may or may not include the shorter paths — a language surface to design once rather than a clause |
| a traversal whose arrows change direction | `a->follows->users<-follows<-users` — "who follows somebody ada follows" — is a real question, and a useful one. It needs a rule for what each step's anchor *is* when the direction turns, and a chain where every arrow reads the same way is the one a reader can follow without one |
| a traversal that answers with the path rather than its end | the answer would be a list of records rather than a record, which is a shape for rows and not for records — the same wall the join met, and the same milestone |
| several distinct edges between one pair in one table | an edge is identified by its endpoints, which is what makes `RELATE` idempotent; one edge table per relation is the spelling |
| stemming | a filter, and one could be added under the rule above; a correct stemmer is a language-specific artefact rather than a hundred lines, and a bad one is worse than none |
| n-grams, so `MATCHES` never answers a substring question | index size proportional to text length × (max − min), paid on every write; Q-31 holds the measurement that would decide it |
| phrase queries (`'"ada lovelace"'`) | they need positions in the postings and a second matching rule |
| layers in the vector index | a hierarchical graph assigns each node a random level, and a random level is what a store whose index entries are *derived rather than logged* cannot have — two replicas would build different graphs from one log. A level derived from a hash of the record id is the right shape when the layers earn their cost; the key already reserves the byte. |
| a filtered nearest-neighbour read | the graph answers a distance question and knows nothing of a `WHERE`, so combining them needs either over-fetching by an unknown factor or a filtered walk |
| highlighting, fuzzy matching, phrase and proximity queries | each needs postings to carry more than membership — offsets for a highlight or a phrase, an edit automaton for fuzziness — which is a different index rather than a bigger one. Ranking itself is built: see [Ranking](#ranking) |
| per-index `k1` / `b`, per-field weighting | tuning knobs nobody can yet turn responsibly: this project has no labelled relevance set to measure a different value against, and a knob chosen without one is a guess with a syntax |
| a **function applied to each reached value** | `array::len(tags[*])` is refused because it has two answers — the function over the collected values, or the function applied to each of them. The second is a mapping operator and deserves its own spelling rather than being what a parenthesis happens to mean. §3 |
| a **`UNIQUE` multikey index** | two readings — no two records sharing an element, or a record's own elements being distinct — which refuse different writes. It needs a spelling that says which, not a default. §4 |
| a **multikey index over two multi-valued routes** | the entries would be one per pair of elements, paid on every write. Worth building when somebody has the query that needs it, so the cost is paid for a reason. §4 |
| `[*]` on the right of a comparison, or twice in one route | the first is the same question written backwards, and a second spelling for one thing is what this language keeps refusing; the second composes two relations and needs a rule for what that means |
| declaring a type on a path | `DEFINE FIELD address.city TYPE string` needs a rule for what declaring a leaf says about its parents, and `SCHEMAFULL` would have to mean "no undeclared path" rather than "no undeclared field" |
| `HAVING` | a filter over groups is a second filter position with its own scoping rule — it sees folds where `WHERE` does not — and is worth its own milestone rather than an afterthought. A fold written in a `WHERE` is refused by name rather than as a stray token, so the message says which of the two the author wanted |
| `DISTINCT` | it is `GROUP BY` over the projection with no fold, and one spelling for one thing. **Both positions it is asked for already have a spelling, and each is now pinned by a case rather than by this sentence:** the distinct set is `SELECT city FROM people GROUP BY city` (and `GROUP BY city, name` for the distinct pairs), and how many there are is that grouping counted — `SELECT count(*) AS cities FROM (SELECT city FROM people GROUP BY city LIMIT 100)`. The bound on the inner read is not friction added here; it is the rule every materialised source obeys (§5) |
| a declared retention policy, enforced in the background | a policy is `DELETE FROM … WHERE`, run by an operator or a schedule; a declared one needs a job runner and a decision about when it runs, and hiding that in a table is how a store deletes something at three in the morning that nobody expected |
| `LIMIT` on a delete | a retention run is one commit, so bounding one means deciding what a half-applied policy means |
| filling a window that has no records | grouping answers with the groups the data has; filling a gap means knowing the range the caller meant, which the statement does not say |
| a sub-second window | `time::bucket` takes a whole number of seconds; the nanosecond remainder is a different arithmetic and is refused rather than rounded |
| a spilling aggregate | **narrowed by measurement, and by a smaller part of it than this row used to claim.** A fold holds one accumulator per group and not one value per record, so what a fold costs is set by how many groups there are and not by how many records went into them — every fold this store has is computable one value at a time. What is still built in memory is the **group map**, so grouping by a field with more distinct values than fit needs a spill, and so would a fold that cannot be computed incrementally, of which there is none. Note that this did not move a grouping read's peak: the fold frees each record as it folds it, so the peak is the source's materialised vector, exactly as it is for the two rows above |
| user-defined functions | a stored function is a catalog entry with its own lifecycle, permissions and replication story |
| a separate `NOT NULL` | `REQUIRED` covers absence and null together; splitting them is additive |
| a default on a whole table | a different feature wearing a similar word |
| `\|\|` as a second spelling for concatenation | `string::concat` says it, and a second spelling for one thing is a decision to take once rather than by accident |
| `BETWEEN` | `a >= x AND a <= y` says it, and one spelling for one thing |
| three-valued logic | §5 — comparison answers true or false, and `= NONE` / `= NULL` say what `IS NULL` would |
| row-level security — a grant that names *which records* rather than which table and fields | a table grant refuses and a field grant edits; a row grant would have to *filter*, which means every read carries a predicate the caller did not write and every count answers about a set they cannot see. That is a different feature from either, and the one where getting it subtly wrong leaks by arithmetic |
| tokens, or a session that outlives a request | a token is a second credential with its own lifetime, revocation and storage |
| `SIGNIN` as a statement | deliberate, and stated above rather than missing |
| rate-limiting a signin | Argon2 is slow on purpose, which is most of the defence; a lockout policy has its own decisions about who it locks out |
| an assertion reading a **second record** | a comparison against another field of the *same* record is built (§4) — the record is already in hand. Reading another one is a different thing: the verdict would stop being a function of the record being written, so a replica would have to reproduce a read, and the refusal would report the existence of a record the writer never named |
| a **computed assertion** (`string::len($value) > 3`) | the useful ones are pure, and the vocabulary could take them — but a function set that is pure *today* is a property somebody would have to re-establish every time the set grows, so the door opens with a marked-pure function set rather than by trusting the current one. §4 |
| changing a declared type in place | `DROP FIELD` then `DEFINE FIELD` re-checks every row through the one path; a migration primitive is its own work |

## 9. What is fixed here, and what can still move

| Decision | Status |
|---|---|
| TessariQL is the only way structure is created | **contract** (ADR-0003) |
| A space is a table of single values; workspace = database | **contract** (ADR-0010) |
| Key-value verbs are expressions, and compose at one snapshot | **contract** |
| `NONE` and `NULL` are distinct literals | **contract** — the storage layer keeps them apart |
| An edge is a record, and traversal is an index read | **contract** — no separate graph keyspace, so edges get MVCC, transactions, replication and the schema check without any of them being built again |
| An edge is identified by its endpoints | fixed for this milestone; an explicit-id form would be additive |
| A traversal is steps written out, and every arrow points the same way | **contract** — the number of hops is readable from the statement, so a walk cannot run away and a reader can count what it costs |
| A walk's landing is deduplicated by record | **contract** — answers are keyed by record, so a record two paths reach is one answer |
| Every table a walk passes through is asked about | **contract** — reaching a record through an edge is not a way around its table's grant, at any hop |
| A fold answers once per group, and a grouped read answers only with keys and folds | **contract** |
| `GROUP BY` is optional; folds without one make a single group | **contract** |
| Every fold but `count(*)` passes over absent and null | **contract** |
| A fold is an expression, so it composes | **contract** — one shape for a fold in the tree, and `mean(age) * 2` is arithmetic over the value it produced |
| A fold's value is constant within its group | **contract** — computed once per group and substituted, so the composition is evaluated by the same code that evaluates any other arithmetic |
| A fold stands in a projection and nowhere else | **contract** — a filter and an ordering are per record, and what a fold there would mean is `HAVING` |
| Sum over nothing is `0`; mean over nothing is `NONE` | **contract** |
| A sort is the value system's order, with `NONE` below `NULL` below every value | **contract** — a sort must place every row, where a comparison may decline to |
| Ties are broken by record identity | **contract** — what keeps an added index from reordering equal rows |
| A bounded order is taken from the index that holds it — a single-field index on the ordered field, **or a composite whose leading field it is** — **descending always and ascending over a `REQUIRED` field** | **contract** — the sort order and the index order are one order, so the walk is the answer rather than a computation of it. The directions differ only in the absences: a record with no value has no entry, and it sorts first, so ascending is admitted exactly where a declaration says there are none. A route *below* a required field is refused — `REQUIRED` promises a value for the field, not for what lives inside it |
| An order taken from an index drains the tie group straddling the bound, **except ascending over an index the order names every field of** | **contract** — cutting at the bound would take the wrong members of the group, all of them real records. The exception is exactly where the group's inner order is already the answer's: a single-field key is its value followed by the record's identity, so a *forward* walk yields ties ascending by identity and there is nothing to correct. Everything else drains — descending because walking backwards reverses that inner order, and a **composite** in either direction because the entries sharing one leading value are ordered by the *next* indexed field rather than by identity |
| A **descending** index that cannot fill the bound hands the read back to the scan | **contract** — the records below its last entry are the ones it does not hold, and the path reported is the one that ran. **Ascending, an exhausted index has answered the whole table**, because it is admitted only where every record has an entry, so a short answer is a complete one |
| An order is served only from the committed tail | **contract** — an entry carries no version, so at an older snapshot a changed record sits under a value the reader cannot see, and the answer comes back in the wrong order rather than short |
| An order is not served over a field the caller's grant excludes | **contract** — a field permission removes the field before anything reads it, and an order taken from the index would sort by what the projection hides |
| `START` and `LIMIT` apply after ordering, always | **contract** — and it is now a cost rule as well as a meaning one. A read with no ordering may have its bound handed to the source, which then stops early; a read with one may not, because the first *n* found and the first *n* in that order are different records. The refusal is what keeps this row true, so an optimisation that dropped it would not be faster — it would answer a different question. A read *with* an ordering is bounded in a second place instead: the sort keeps only the records that can still reach the answer, so it visits every record and holds *n*. Both are one rule read twice — a bound may move anywhere that nothing between it and the answer can change how many records there are, and only the source has such stages above it |
| `ORDER`, `BY`, `ASC`, `DESC`, `LIMIT`, `START` are contextual, not reserved | **contract** — reserving a word takes a name away from data that exists |
| The analyzer belongs to the field, not to the index | **contract** — an analyzer on an index lets adding one change an answer |
| `MATCHES` asks about words, `LIKE` about characters, `CONTAINS` about membership | **contract** |
| A bucket is a table and a file is a record in it | **contract** (ADR-0011) — what makes linking, relating, listing, granting, subscribing and backing up files cost nothing |
| A file's identity is text | **contract** — a chunk is addressed by the path and its ordinal, so an integer identity would collide with the text of that integer |
| A bucket's records are written by `PUT` and never by hand | **contract** — metadata that can be written is metadata that can lie about bytes |
| `PUT` is one commit | **contract** — a file is whole or absent, never partial |
| A parameter is legal exactly where a literal is, and nowhere a name is | **contract** — the whole safety argument, and it is checkable by reading the grammar rather than by auditing the places a value is used |
| A record's **id** is a value, so a parameter stands there; its table is a name, so one does not | **contract** — the line between what a caller may supply and what they may choose |
| A parameter is replaced after parsing and before the first statement runs | **contract** — so a supplied value can never be read as grammar, and a script with an unsupplied one writes nothing at all |
| Values are supplied as values, not as text | **contract** — `36` and `'36'` are different questions, and a caller must not have to know how this language would have read a string |
| A supplied value under an unused name is accepted | **contract** — reusing one set of values across two scripts is not a mistake this store can see |
| Several terms mean all of them | **contract** |
| A field with no analyzer holds no terms and matches nothing | **contract** |
| `REQUIRED` means present and not null | **contract** |
| A bounded descending order under a `WHERE` is taken from the index that holds the order, and gives it up rather than answering short | **contract** — the index narrows and the condition decides, so the walk continues until the **bound is filled by records that survive the condition**, not until the bound is filled by entries. Past a stated multiple of the bound the condition is too thin for the order to be worth serving that way and the read takes the scan it would have taken anyway. The ceiling bounds the cost and never the answer |
| An index over several fields serves a condition on the **leading run** of them the condition fixes to values | **contract** — the field order decides which reads it can serve, and `last = 'x' AND first = 'y'` on `(last, first)` is one lookup rather than a lookup on `last` and a re-test of `first`. The *lookup* run stops at the first field the condition does not fix with an equality, so a `LIKE` on the second column leaves the lookup at one column and is re-tested like any other clause. A **range** on the field immediately after the run is the exception, and it is served: the fixed values name one contiguous run of entries and that run is already ordered by the very field being bounded, so the bounds are a bound on a walk rather than a filter over the run. `at = 20 AND tag >= 1950 AND tag <= 1959` on `(at, tag)` reads the ten entries the bounds name and not the day's hundred |
| A `UNIQUE` composite promises a ceiling of **one** exactly when the condition fixes every one of its fields, and none otherwise | **contract** — uniqueness is over the whole tuple, so fixing only the first promises nothing: one `last` may have any number of `first`s |
| Among candidates with no ceiling, the one narrowing more of its index's columns wins — **before** shape is consulted | **contract** — a proof rather than an estimate: the entries matching two fixed fields are a subset of those matching the first alone, and the entries a range keeps are a subset of the run it walks, whatever the data holds. The shape order below it says what a candidate is *trusted* to narrow when nothing exact is known, which is a heuristic, and a proof outranks a heuristic. That ordering is load-bearing rather than tidy: with shape on top, `a = 1 AND b > 2` on `(a, b)` lost the range candidate to the equality one, because `Equality` sorts before `Range` — the wider candidate winning on a guess. An equal count still falls through to shape, and an equal shape to the order the conjuncts were written |
| `EXPLAIN` reports the plan that actually runs, and needs the read's own permission | **contract** — a second planner would disagree, and a plan is metadata about a table |
| A plan carries no number the store cannot know | **contract** — no estimated rows, no cost |
| `INFO FOR` reports what the caller could have found out anyway | **contract** — the change feed's rule applied to a description instead of to records: what is granted is listed, what is not is absent, and a named table is refused |
| A field grant hides a field's declaration **and** the indexes that project it | **contract** — an index is named after the values it projects, so listing one names them |
| `INFO FOR USER` refuses rather than narrows, and needs an owner | **contract** — a grant list with rows quietly removed reads as the whole of what that user can reach |
| A report never carries a password hash | **contract** — the stored definition does, so the report is built field by field and not from it |
| `INFO` is reserved; `FOR` and `STORE` are contextual | **contract** — a statement's leading word must be a keyword, and nothing else here must be |
| A report's names are in name order | **contract** — an order that depends on how a script was written is two answers to one question |
| `UPDATE` replaces with a value and changes with `SET`, and both touch one record | **contract** |
| Every right-hand side of a `SET` sees the record as it was | **contract** — otherwise a statement's meaning depends on clause order |
| Assigning `none` removes the field; `null` is a value and stays | **contract** |
| A route the record does not have is refused, never created | **contract** — the store does not write structure nobody asked for |
| `START` and `LIMIT` on a file are the row rule over bytes, and a range past the end is empty | **contract** |
| A ranged write is one commit, and leaving `START` out replaces the file | **contract** — an offset writes at it and keeps what lies beyond |
| A write that would leave a hole is refused, never zero-filled | **contract** — the store does not invent bytes nobody wrote |
| `BACKUP` answers with the log as the file the verifier reads | **contract** — a surface that rendered it instead would be a second format |
| A backup is fixed at the log's tail when it began | **contract** — a write landing mid-backup is outside it, and the header says where "outside" starts |
| A backup needs an owner, and no grant can permit one | **contract** — it is every table at once, so an empty list of named tables must not read as permission |
| `ASSERT` constrains a present, non-null value, and `REQUIRED` is the one constraint about absence | **contract** — otherwise `REQUIRED` would mean two things depending on what stood beside it |
| An assertion is a closed vocabulary and what falls outside it is refused where it is written | **contract** — validation must stay a pure function of the record, and "happens to be pure" is not a property to re-establish forever |
| `$value` is the only parameter an assertion may name | **contract** — a declaration belongs to no call, and the store is what binds this one |
| `ASSERT` and `WHERE` compare by the same function | **contract** — two implementations would eventually make a write one node refuses and another accepts |
| A default fills only what a write leaves out, and never reaches backwards | **contract** |
| A default is a value-position expression, checked when it is declared | **contract** |
| A filter is a condition, and a condition is a boolean | **contract** |
| Numeric kinds promote int → decimal → float | **contract** |
| Division produces at least a decimal | **contract** — truncating integer division is a wrong number that looks right |
| Overflow and division by zero are failures | **contract** |
| A vector is an array of numbers, not a type of its own | **contract** |
| Cosine is a distance, so smaller is nearer | **contract** |
| What has no distance is `+∞`, so a bounded read never mistakes it for a neighbour | **contract** |
| `ORDER BY` takes an expression | **contract** |
| A function is added only when the language cannot already say it | **contract** — the rule that keeps the surface from growing by association |
| An absent or null argument makes a call answer `none` | **contract** — except `type::of`, which asks about the value rather than computing from it |
| A cast produces the kind it names or refuses, never something near it | **contract** — an absent argument still answers `none`, so a cast narrows a read; a value that is there and does not convert fails it, because a filter silently returning fewer rows is the one wrong answer nothing downstream detects |
| Of two candidate behaviours for a collection function, the one the other can be written from wins | **contract** — `array::reverse(array::sort(x))` is why there is no `sort_desc`, and `array::distinct` keeps the first occurrence because sorting throws away an order nothing recovers; `object::keys` and `object::values` correspond position by position; positions count characters, not bytes |
| A date read from an instant is UTC, and `time::second` is the second of the minute | **contract** — an instant has no zone, and `time::unix` is the other question; `time::from_unix` refuses a fraction because `math::round` says which second was meant, while `time::unix` drops a remainder because nothing else could say that conversion |
| Reading no record and being safe to evaluate once are two properties, and a function says which it has | **contract** — every record-independent expression is evaluated once above the records, so `rand::uuid()` is asked again per record while `time::now()` is not; the two impure functions want opposite treatment, and a single rule about impurity would get one of them wrong in silence |
| A generated identifier is a real UUID, and a machine that cannot read its randomness source refuses | **contract** — the canonical text is parsed by something else, and a fallback to a clock or a counter is how a collision arrives on the day two containers start from one image |
| A fold says whether its answer can be computed as the records go past | **contract** — `count`, `sum`, `mean`, `min`, `max`, `variance` and `stddev` cost a running value whatever the group's size; `median` and `collect` hold the group, and the ceiling on a held read is decided by that answer rather than by the presence of a fold |
| `variance` and `stddev` are the sample forms, and the population form is arithmetic | **contract** — `variance(x) * (count(*) - 1) / count(*)`; one name exists rather than two because the language already says the other, and over fewer than two numbers both answer `NONE` because zero would be a claim |
| `median` answers exactly rather than returning the value it selected | **contract** — `3`, `3.0` and the decimal `3.0` are one value in this system and three answers on the wire, so "the middle value as written" would be decided by a sort rather than by the data |
| `collect` over nothing is `[]`, not `NONE` | **contract** — the rule `sum` follows: an answer every caller writes `?? []` after is the wrong answer |
| A refusal names an escape that works | **contract** — a `LIMIT` bounds what a fold answers with and not what it reads, so a collecting read is told to bound its source instead; a ceiling whose stated escape does not lift it is worse than one with no advice |
| A digest takes text and answers lowercase hexadecimal text | **contract** — the comparison `crypto::sha256(body) = $expected` is the reason the function exists, and hashing any value's rendering would promise one digest for three numeric kinds that compare equal |
| No function exposes the credential hasher to a query | **contract** — SHA-2 is fast by design, which is what a stored password must not be |
| Anything computed in a projection needs `AS` | **contract** |
| Comparison is the value system's declared order, including across types | **contract** — a comparison disagreeing with the order its index is stored in is an answer that changes when an index appears |
| An ordered comparison against `NONE` or `NULL` is false | **contract** — they are the absence of a value, not a small one |
| `= NONE` and `= NULL` are the two questions `IS NULL` would blur together | **contract** |
| An index narrows a conjunct; the whole condition still decides | **contract** — what keeps an index from changing an answer |
| `[*]` makes a path denote several values, and each context has its own rule for several | **contract** — one denotation, three rules, rather than one syntax with three meanings |
| A comparison over several holds when any of them does | **contract** |
| A single value is not an array of one | **contract** — the rule `CONTAINS` already follows |
| A projection over several answers with all of them, in route order, duplicates kept | **contract** — a projection collects; deduplicating or sorting is a different statement |
| A projection over several always answers, and an empty reach answers `[]` | **contract** — a relation is total, so zero values is an empty collection rather than an absence; an empty array, an absent field and a single value therefore answer alike |
| `[*]` stands as a whole projected value and not inside a larger expression | **contract** — the alternative has two readings and picking one silently teaches the other by surprise |
| An index over `[*]` keeps one entry per reached value, and a repeated element is one entry | **contract** — what a multikey index *is* |
| `tags` and `tags[*]` are different routes, and an index is matched by exact route equality | **contract** — the matching rule is what keeps a whole-array index from answering a question about elements |
| A record answers once however many of its elements match | **contract** — answers are keyed by record, so twice is wrong rather than verbose |
| A build is authoritative, so `REBUILD INDEX` is `DEFINE INDEX` run again | **contract** — an index's entries are made to *be* what the rows imply rather than added to what is there, which is why a rebuild needs no second path and no log shape of its own |
| A rebuild is a statement, never the store's own decision | **contract** — replicas that each rebuilt on their own reckoning would answer one approximate question differently, and differ in silence |
| A rebuilt index is a function of the rows, not of the order they arrived in | **contract** — the rows are read in record-id order, so two replicas that received them differently still agree |
| A bare name is a path in a condition and a table in a value position | **contract** |
| A projection is named by the last step of its path | **contract** — the alternative puts a delimiter inside a field name, which no path can then address |
| A projected path reaching nothing omits its field rather than answering `none` | **contract** — the same reason `NONE` and `NULL` are different literals |
| A projection never changes the access path | fixed for this milestone; a covering read is a planner decision |
| A path names a value inside a record, and one that reaches nothing matches nothing | **contract** — the same answer a missing top-level field has always given, so records of differing shapes share a table without the index or the filter having an opinion about it |
| An index answers the exact path it projects | **contract** — an index on `address.city` is not one on `address`, for the same reason an index on `(a, b)` is not one on `a` |
| A declared type constrains a present, non-null value | **contract** — absent is unconstrained, null is allowed, and neither is a hole to be closed later without changing what existing scripts mean |
| A schema is enforced by the store, not by the session | **contract** — a check the session owned would be one every other writer bypasses |
| A declaration constrains the rows that predate it, in its own commit | **contract** — the alternative is a constraint that can be declared and not hold |
| The decimal marker | fixed; dropping it would silently make money a float |
| The three access paths | fixed for this milestone; a planner changes how one is chosen, not what exists |
| Verb spellings and clause names | movable while the language is unimplemented |
| Everything in §8 | additive |
