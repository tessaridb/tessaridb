# The value system

Normative. This document says what a value *is* in this store, how values
compare, and what bytes they become. `docs/key-grammar.md` is its counterpart
for keys.

The two are separate on purpose: keys sort, payloads do not. A key's encoding is
built so that byte order equals logical order, because that is what makes a range
scan correct. A payload is read, never compared byte by byte, so its encoding is
built for exactness and for surviving a format change instead.

---

## 1. The fifteen types

Fixed by the milestone-1 scope.

| Type | Holds |
|---|---|
| `none` | nothing, and the field is not there |
| `null` | nothing, and the field is there |
| `bool` | true or false |
| `number` | an integer, a binary float, or an exact decimal |
| `string` | text |
| `bytes` | opaque bytes |
| `duration` | a span of time, possibly negative |
| `datetime` | a point in time |
| `uuid` | sixteen bytes |
| `table` | a reference to a table |
| `record` | a reference to one record: a table and an identity |
| `array` | an ordered sequence |
| `object` | a map from field name to value |
| `range` | a span between two values, either end open or closed |
| `set` | a collection with no duplicates and no significant order |

Three types exist in the wider design and are deliberately absent: `geometry`,
`file` and `regex`. Each is named here rather than merely missing, so that its
absence reads as a decision instead of an oversight. `file` needs a bucket
subsystem that does not exist; `regex` is a query-language concern before it is a
storage one; `geometry` is deferred with the rest of its milestone.

## 2. Absent and null are different values

`none` means the field is not there. `null` means it is there and holds nothing.

Collapsing them is the most common shortcut in a value system, and it costs the
ability to distinguish *"we never looked"* from *"we looked and found nothing"*.
Those are different facts, and a store whose consumer is a memory for agents
cannot afford to confuse them.

The distinction survives storage — they encode to different bytes — and it
survives deletion: a deleted record has **no payload at all**, which is a third
thing again from a record whose payload is `none`.

## 3. Ordering

The order is **total**: any two values compare, and comparison is antisymmetric
and transitive. It has to be, because an index holds whatever a column holds, and
a range scan over a column with two types in it has to mean something.

### Across types: by declared rank

```
none < null < bool < number < string < bytes < duration < datetime
     < uuid < table < record < array < object < range < set
```

This order is **contract**. Reordering it reorders every index that holds a
mixed column, which makes it a data migration and not a refactor.

### Within a type: by the type's own order

Text by code point, bytes lexicographically, arrays element by element, objects
by their fields in name order, and so on.

### Numbers are the exception, and the exception is the point

All three numeric kinds share one rank and compare **semantically**. `1`, `1.0`
and `1.00` are one value; `1 < 1.5` is true whichever kinds the two sides arrive
as.

Ranking the kinds apart would have been simpler to write and total as well, and
it would have made `1 < 1.5` **false** — while every test that used a single kind
kept passing. That is the shape of failure this rule exists to prevent.

The rule in full:

- Integers and decimals compare exactly.
- A finite float compares by converting to a decimal. Within a decimal's
  precision this is exact; a float needing more digits than a decimal carries
  compares by its rounded value, and that is the one approximation in the type.
- A finite float too large for any decimal orders by its sign, which is
  unambiguous because nothing representable as a decimal reaches that magnitude.
- `-∞` is below every number, `+∞` above every number, and not-a-number above
  `+∞`. Not-a-number has to go somewhere for the order to be total; putting it at
  one end keeps it out of the middle of a range scan.

Two values are normalised on the way in so that equality and ordering agree:
negative zero becomes zero, and every not-a-number becomes one canonical
not-a-number.

## 4. Time is stored, not computed

A `datetime` is an offset from the Unix epoch in UTC; a `duration` is a span. Both
are whole seconds plus a sub-second remainder, and both are **normalised**: the
remainder is always in `[0, 1_000_000_000)`, so a negative span carries a
negative second count and a positive remainder.

That form is the one in which comparing the pair field by field is the same as
comparing the quantity, which is what lets both types sit in an index without a
comparison function of their own. A remainder of a whole second or more is
refused rather than normalised, because that pair has a second spelling and two
spellings of one value do not compare equal.

There is **no time zone** on a stored instant. A zone is a rendering choice made
where the value is displayed; storing one would make two instants naming the same
moment compare unequal.

Formatting, parsing, zones and date arithmetic are not part of the value system.
They belong to the query language, and a library layered on top of this
representation changes nothing about the bytes underneath it.

## 5. The payload encoding

A record version carries a versioned envelope with a tombstone flag. What follows
is what goes *inside* it.

```
<type-tag:1> <payload…>
```

The tags are **permanent**. A tag is never reused for a different type and never
renumbered, for the same reason key kinds are not: data already written carries
them.

| Tag | Type | Payload |
|---|---|---|
| `0x01` | `none` | — |
| `0x02` | `null` | — |
| `0x03` | `bool` | one byte |
| `0x04` | `number` | `<kind:1>` then integer `i64`, float bits `u64`, or decimal `<mantissa:i128><scale:u32>` |
| `0x05` | `string` | `<len:u32>` then UTF-8 |
| `0x06` | `bytes` | `<len:u32>` then bytes |
| `0x07` | `duration` | `<seconds:i64><nanos:u32>` |
| `0x08` | `datetime` | `<seconds:i64><nanos:u32>` |
| `0x09` | `uuid` | sixteen bytes |
| `0x0a` | `table` | `<table:u32>` |
| `0x0b` | `record` | `<table:u32>` then the record id |
| `0x0c` | `array` | `<count:u32>` then values |
| `0x0d` | `object` | `<count:u32>` then `<name-len:u32><name>` and a value, in name order |
| `0x0e` | `range` | two bounds, each `<kind:1>` and, unless open, a value |
| `0x0f` | `set` | `<count:u32>` then values |

An unknown tag is **refused**, and refused as `incompatible` rather than
`corruption`: the bytes are well formed and a newer build would read them, so the
operator action is to deploy a different binary, not to repair data. Inferring a
type from what follows the tag would read a newer format as a plausible wrong
value, and nothing downstream could tell.

### A decimal is stored in our terms

An exact decimal is written as its unscaled value and its number of fractional
digits — the two numbers that define it — not as whatever the arithmetic library
holds in memory. Writing the library's own layout to disk would make a dependency
upgrade a data migration, and would do it silently, because the bytes would still
parse.

Scale is preserved. `2` and `2.00` compare equal as numbers and encode to
different bytes, because scale is information about how the value was written and
losing it would change what a caller reads back.

### This encoding does not preserve order, and does not need to

Values are ordered by the comparison in §3, which is semantic: three spellings of
the number one compare equal there and could not possibly encode to the same
bytes here. Making the payload bytes sort as well would be a second ordering
authority disagreeing with the first.

## 5a. Reaching inside a value

A payload nests without limit: an object holds objects and arrays, and those hold
more. A **path** is how a value inside one is named — `address.city`, `tags[0]`,
`history[2].by`. The grammar is in `docs/tessariql.md` §3; what a step *means* over
each container is decided here, because it is a property of the value system and
not of the language that spells it.

| Step | Over | Reaches |
|---|---|---|
| `.name` | `object` | the value under that field, if the object has it |
| `.name` | anything else | nothing |
| `[n]` | `array` | the value at position `n`, counting from zero, if the array is long enough |
| `[n]` | `set` | nothing — a set has no significant order, so a position in one would name a different value as the set changed |
| `[n]` | anything else | nothing |

A path starts at a field of the record, so a payload that is not an object — a
space holds single values (ADR-0010) — has no paths into it at all.

**Reaching nothing is an answer, not a failure.** Every row above that says
"nothing" means the same thing as a missing top-level field has always meant: the
filter does not match, and the index does not index. Two consequences follow and
both are deliberate. Records of different shapes share a table without the store
having an opinion about it, which is the reason to hold documents. And a path is
a **function** — one route, one value or none — which is what lets an index over
a path have exactly one entry per record, the same as an index over a field.

`[*]`, "any element", would break that: it makes a path a relation, and an index
over one a multikey index with an entry per element. It is named in
`docs/tessariql.md` §8 as its own decision rather than left as a gap.

A path reaching `none` is distinct from a path reaching nothing, and the walk
keeps them apart for the same reason §2 keeps absent and null apart. Callers
above collapse the two where the rule they apply is the same — an index treats
both as "not indexed" — but that is their choice to make, not one the walk makes
for them.

## 6. What is fixed, and what can still move

| Decision | Status |
|---|---|
| The fifteen types | fixed for milestone 1; adding a sixteenth is additive |
| Type tags | **permanent** — never reused, never renumbered |
| The cross-type rank order | **contract** — changing it is a data migration |
| Numbers comparing semantically across kinds | **contract** |
| Decimal stored as mantissa and scale | fixed; it is what makes the library replaceable |
| Time as seconds plus a normalised remainder | fixed |
| No time zone on a stored instant | fixed |
| The three absent types | additive — each can arrive later with a new tag |
| A path is a function: one route reaches one value or none | **contract** — what makes an index over a path have one entry per record |
| A step that cannot be taken reaches nothing rather than failing | **contract** — the rule a missing field has always followed |
| A position addresses an array and never a set | fixed — a set has no order for a position to mean anything against |
| Whether the store's own API takes a value instead of bytes | **open** — the engines above will decide that surface |
