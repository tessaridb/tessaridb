# Key grammar

The key space is the schema. What a range scan returns, whether an index is
correct, and what can never be changed after the first byte is written are all
decided here rather than in the code that uses it. This document is normative:
the encoding layer implements it, and a disagreement between the two is a defect
in the code.

Every rule below exists because its absence fails **silently** — wrong scan
results, an index that answers queries incorrectly, a decode that succeeds and
returns the wrong record. None of it raises an error.

## 1. Ordering model

Keys are compared as unsigned bytes, lexicographically, shortest-first on a
prefix tie. That is the only order the substrate provides
(`docs/storage-contract.md`, guarantee 1), so **every encoding must make byte
order equal logical order**.

Two consequences that are easy to get wrong:

- Numbers are never formatted as text. `"10"` sorts before `"9"`.
- A length prefix does not preserve order. Length-prefixed `"b"` sorts before
  `"aa"`, which is not lexicographic in the content.

## 2. Keyspaces

The four keyspaces are fixed (`docs/storage-contract.md` §"Why the keyspace set
is fixed"). Each key kind belongs to exactly one.

| Keyspace | Holds | Access shape |
|---|---|---|
| `data` | record versions | point read + bounded prefix scan |
| `index` | index entries of every kind | prefix scan |
| `log` | the ordered log | append + forward scan |
| `meta` | store-level state and the catalog | small, point read at open |

### 2.1 Tenancy is prefix, not physical

Namespace, database and table are **leading bytes of a key** and nothing else.
None of them is a separate tree, a separate file, or a separate engine instance.

The test is whether the count grows with application data. A store whose physical
structure has one unit per tenant grows its memory footprint and its background
scheduling with the customer list rather than with a design decision, and that
cost is paid whether or not the tenant is being written. The keyspace set is the
opposite: four, fixed here, justified one by one.

Two consequences follow, and both are rules rather than observations.

**A key never carries a shard, node, or partition component.** Placement is
derived from the key by the routing layer — key → virtual shard → node, through a
versioned map over a virtual-shard count fixed at design time. Putting the shard
in the key would make moving a shard a re-keying of its data, and deriving the
partition count from the node count would make adding a node a re-keying of
nearly everything. Both are migrations wearing the costume of an operation.

**Everything one transaction touches must share one physical unit.** Separate
engine instances share no write-ahead log, so a batch cannot span them — not even
two instances inside one process. The store's boundary is therefore the database:
a transaction may span tables within a database and may not span databases.

## 3. Key-kind tags

The first byte of every key is its kind tag. This is what lets one keyspace hold
several kinds and still be scannable, and it makes a collision between kinds
impossible rather than unlikely.

The table is assigned **once**, here and in `kind.rs`, and tags are never reused
or renumbered. Tags for kinds that are not yet implemented are reserved now,
because renumbering after data exists is a full rebuild.

| Tag | Kind | Keyspace | Status |
|---|---|---|---|
| `0x00` | — | — | never assigned; reserved as a sentinel and as the escape byte (§4.3) |
| `0x01` | `Record` | `data` | implemented |
| `0x10` | `SecondaryIndex` | `index` | implemented |
| `0x11` | `UniqueIndex` | `index` | implemented |
| `0x12` | `Posting` (full-text) | `index` | implemented |
| `0x13` | `VectorNode` | `index` | implemented — see §6.2c |
| `0x14` | `Edge` (adjacency) | `index` | implemented — see §3a |
| `0x15` | `SearchStatistics` | `index` | implemented — see §3b |
| `0x16` | `SpatialIndex` | `index` | implemented — see §3c |
| `0x17` | `VectorRecall` | `index` | implemented — see §3d |
| `0x20` | `LogEntry` | `log` | implemented |
| `0x30` | `FormatVersion` | `meta` | implemented |
| `0x31` | `AppliedPosition` | `meta` | implemented |
| `0x32` | `NamespaceCatalog` | `meta` | reserved, unused — see §9 |
| `0x33` | `DatabaseCatalog` | `meta` | reserved, unused — see §9 |
| `0x34` | `TableCatalog` | `meta` | reserved, unused — see §9 |
| `0x35` | `IndexCatalog` | `meta` | reserved, unused — see §9 |
| `0x36` | `IdAllocator` | `meta` | reserved, unused — see §9 |
| `0x37` | `BackfillWatermark` | `meta` | reserved — SG4 |
| `0x38` | `NodeIdentity` | `meta` | implemented — see §3b |
| `0x39` | `ReclaimFloor` | `meta` | implemented |
| `0x3a` | `GraphCatalog` | `meta` | reserved, unused — see §9 |
| `0x3b` | `EdgeKindCatalog` | `meta` | reserved, unused — see §9 |

### 3c. The spatial entry

```text
key    <0x16> <namespace:u32> <database:u32> <table:u32> <index:u32> <first:u64> <level:u8> <record-id>
value  <west:i64> <south:i64> <east:i64> <north:i64>
```

One entry per cell of the record's covering, so a record whose geometry spans
several cells has several entries. `first` is the start of the cell's range in
the Hilbert numbering of the finest level, and `level` says how coarse the cell
is.

**The range start comes first and the level second, and the order is the whole
design.** A cell's index within its own level is small at a coarse level and
large at a fine one, so ordering by it would interleave levels rather than
space. Ordering by the range start does not:

- every descendant of a cell has its start inside that cell's range, so **the
  descendants of a cell are one contiguous scan**;
- an ancestor's start is a truncation of a descendant's, so **the ancestors of a
  cell are a bounded, computable set** — at most `level` of them.

Both halves are needed by a reader. A record larger than the query box sits at a
*coarser* cell, whose start lies below the query cell's range, so a scan alone
never finds it: a reader that only scanned would return fewer rows than exist,
with nothing raised.

The value carries the record's bounding box so the filter step can reject a
candidate without decoding the geometry. It is computed in the batch that
carries the record's own mutation and by nothing else — a box maintained by a
background job or recomputed by a reader can lag the geometry it describes, and
a stale box excludes rows that should have matched.

A cell match is a **candidate and never a result**: the cells are coarser than
the box and the box is coarser than the shape.

#### How a read uses it

The query shape gets a covering of its own, and each of its cells is two reads:

| half | read | why it exists |
|---|---|---|
| descendants | one scan of `[first, last+1)` after the index prefix | every cell under it has its range start inside that span |
| ancestors | one fixed-width prefix lookup per level above it, at `(truncated start, level)` | a record **larger** than the query sits at a coarser cell, whose start lies *below* the span |

**Neither half is optional.** A reader that only scanned would answer small
questions perfectly and lose exactly the large records, with nothing raised — the
one failure direction a filter must not have. There are at most `level` ancestors
and each is one truncation, so the second half is cheap as well as necessary.

Completeness, without appealing to the curve: if a record's geometry meets the
query box, some position lies in both; a covering holds the cell of every
position inside its box, so that position's finest cell lies under a record cell
and under a query cell; two cells containing a common cell are nested. So the
record's cell is a descendant of a query cell, is one, or is an ancestor of one —
and those are exactly the two reads above. The curve decides the *order*; the
monotonicity of the placement decides *inclusion*.

The box test that follows is chosen per predicate rather than shared, because a
filter narrower than its predicate drops true results while a wider one only
costs refinement — `intersects` filters by box intersection, `within` by the
query box containing the record's, `contains` by the reverse, `equals` by
equality. `disjoint` has no such test at all and takes the scan.

#### How a nearest-first read uses it instead

A "closest ten" read asks a different question of the same keys and does not use
either half above. It walks the **implicit quadtree** of cells from the root,
keyed by a floor under the distance from the query position to the cell's own
box, opening the cheapest cell waiting and stopping when the cheapest one left is
further away than the worst answer already held.

Two properties of the layout carry it. A cell's entries are one fixed-width
prefix lookup at `(range start, level)`, which is what makes "what is stored
exactly here" separable from "what is stored below here"; and a cell's whole
subtree is the one span `[first, last+1)`, which is what lets the walk read a
sparse region outright instead of descending thirty-two levels through cells that
exist only because every cell exists. A subtree that comes back short of
`SPATIAL_WALK_SUBTREE_ENTRIES` was not truncated, so the walk has all of it and
never splits that cell.

The floor is the whole of the correctness argument and the one thing that must
not be approximated: a key that could **exceed** the true distance to something
inside the cell would let the walk discard the cell holding the nearest record
and answer, in order and with confidence, with the second. It is computed as the
larger of two independently provable floors — the meridian arc to the cell's
latitude band, and the straight-line distance in space to the wedge its longitude
span sweeps, a chord being no longer than any surface path between the same two
points.

Ranking by an entry's **position** is not the same act as filtering by its
existence, so this read carries the refusals an ordered read carries and a
filtered read does not: an uncommitted write, a snapshot behind the committed
tail, a field the caller cannot see, and an index that runs out before the bound
is filled all send it to the scan.

### 3b. The node identity, and why it is `meta` rather than a record

Every other kind above is either derived from the log or written into it. This
one is neither, and that is the point of it.

A node's own identity — its id, its roles, the build it last ran, and where peers
reach it — is a fact about **this process on this machine**. State in this store
is a function of the log, so anything written there is reproduced by whoever
replays it; an identity reproduced that way would be inherited by a machine that
restored last night's backup, and two processes would then answer to one id with
nothing reporting the collision.

So it sits beside the format version and the applied position, both of which are
per-store facts for the same reason, and a backup neither carries it nor restores
it. The value's own payload begins with a revision byte because it is expected to
grow: the version field in it is rewritten whenever the binary changes, which is
what gives an upgrade a place to notice itself.

### 3a. The adjacency entry

```text
<0x14> <ns:u32> <db:u32> <graph:u32> <node-table:u32> <node-id>
       <edge-kind:u32> <dir:u8> <neighbour-table:u32> <neighbour-id>
```

`0x14` was reserved for a graph adjacency key, went unused while an edge was an
ordinary record, and is now what the graph engine writes.

**The earlier reading was right about edge tables and wrong about graphs.** An
edge in an edge table is a record carrying `out` and `in`, so "the edges out of
`users:1`" is a read of an ordinary secondary index (`0x10`), and that model
still stands — edge tables are unchanged. What it cannot do is hold a node's
neighbours *together*: a hop is an index probe followed by one random read per
neighbour, and at depth three over a fanned-out node that is thousands of random
reads. Holding adjacency beside the node makes a hop a single range read, and
that difference is the reason the graph engine exists at all.

Every component's position is load-bearing. `<ns><db>` first, as everywhere else,
so a tenancy is one range. `<graph>` above the node, so the whole structure is one
prefix and `DROP GRAPH` is a range delete rather than a scan. `<node-table>` and
`<node-id>` together, so everything touching one node is contiguous.
`<edge-kind>` before `<dir>`, because "this node's `works_at` edges" is the common
question and the reverse order would make it two ranges. The neighbour last, which
makes the entry unique and makes "is A joined to B" a point read.

Both directions are written — `0x00` out, `0x01` in — in the **same `WriteBatch`**
as the edge that creates them. An entry written outside that batch is an orphan
nothing reconciles: either the edge is gone and a walk still reaches through it,
or the edge is there and no walk finds it, and neither is an error state. A
direction byte that is neither `0x00` nor `0x01` is refused rather than defaulted,
because a mis-decoded direction turns a follower into a followee silently.

The value carries the edge's properties, encoded, on **both** entries. Storing a
pointer to an edge record instead would reintroduce exactly the random read per
neighbour this layout removes. The two copies cannot drift apart, because an edge
is identified by its endpoints and endpoints are immutable; a property update
rewrites both entries in the one batch that wrote them.

Tags are grouped by family (`0x0_` data, `0x1_` index, `0x2_` log, `0x3_` meta)
so a hex dump is readable and each family has room to grow.

Every decode error names the kind it was decoding. An error that reports opaque
bytes leaves an operator unable to tell which subsystem wrote the bad key.

### 3b. The search-statistics tag

`0x15` holds what a full-text index knows about its collection rather than about
any one document: how many records hold at least one term, and how many tokens
those records hold in total.

It is a **key kind and not a value on the postings** because it is not a fact
about a posting. A posting says a term is in a document; a score says how much
that matters, and that needs the collection's size and its typical document
length — neither of which any record can be asked for. They are maintained on
write, in the same batch as the postings they summarise, for the reason index
entries are: state written outside the batch that implies it is state nothing
will ever reconcile.

It sits in the `index` keyspace rather than `meta` because it is derived from the
postings, is meaningless without them, and is swept with them.

### 3d. The vector-recall tag

`0x17` holds the recall one vector index was last **measured** at, together with
everything a reader needs to know what that number describes.

It is a key kind rather than a field on the index definition for the reason
`0x15` is: a definition is what the language wrote, and this is a measurement
derived from the log. The key is the bare index prefix with no suffix, so one
index has exactly one and finding it is a point read; and it is **derived from
the index address** rather than stored beside it, so it cannot come to name the
wrong index.

Being in the `index` keyspace is what makes the figure correct over time. The
keyspace is cleared as a unit when an index is rebuilt, so a rebuild that
produces no measurement leaves **no** key rather than the previous one — and
absence reads as *never measured*, which is a different statement from a measured
zero. A figure left behind would describe a graph that no longer exists, and
nothing would raise it until somebody read the number.

## 4. Component encodings

### 4.1 Unsigned integers

Fixed-width big-endian. `u32` → 4 bytes, `u64` → 8 bytes. Variable-width and
little-endian both break ordering.

### 4.2 Signed integers

Fixed-width big-endian with the sign bit flipped — XOR the leading byte with
`0x80`. This maps `i64::MIN` to `0x00…00` and `i64::MAX` to `0xFF…FF`, so
negatives sort before positives.

### 4.3 Variable-length byte strings

Escaped terminator, never a length prefix.

- `0x00` in the payload is written as `0x00 0xFF`
- the component ends with `0x00 0x01`

The terminator is smaller than any escaped continuation (`0x01 < 0xFF`), which
is what makes a shorter component sort before a longer one that extends it.
`0x00 0x00` is deliberately left unused so it remains available as a
sorts-before-everything sentinel for range bounds.

Worked cases, which are also the test vectors:

| Content | Encoded | Ordering fact |
|---|---|---|
| `"a"` | `61 00 01` | precedes `"ab"` — `0x00 < 0x62` |
| `"ab"` | `61 62 00 01` | |
| `"a\x00"` | `61 00 FF 00 01` | follows `"a"` — `0xFF > 0x01` |
| `""` | `00 01` | precedes every non-empty content |

### 4.4 Descending u64

Complement every byte (`!value`), written big-endian. Largest value sorts first.
Used for the MVCC version suffix, and nowhere else without saying so.

### 4.5 Numbers in an index

An earlier draft of this document proposed encoding a float by its IEEE-754 bit
pattern with the sign bit flipped. That is order-preserving **within** floats and
useless across the numeric union, because the three kinds must compare
semantically: `1`, `1.0` and decimal `1.00` are one value and must produce one
set of bytes. A bit-pattern encoding gives three, and the consequence is a unique
index that admits duplicates and an equality lookup that misses — with no error
anywhere. The forward obligation in §5 is what that draft rule would have broken.

The encoding actually used is a **normalised decimal form**:

```
<class:1> [ <exponent:i32, sign-flipped> <digits…> <terminator:1> ]
```

| Class | Byte | Payload |
|---|---|---|
| negative infinity | `0x00` | — |
| negative finite | `0x01` | exponent and digits, every byte complemented, terminated `0xFF` |
| zero | `0x02` | — |
| positive finite | `0x03` | exponent and digits, terminated `0x00` |
| positive infinity | `0x04` | — |
| not a number | `0x05` | — |

The four classes without a payload each hold exactly one value, so their class
byte alone places them — which is also how the declared order `-∞ < finite < +∞ <
NaN` becomes simple byte order.

A finite value is written as `0.<digits> × 10^exponent` with trailing zeros
dropped, so `1`, `1.0` and `1.00` all become exponent `1` and digits `1`. Digits
are ASCII, and the terminator is below them for a positive number and above their
complements for a negative one, so a shorter digit string sorts before a longer
one that extends it — `0.5 < 0.51`, and `-0.51 < -0.5`.

The digits come from the number's **decimal normal form**, which is the same form
the comparison reduces to. Deriving them independently from the float would agree
in every obvious case and disagree exactly where it matters: a float too small
for a decimal compares *equal to zero*, and its own digits would place it just
above.

This encoding **cannot be reversed** — normalisation is what makes it correct and
also what destroys the distinction between the three spellings. An index stores
ordering identity, not values; a caller that wants the value reads the record.

## 5. `RecordId`

A record id is a discriminated union. The discriminant is one byte and it
determines both the payload shape and whether the payload is terminated.

| Discriminant | Variant | Payload | Terminated |
|---|---|---|---|
| `0x01` | `Int(i64)` | 8 bytes, sign-flipped big-endian (§4.2) | no — fixed width |
| `0x02` | `Text(String)` | UTF-8, escaped (§4.3) | yes |
| `0x03` | `Uuid([u8; 16])` | 16 raw bytes | no — fixed width |
| `0x04` | `Bytes(Vec<u8>)` | escaped (§4.3) | yes |

Ordering across variants is by discriminant: every `Int` sorts before every
`Text`, and so on. That is arbitrary but total, deterministic, and documented,
which is what the index requires.

Fixed-width variants carry no terminator because the discriminant already
declares their width. Key bytes are duplicated into index blocks and filters, so
two bytes per key is a real cost at scale, not a rounding error.

**Forward obligation.** When the numeric union (`Number` = int / float / decimal)
becomes a legal record id or index value in SG2.T4, it needs **one canonical
encoder**: `0`, `0.0` and decimal `0` are one value and must encode to identical
bytes. Encoding each variant with its own natural representation gives a unique
index that admits duplicates and an equality lookup that misses — with no error
anywhere.

## 6. Key layouts

### 6.1 `Record` — keyspace `data`

```
<0x01> <namespace:u32> <database:u32> <table:u32> <record-id> <!version:u64>
   1        4              4             4          variable        8
```

- Bytes 0..13 are a **fixed-width prefix** identifying one table. That is the
  prefix a prefix-extractor would use, and it is why the three ids are
  fixed-width integers rather than names.
- Names are not in the key. A namespace, database or table can be renamed
  without rewriting a single record, because the key carries the id and the
  catalog carries the name. Keys hold identity; anything that can change lives
  in the value.
- The record id **must** be self-delimiting, because a fixed-width suffix
  follows it. Without a terminator, `"a"` followed by a version starting `0x62`
  would sort after `"ab"` — a wrong order between two distinct records, with no
  error.
- The version suffix is the complement of the sequence (§4.4), so versions of one
  record sort newest-first. A read at snapshot `S` seeks to
  `<prefix><id><!S>` and takes the first entry within `<prefix><id>`: that is
  the newest version at or before `S`.

### 6.2 `LogEntry` — keyspace `log`

```
<0x20> <sequence:u64>
   1        8
```

Ascending, so replay is a forward scan from a position. The same sequence is
encoded **ascending here and descending in a record key** — two access patterns,
two encodings, one number. Confusing the two silently reverses replay order.

The value is a `LogRecord`: everything one commit changed.

```
<codec-version:1> <flags:1> then, repeated until the value ends:
  <namespace:u32> <database:u32> <table:u32> <record-id, terminated>
  <value-len:u32> <RecordValue bytes>
```

Three properties of that layout are load-bearing.

**A mutation carries the record's address, not its encoded key.** An encoded key
has the version baked into it, so a record whose embedded version disagreed with
its own log sequence would create a second ordering authority — the thing the
log exists to prevent. Applying derives the version from the entry's own
sequence, which makes the disagreement unrepresentable rather than merely
forbidden.

**There is no mutation count.** Every mutation is self-delimiting: the record id
is terminated and the value is length-prefixed, so the decoder reads until the
value ends. A count would be a second statement of the same fact, and two
statements of one fact can disagree. It would also need a width, and a width
needs a policy for a commit that exceeds it.

**Mutations are stored in address order.** Byte-identical replay depends on the
*encoder* being deterministic, not only on the apply path, so the record type
orders them at construction rather than trusting its caller to.

### 6.2a Index entries — keyspace `index`

```
secondary  <0x10> <namespace:u32> <database:u32> <table:u32> <index:u32> <values…> <0x00> <record-id>
unique     <0x11> <namespace:u32> <database:u32> <table:u32> <index:u32> <values…> <0x00>
```

Bytes 0..17 are a **fixed-width prefix** identifying one index, for the same
reason the record key's first 13 bytes identify one table.

**A unique entry carries no record id, and that is the enforcement.** Two records
holding the same indexed value produce the same key, so the second write collides
with the first instead of sitting beside it. Uniqueness is a property of the
layout rather than a check someone has to remember to run. The record id lives in
the entry's *value* instead, because a unique lookup still has to say which
record it found.

A non-unique entry ends with the record id, so many records share one value. Its
value is empty: the id is already in the key, and the same fact written twice is
two statements that can disagree.

The two are separate kinds rather than one kind with an optional suffix. A single
kind would force a decoder to guess whether trailing bytes are a record id, and a
grammar whose parse depends on a guess is precisely what this layer exists to
rule out.

**The field list is terminated** even though an index's arity is fixed by its
definition. That keeps a key decodable on its own, without the catalog entry that
describes it — which matters exactly when something has gone wrong and an
operator is looking at raw bytes. The terminator `0x00` is below every value tag,
so a shorter list sorts before a longer one that extends it.

Values are encoded per §4.5 and §7a, and each is self-delimiting, which is what
lets the parser find where the list ends and the record id begins.

A full-text posting is `0x12` with the same shape as a secondary entry, its term
in the value position.

### 6.2b `SearchStatistics` — keyspace `index`

```
key    <0x15> <namespace:u32> <database:u32> <table:u32> <index:u32>
value  <documents:u64> <terms:u64>
```

The key is exactly the 17-byte index prefix with **no suffix**, so one index has
exactly one of these and reading it is a point read rather than a scan for the
single entry a prefix would hold.

`documents` counts the records contributing at least one term; a record whose
indexed field is absent, empty, or not text is not in the index and is not
counted. `terms` is the token count **with repeats**, because it exists to be
divided by `documents` and yield an average document *length*. The postings
deduplicate and this does not; both come from one analyzer pass over the same
text, so they cannot drift apart.

### 6.2b-2 `VectorRecall` — keyspace `index`

```
key    <0x17> <namespace:u32> <database:u32> <table:u32> <index:u32>
value  <recall:u32> <at:u32> <sample:u32> <records:u64> <neighbours:u32> <exploration:u32>
```

The key is the 17-byte index prefix with **no suffix**, exactly as `0x15` is.

`recall` is a percentage and never appears alone. Recall decays as records arrive
after the build that measured it, so a lone figure describes a store that may no
longer exist: `at` says which `k` it is recall *at*, `sample` how many queries it
averages, `records` how large the store was at the time — which is what lets a
reader see the number has been outgrown — and `neighbours` and `exploration` the
engine constants in force, since a recall measured at one budget does not
describe another.

The queries are the store's own vectors, sampled by position in key order, with
the query record removed from both the exact answer and the walk's answer before
they are compared. A stored vector queried against itself is at distance zero, so
keeping it would put a floor of `1/at` under every figure.

### 6.2c `VectorNode` — keyspace `index`

```
key    <0x13> <namespace:u32> <database:u32> <table:u32> <index:u32> <level:u8> <record-id>
value  <dimensions:u32> <component:f64 × dimensions> <neighbours:u32> <record-id × neighbours>
```

One node of a vector index's navigable graph.

**The level byte is reserved and is always zero.** A hierarchical graph assigns
each node a level, and the layers improve routing at large collection sizes — but
a level drawn from a generator is what a store whose index entries are *derived
rather than logged* cannot have, because two replicas would build different
graphs from one log and disagree, silently, about which records are nearest. One
layer needs no levels: insertion order is log order, which every replica replays
identically. The byte is in the key anyway, because reserving room costs nothing
today and cannot be done retroactively, and because it sorts before the record id
so a future level's nodes group together.

**The vector is in the node.** A walk visits many nodes and answers with few, so
carrying it here means the search touches index keys and decodes no records until
the answer is chosen.

Components are stored as their **bit patterns**, not in the order-preserving form
an index key uses: nothing here has to sort, and the walk wants numbers to
compute with.

### 6.3 `FormatVersion` — keyspace `meta`

```
<0x30>
   1
```

A singleton. The store's own on-disk format version, independent of any engine's
internal versioning. It is written at creation and read at open; an on-disk
version newer than the binary supports is a refusal to open, never a
best-effort continue.

### 6.4 `AppliedPosition` — keyspace `meta`

```
<0x31>
   1
```

A singleton holding the log position whose effects are durably present in the
state. It is written **in the same batch as the state it describes** — that is
what makes recovery a resumable replay instead of a guess (ADR-0001).

## 7. Value layout

Every stored value begins with a codec version. The first byte is format
metadata, not payload; decode dispatches on it, and an unknown version is a typed
error naming the version found and the versions supported.

```
<codec-version:1> <flags:1> <payload…>
```

| Field | Meaning |
|---|---|
| `codec-version` | `0x01` for this format. Not a payload byte. |
| `flags` | bit 0 = tombstone, for value types that have versions. Every other bit is reserved, and so is bit 0 for value types that cannot be deleted. |
| `payload` | opaque to this layer; the document codec (SG2.T4) owns it |

Policies, stated rather than left to be discovered:

- **A tombstone is a version, not an absence.** Deleting a record writes a new
  version with the tombstone bit set and an empty payload. Under MVCC a delete
  must be visible at the sequence it happened, which an absence cannot express.
- **Reserved flag bits must be zero.** A non-zero reserved bit is a decode error,
  not something to ignore: it means the writer knew something this binary does
  not, and continuing would misread the record.
- **Unknown codec version is an error, never a fallback.** Opening
  forward-compatibly writes old-format data into a new-format store.
- **No compression in the codec.** When a block-compressing engine is added
  (SG2.T5), compressing again per value defeats cross-record redundancy and
  doubles the CPU. Compression belongs to exactly one layer.

## 7a. The index value tag table

Index field values carry a leading tag in the value system's rank order, so byte
order across types **is** the declared cross-type order.

| Tag | Type | Encoding |
|---|---|---|
| `0x01` `0x02` | `none`, `null` | — |
| `0x03` | `bool` | one byte |
| `0x04` | `number` | §4.5 |
| `0x05` `0x06` | `string`, `bytes` | escaped (§4.3) |
| `0x07` `0x08` | `duration`, `datetime` | `<seconds:i64 sign-flipped><nanos:u32>` |
| `0x09` | `uuid` | sixteen bytes |
| `0x0a` | `table` | `<table:u32>` |
| `0x0b` | `record` | `<table:u32>` then the record id (§5) |
| `0x0c` `0x0f` | `array`, `set` | elements, then `0x00` |
| `0x0d` | `object` | escaped name and value per field in name order, then `0x00` |
| `0x0e` | `range` | two bounds, each `<kind:1>` and, unless open, a value |

These are the same numbers as the payload codec's tags (`docs/value-system.md`
§5) so that a hex dump reads the same way in both. They are nonetheless
**separate contracts**: one is built to order and cannot be reversed, the other
is built to round-trip and does not sort. Neither may be renumbered.

The container terminator is `0x00`, which is below every tag — that is what makes
`[1]` sort before `[1, 2]`.

## 8. What is fixed forever, and what is not

| Decision | Changeable later? |
|---|---|
| Kind tag values | no — a renumber is a full rebuild |
| Component encodings (§4) | no — byte order is the index |
| The 13-byte record prefix width | no — a prefix extractor records its identity in every file it writes |
| `RecordId` variant discriminants | no |
| Adding a **new** kind tag from the reserved range | yes — that is what the range is for |
| Adding a new `RecordId` variant with a fresh discriminant | yes |
| Codec version and payload format | yes — via a new codec version, with both decoders live until a sweep proves the old one unused |

Anything in the first group changes only by rebuilding the store from an export.
That is the reason this document exists before the code does.

## 9. The catalog is records, and its reserved tags stay unused

Tags `0x32`, `0x33`, `0x34` and `0x36` were reserved for namespace, database,
table and allocator entries in the `meta` keyspace. They are **not used**, and
the reservation is not withdrawn — withdrawing it would let a future kind reuse
those bytes, and tags are permanent.

The catalog is instead ordinary records in a reserved tenancy: namespace `0`,
database `0`, and seven well-known table ids inside it. Two properties are what
decided it, and neither is available to a `meta` key kind:

- **It rides the log.** State is a deterministic function of the log, so a
  catalog change that was not a record would not appear in a log entry, and a
  replica replaying the log would rebuild every row while knowing about no table
  at all. Fixing that means a second mutation variant and a second apply path.
- **It is versioned.** A definition is a record with an MVCC version, so a read
  at sequence `S` sees the schema at `S`. Without versions, a transaction that
  began before a change would decode its records against a definition written
  after it — silently, and with plausible wrong values.

That a table can be created and written to in one transaction falls out of the
same choice rather than being built.

A key never carries a catalog name. Names live inside the definition, and a
separate name record makes them unique — conflict detection is over what a
transaction wrote, so an invariant two transactions must not both satisfy has to
be materialised into a key they both write.
