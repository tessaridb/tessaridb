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
`--param <name>=<value>` with the value written as bgvQL, repeatably. The console
spells it in bgvQL rather than JSON because what the console prints already
pastes back into the next statement, and the value is parsed **in isolation**, so
`--param x='1; DROP TABLE users'` is refused as a value rather than smuggled in
as a statement.

Values are supplied as values and not as text, so `$age` bound to the number
`36` and `$age` bound to the string `'36'` ask different questions — the caller
never has to know how this language would have read a piece of text.

An index still serves a read whose value came in this way. Replacement happens
before anything plans the read, so `WHERE name = $who` is planned exactly as
`WHERE name = 'ada'` is.

### Naming a value inside a record

A record payload is a tree: an object may hold objects and arrays, and those may
hold more, with no depth limit. A **path** is how something inside it is named.

Every fenced example in this document is executable bgvQL, so a path is shown in
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
| a projection | would answer with all of them — **not built**, see §8 |
| an index | would keep one entry per element — **not built**, see §8 |

Only the first exists, and the other two are refused **by name** rather than
half-built: a `[*]` in a projection, an ordering, a group key, a function's
argument, a `FETCH` route or an index's fields is an error that says `[*]` is
what it does not yet handle.

**No index serves a comparison over several.** An ordinary index over `tags`
holds one entry for the whole array, so answering `tags[*] = 'urgent'` from it
would answer a question about elements with an answer about arrays — and an index
in this store changes what a read costs and never what it answers. Such a read
takes the scan and the access path says so.

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
```

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
because that is bulk work whose cost belongs where a caller can see it.

### What a table declares about its fields

A table is schemaless until something is declared on it, and stays schemaless
about everything nobody declared. `DEFINE FIELD` names one field and what it may
hold:

| Written | Meaning |
|---|---|
| `TYPE string` | a present, non-null value in that field must be text |
| `TYPE int` / `float` / `decimal` | one numeric form, kept apart |
| `TYPE number` | any of the three |
| `TYPE any` | anything — a declaration that constrains nothing, so that a field can be *declared* without being narrowed |

Every one of the fifteen literal types of §3 is a spelling, plus `any` and
`number`. Five of them — `table`, `set`, `range`, `datetime`, `uuid` — are
reserved words elsewhere, and are read as type names here for the same reason a
field name inside an object literal may be reserved: after `TYPE`, nothing but a
type name can appear.

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
that was meant. A `SCHEMAFULL` table refuses it instead. The flag is fixed when
the table is defined; changing it on a populated table is a migration, and the
honest spelling of one is a drop and a redefinition, which re-checks every row.

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
SELECT * FROM users ORDER BY name;
SELECT * FROM users ORDER BY city, joined DESC START 20 LIMIT 10;
SELECT count(*) AS n FROM users;
SELECT city, count(*) AS n, mean(age) AS average FROM users GROUP BY city;

UPDATE users:1 = { name: 'ada', email: 'ada2@example.com' };
DELETE users:1;
```

`CREATE` and `UPDATE` are not two spellings of one verb. **`CREATE` over a
record that already exists is refused**, and **`UPDATE` over one that does not
exist is refused**. The alternative — either verb quietly doing the other's job —
loses a record with nothing anywhere to notice, and `SET` already exists for the
caller who means "whatever is there, replace it".

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

**What it costs, stated rather than measured later:** one point read per
*distinct* reference across the whole read. Distinct, because a read resolves at
one snapshot and two reads of one address at one snapshot must answer the same
thing — so a hundred posts by three authors is three reads. Turning many point
reads into one batched request is a planner decision and is not made here.

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
access path an answer reports (`index` or `scan`) says which kind of read ran; it
does not yet say *which index*, and naming it wants an `EXPLAIN` of its own.

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
DELETE FROM readings WHERE at < datetime '2026-01-01T00:00:00Z';
```

**`FROM` is what tells the two apart, and it is required.** `DELETE readings
WHERE …` would read as a table name where an identity belongs, and a statement
that removes rows should not be one word away from a typo.

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

Ordering does not become an index read. An ordered index could serve
`ORDER BY email LIMIT 10` without sorting anything, and that is not built — the
same deferral `<` and `>` already carry. The path reported is the one that ran.

`ORDER`, `BY`, `ASC`, `DESC`, `LIMIT` and `START` are **not reserved words**.
They shape a clause where nothing else can stand, so nothing is ambiguous, and
reserving them would take six perfectly good names away from data that already
exists — `SELECT * FROM order ORDER BY by LIMIT 1` is a legal statement.

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

| Group | Functions |
|---|---|
| `string` | `len` (characters, not bytes) · `lower` · `upper` · `trim` · `concat(a, b)` |
| `array` | `len` · `first` · `last` |
| `math` | `abs` · `floor` · `ceil` · `round` (half away from zero) |
| `time` | `now()` · `bucket(instant, width)` — the start of the window an instant is in |
| `type` | `of(value)` — the type's name, as §3 spells it |
| `vector` | `cosine(a, b)` · `euclidean(a, b)` · `dot(a, b)` |
| `search` | `score(field, 'query')` — see [Ranking](#ranking) |

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
(§5); and a fifth, **traversals longer than one hop**, disproved by
`SELECT * FROM users:1->follows->users->follows->users` (§4a) — the last of which
left three narrower rows behind it, because building the half that was asked for
showed exactly what the other half would take.

| Absent | Why |
|---|---|
| `OFFSET` as a second spelling for `START` | one spelling for one thing |
| a byte range on `PUT` or `READ` — writing or reading part of a file | one commit is what makes a file whole or absent, and a partial write needs a rule for what a reader sees between two of them. The read half is cheaper than the write half and will land first |
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
| joining a table to itself | two records under one name is not a row anybody can read, and telling them apart needs aliases — a language surface to design once rather than a clause |
| a join on anything but an equality, or on more than one pair | `ON a.x = b.y` is what an index can serve and what a map can be keyed by; a join predicate that is neither is a nested loop with a filter, which is the shape the equality was chosen to avoid |
| a join of more than two tables | the row is `{ left: …, right: … }`, so a third side is a shape decision (nest or flatten) and an order decision, and neither is worth taking before something needs it |
| `FETCH` through something already fetched, and cycles | one level, so the work is one point read per reference and a cycle is impossible rather than handled |
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
| a **projection** over `[*]` | it has to answer what an empty reach projects — an absent field or an empty array — and those are different claims about a record. §3 |
| a **multikey index** over `[*]` | one record produces several entries, so "remove the entry for the value it replaced" becomes "remove the entries", and an element leaving an array must remove exactly its own. `UNIQUE` over one is a second question — no two records sharing an element, or a record's own elements being distinct — and is refused until it is answered. §3 |
| `[*]` on the right of a comparison, or twice in one route | the first is the same question written backwards, and a second spelling for one thing is what this language keeps refusing; the second composes two relations and needs a rule for what that means |
| declaring a type on a path | `DEFINE FIELD address.city TYPE string` needs a rule for what declaring a leaf says about its parents, and `SCHEMAFULL` would have to mean "no undeclared path" rather than "no undeclared field" |
| `HAVING` | a filter over groups is a second filter position with its own scoping rule — it sees folds where `WHERE` does not — and is worth its own milestone rather than an afterthought. A fold written in a `WHERE` is refused by name rather than as a stray token, so the message says which of the two the author wanted |
| `DISTINCT` | it is `GROUP BY` over the projection with no fold, and one spelling for one thing |
| a declared retention policy, enforced in the background | a policy is `DELETE FROM … WHERE`, run by an operator or a schedule; a declared one needs a job runner and a decision about when it runs, and hiding that in a table is how a store deletes something at three in the morning that nobody expected |
| `LIMIT` on a delete | a retention run is one commit, so bounding one means deciding what a half-applied policy means |
| filling a window that has no records | grouping answers with the groups the data has; filling a gap means knowing the range the caller meant, which the statement does not say |
| a sub-second window | `time::bucket` takes a whole number of seconds; the nanosecond remainder is a different arithmetic and is refused rather than rounded |
| a spilling aggregate | groups are built in memory; a store that must aggregate more than fits needs a spill, and that is a measurement away rather than a guess away |
| user-defined functions | a stored function is a catalog entry with its own lifecycle, permissions and replication story |
| a separate `NOT NULL` | `REQUIRED` covers absence and null together; splitting them is additive |
| a default on a whole table | a different feature wearing a similar word |
| `||` as a second spelling for concatenation | `string::concat` says it, and a second spelling for one thing is a decision to take once rather than by accident |
| a range over the first column of a **composite** index | an index whose field list is one path serves a range today; a prefix of a multi-column one needs its own bound construction and its own equivalence test |
| a descending bounded scan | `ORDER BY` sorts what the range produced; making the scan itself run backwards is a saving that needs the `LIMIT` pushed into it |
| `BETWEEN` | `a >= x AND a <= y` says it, and one spelling for one thing |
| three-valued logic | §5 — comparison answers true or false, and `= NONE` / `= NULL` say what `IS NULL` would |
| row-level security — a grant that names *which records* rather than which table and fields | a table grant refuses and a field grant edits; a row grant would have to *filter*, which means every read carries a predicate the caller did not write and every count answers about a set they cannot see. That is a different feature from either, and the one where getting it subtly wrong leaks by arithmetic |
| tokens, or a session that outlives a request | a token is a second credential with its own lifetime, revocation and storage |
| `SIGNIN` as a statement | deliberate, and stated above rather than missing |
| rate-limiting a signin | Argon2 is slow on purpose, which is most of the defence; a lockout policy has its own decisions about who it locks out |
| `ASSERT` | it needs an expression **evaluated where validation lives**, and validation lives on the store's apply path so that a replica reaches the same verdict without anything being sent. The store sits below the language and cannot parse or evaluate bgvQL. The fix has a shape — a guard the store calls and the session implements — and inverting the layering is not it. |
| changing a declared type in place | `DROP FIELD` then `DEFINE FIELD` re-checks every row through the one path; a migration primitive is its own work |

## 9. What is fixed here, and what can still move

| Decision | Status |
|---|---|
| bgvQL is the only way structure is created | **contract** (ADR-0003) |
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
| `START` and `LIMIT` apply after ordering, always | **contract** |
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
| Anything computed in a projection needs `AS` | **contract** |
| Comparison is the value system's declared order, including across types | **contract** — a comparison disagreeing with the order its index is stored in is an answer that changes when an index appears |
| An ordered comparison against `NONE` or `NULL` is false | **contract** — they are the absence of a value, not a small one |
| `= NONE` and `= NULL` are the two questions `IS NULL` would blur together | **contract** |
| An index narrows a conjunct; the whole condition still decides | **contract** — what keeps an index from changing an answer |
| `[*]` makes a path denote several values, and each context has its own rule for several | **contract** — one denotation, three rules, rather than one syntax with three meanings |
| A comparison over several holds when any of them does | **contract** |
| A single value is not an array of one | **contract** — the rule `CONTAINS` already follows |
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
