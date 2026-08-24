# Storage substrate — what it guarantees, and what it does not

A store is chosen by what it provides and operated correctly by what it does
not. Both lists are written here, because the second one is the one that gets
assumed: a missing guarantee that nobody wrote down does not fail at adoption —
it fails later, as data drift, with no error anywhere.

This document describes the `tessari-kv` layer only. Layers above it supply most
of what is on the second list, and they are named per item.

## What a backend guarantees

| # | Guarantee | Proven by |
|---|---|---|
| 1 | Keys iterate in lexicographic byte order, and a reverse scan is exactly the forward scan reversed | `scan-is-ordered`, `reverse-scan-mirrors-forward` |
| 2 | A batch is atomic — all of it is visible or none of it, including across keyspaces and across a crash | `batch-is-atomic-across-keyspaces` |
| 3 | Preconditions are evaluated against the same state the batch is applied to; a failed precondition writes nothing | `failed-precondition-writes-nothing`, `satisfied-precondition-applies` |
| 4 | Keyspaces are isolated — a key written to one is invisible from another | `keyspaces-are-isolated` |
| 5 | Reading an absent key returns `None`, never an error | `absence-is-a-value` |
| 6 | A scan limit caps the result, and an empty or inverted range returns nothing rather than failing | `scan-limit-is-honoured`, `empty-range-returns-nothing` |
| 7 | Deleting an absent key succeeds; later operations in one batch win over earlier ones | `delete-of-absent-key-succeeds`, `last-write-in-a-batch-wins` |

Every guarantee has a check in the conformance suite, and every backend runs the
same suite. A backend that does not pass it is not a backend.

## What a backend does not guarantee

These are the assumptions a relational background brings and that this layer does
not honour. Each names the component that supplies it and the test that will
prove it, so nothing on this list stays an intention.

| Missing guarantee | Supplied by | Proven by | Status |
|---|---|---|---|
| **Sequencing.** The backend assigns no version, timestamp or order to writes | the record store's committed tail; the replication log extends it — ADR-0001 | log replay determinism test | **partial** — the store assigns sequences; the log follows |
| **Multi-statement transactions.** A batch is atomic; a transaction spanning reads and writes over time is not | the transaction layer, built from batches plus preconditions | isolation test under concurrent writers | **built** — `tessari-storage` |
| **Snapshot isolation.** Concurrent readers and writers get no isolation beyond individual operations | the MVCC layer, keyed on the sequence — ADR-0006 | concurrent read-write consistency test | **built** — `tessari-storage` |
| **Serializability.** The declared level is snapshot isolation, so write skew is permitted | not provided — ADR-0006 records the SSI upgrade path, which changes no bytes on disk | write-skew test, which asserts the anomaly **happens** | **deliberately absent** |
| **Range or predicate locks.** Conflict detection is per record, so phantoms are possible | not provided at snapshot isolation; would arrive with SSI | phantom-read test | **deliberately absent** |
| **Secondary index maintenance.** Index entries are ordinary keys in the index keyspace; nothing maintains them automatically | the engine, writing index entries in the *same batch* as the record | orphan-index-entry sweep | not built |
| **Uniqueness.** No constraint exists | the engine, via an `Absent` precondition in the same batch | `absent-precondition-guards-uniqueness` | **proven at this layer** |
| **Retention or garbage collection.** Delete exists; deciding what and when does not | the engine's GC, over log and MVCC versions | space-reclamation test | not built |
| **Exactly-once retry.** A timeout is ambiguous, as in every store | the engine, via idempotent request identity | retry-idempotency test | not built |
| **Durability.** The in-memory backend has none, by construction | the persistent backend's write path | kill-the-writer-and-reopen test | **built** — `tessari-lsm` |
| **Cross-process access.** One process owns the store | out of scope until the cluster milestone | second-open test, which asserts the refusal | **enforced** — the second open is refused by name |

## Durability

The in-memory backend promises nothing and says so. The persistent backend
names a level per store, chosen when it is opened, because "durable" on its own
tells a caller nothing.

| Level | An acknowledged write survives | Cost |
|---|---|---|
| `power-loss-safe` | loss of power to the machine | one device sync per commit; throughput is bounded by the device's sync rate |
| `process-crash-safe` | the process being killed — **lost on power loss** | none beyond the write itself |

`power-loss-safe` is the default, because this store is a system of record.
There is deliberately no third level below these: a store that buffers
acknowledged writes inside the process is not something a caller can reason
about, and no caller has asked for one.

Two limits belong in the same breath as the promise, because neither is
discharged by the test suite passing:

- The power-loss half of `power-loss-safe` is only as strong as the platform
  underneath it. A sync that a drive acknowledges out of its own cache is not
  durability, and no software test can tell the difference — confirming it is
  part of taking a store into production on particular hardware.
- The engine setting that keeps a cross-keyspace batch whole across a *flush* is
  in effect and is asserted against the option set the engine itself writes.
  That it holds under a *divergent* flush — one keyspace flushed, another not —
  needs a crash placed inside the flush, and is not yet proven.

## What a write conflict means at this layer

A batch may carry preconditions, and the substrate guarantees they are evaluated
against the same state the batch is applied to. Both backends deliver that by
serialising batches: one writer, one lock, for the whole check-and-write.

That is a deliberate choice over a transaction database. Conflict detection that
validates at commit and retries would contend on every commit, because the layer
above commits through one shared position key — the retry loop would become the
workload rather than protect against it. Locking transactions would buy a lock
manager, lock timeouts and deadlock detection to protect a store that already
has exactly one writer process, which the engine enforces by holding the store
directory.

The cost is that commits serialise. When that becomes the limit, the answer is
one writer draining a queue and batching what it finds — not a weaker guarantee.

## Why the keyspace set is fixed

`meta`, `data`, `index`, `log` are declared at compile time and created at first
open, including while empty.

A keyspace maps onto a physically separate region in backends that have them.
Adding one to a store that already holds data is not a code change — it is a
coordinated redeployment that reopens every existing store with a new region
name. The set is therefore decided once, up front.

`meta` exists from the first open for exactly this reason, even though nothing
writes to it yet.

Tables are **not** keyspaces. A table is a key prefix inside `data`. Keyspaces
separate access *shapes*: an append-and-scan log behaves nothing like random
point reads, and separating them is what lets each be tuned independently.

## Errors

Every error carries a category, and every category declares whether retrying can
help. Callers branch on the category, never on the message text.

| Category | Retryable | Meaning |
|---|---|---|
| `conflict` | no | a precondition did not hold; re-read and decide again |
| `validation` | no | the request was malformed |
| `busy` | **yes** | contention or a stall; the caller owns the backoff |
| `unavailable` | **yes** | a dependency is temporarily unreachable |
| `corruption` | no | stored data failed an integrity check; an operator decision |
| `lifecycle` | no | shutting down, or the keyspace was dropped |
| `incompatible` | no | the data is intact but written in a format this binary does not support; deploy a newer binary rather than repair the store |
| `internal` | no | a bug or violated invariant |

A missing key is not in this table because it is not an error — it is `None` from
a successful read.
