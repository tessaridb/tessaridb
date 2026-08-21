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
| `0x10` | `SecondaryIndex` | `index` | reserved — SG4 |
| `0x11` | `UniqueIndex` | `index` | reserved — SG4 |
| `0x12` | `Posting` (full-text) | `index` | reserved — SG4 |
| `0x13` | `VectorNode` | `index` | reserved — SG4 |
| `0x14` | `Edge` (graph) | `index` | reserved — SG4 |
| `0x20` | `LogEntry` | `log` | implemented |
| `0x30` | `FormatVersion` | `meta` | implemented |
| `0x31` | `AppliedPosition` | `meta` | implemented |
| `0x32` | `NamespaceCatalog` | `meta` | reserved — SG2.T4 |
| `0x33` | `DatabaseCatalog` | `meta` | reserved — SG2.T4 |
| `0x34` | `TableCatalog` | `meta` | reserved — SG2.T4 |
| `0x35` | `IndexCatalog` | `meta` | reserved — SG4 |
| `0x36` | `IdAllocator` | `meta` | reserved — SG2.T4 |
| `0x37` | `BackfillWatermark` | `meta` | reserved — SG4 |

Tags are grouped by family (`0x0_` data, `0x1_` index, `0x2_` log, `0x3_` meta)
so a hex dump is readable and each family has room to grow.

Every decode error names the kind it was decoding. An error that reports opaque
bytes leaves an operator unable to tell which subsystem wrote the bad key.

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

### 4.5 Floats

Not used in a key yet. When the value model lands (SG2.T4), the encoding is: take
the IEEE-754 bit pattern, flip the sign bit when the value is positive, flip
every bit when it is negative. **NaN has no position in a total order and is
rejected by the encoder rather than given an arbitrary one.**

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
