# What the record count costs the write (G023 S2.2)

- backend: `memory` and `disk`
- build: release profile, `lto = "fat"`, `codegen-units = 1`
- machine: `macos` / `aarch64`
- workloads: `write` (2 000 point writes into a fresh store) and `capacity`
  (100 batches of 2 500, so 250 000 records in one table)
- method: **counterfactual build** — the same binary twice, differing only in
  whether `crate::cardinality::maintain` is called; the two call sites
  (`crates/tessari-storage/src/store.rs:458` and
  `crates/tessari-storage/src/transaction/commit.rs:212`) commented out and
  **never committed**, the tree restored byte-identical and verified against
  `HEAD` before a single number below was written down
- the arms are **interleaved** A/B/A/B in one session, not run in blocks

The criterion this answers is G023 **S2.2**: *gathering the estimate does not put
an O(table) cost on any read, and its write-path cost is measured*. The read half
was already structural — the probe's cap is the threshold, so the guard cannot
cost more than the plan it guards. This is the write half, which had never been
taken.

**It is not a background pass**, so no schedule and no staleness bound are owed:
`maintain` runs inside the commit's own `WriteBatch`, at both call sites, before
the batch is applied. That is read from the source, not inferred from a timing.

## Interleaving is what made the term resolvable

The first attempt ran the arms in blocks — three runs of A, then three of B — and
on disk the two overlapped completely: A 69.2/69.7/70.0 against B 68.9/69.7/71.5.
Read alone, that says the term is below the noise floor, which is what the
closest precedent in this project
(`2026-09-08-macos-aarch64-q414-sealing-lookup.md`) concluded about a different
term on the same backend.

It was drift, not noise. Between the two blocks the same workload moved from
14.4 µs to 15.5 µs on the memory backend with no code involved, so a block
comparison charges that drift to whichever arm ran later. Interleaving removes
it, and the term separates cleanly on both backends.

**In 100 of 100 table sizes on memory and 100 of 100 on disk, the arm carrying
the count was the slower one.** That consistency of sign, rather than any
interval, is what makes the number below reportable — a counterfactual that comes
out backwards names its own noise floor, and this one never did.

## Point writes into a fresh store — `write`, 2 000 records

Five interleaved pairs, p50 µs.

| | A, with the count | B, without | delta |
|---|---|---|---|
| memory | 16.0 / 15.5 / 15.4 / 15.7 / 15.5 | 14.9 / 14.2 / 14.4 / 14.2 / 15.0 | **+1.1** |
| disk | 84.2 / 83.2 / 83.8 / 82.5 / 82.4 | 81.6 / 80.6 / 78.6 / 79.8 / 76.2 | **+2.7** |

A is higher in all ten pairs.

## Sustained writes into one growing table — `capacity`, 250 000 records

Three interleaved pairs; each cell is the median of the three runs' p50 for that
batch.

### memory

| records in the table | A | B | delta |
|---|---|---|---|
| 2 500 | 14.3 | 14.1 | +0.20 |
| 12 500 | 14.3 | 13.5 | +0.80 |
| 25 000 | 14.2 | 13.4 | +0.80 |
| 50 000 | 14.4 | 13.5 | +0.90 |
| 75 000 | 14.5 | 13.5 | +1.00 |
| 125 000 | 14.6 | 13.5 | +1.10 |
| 175 000 | 14.7 | 13.6 | +1.10 |
| 225 000 | 14.6 | 13.7 | +0.90 |
| 250 000 | 14.8 | 13.7 | +1.10 |

Median delta over all 100 batches **1.00 µs**; first ten batches 0.80, last ten
1.00. Both arms ramp by the same amount across the whole run — A by +0.3 µs, B by
+0.2 — so **on this backend the count is a constant per write and does not grow
with the table**. One microsecond is **6.8%** of a 14.6 µs write.

### disk

| records in the table | A | B | delta |
|---|---|---|---|
| 2 500 | 75.5 | 74.0 | +1.50 |
| 12 500 | 76.8 | 72.7 | +4.10 |
| 25 000 | 77.8 | 73.6 | +4.20 |
| 50 000 | 78.0 | 74.0 | +4.00 |
| 75 000 | 78.0 | 75.1 | +2.90 |
| 125 000 | 92.5 | 72.6 | +19.90 |
| 175 000 | 93.8 | 83.5 | +10.30 |
| 225 000 | 90.7 | 86.7 | +4.00 |
| 250 000 | 100.7 | 84.0 | +16.70 |

Median delta over all 100 batches **7.10 µs**; first ten batches **3.90**, last
ten **12.30**. Individual batches swing widely — the engine's own compaction
lands somewhere in each run — so the robust reading is the first-ten against the
last-ten, and it says the same thing the ramp columns do: A rises **+19.8 µs**
across the run against B's **+11.6**.

**On disk the count is not a constant. Its per-write cost roughly triples over a
quarter of a million writes**, from about 5.1% of a write to about 12.7%.

## The tail grows even where the median does not

p99, median of the three runs.

| records | memory A | memory B | delta | disk A | disk B | delta |
|---|---|---|---|---|---|---|
| 2 500 | 20.5 | 20.4 | +0.1 | 121.2 | 128.0 | **−6.8** |
| 25 000 | 19.4 | 18.9 | +0.5 | 99.8 | 93.2 | +6.6 |
| 125 000 | 18.1 | 16.0 | +2.1 | 145.5 | 90.8 | +54.7 |
| 250 000 | 20.8 | 16.8 | +4.0 | 133.6 | 112.8 | +20.8 |

The memory column is the one worth reading, because its median is flat. **The p99
delta grows from 0.1 µs to 4.0 µs while the p50 delta stays at 1.1** — so
something in maintaining the count gets occasionally more expensive as the table
grows, on a backend where the typical case does not move at all. The single
negative cell, disk at 2 500 records, is this table's own noise floor stated out
loud.

## Why a growth term exists at all, from the source

The count is stored as an ordinary record:

```
RecordKey::new(SYSTEM_NAMESPACE, SYSTEM_DATABASE, RECORD_COUNTS, id, at)
```

`at` is the commit's sequence, so **every write to a table appends a new version
of that table's count record, and nothing removes the old ones**. After this
workload one logical count carries 250 000 versions. Reading the current value is
therefore a read of the newest version of an unboundedly long chain, and it
happens once per commit.

That is a fact about the key grammar, not an inference from the timings.

## What is not separated here

Two mechanisms would both produce the disk ramp and this measurement does not
tell them apart:

1. the count record's own version chain, read once per commit and growing by one
   entry per write;
2. the store simply holding twice the keys, so the engine's own levels and
   compaction cost more for **every** write, the record's included.

The memory backend argues weakly against (1) being the whole of it — its median
does not move while its tail does — but a tree-map seek over a long chain is
cheap in a way an LSM seek is not, so that is suggestive rather than decisive.

**The decisive test is cheap and is not this wave's**: run the same 250 000
writes spread across a hundred tables instead of one. That makes each count
record's chain a hundred times shorter while leaving the store's total key
population unchanged. If the ramp mostly disappears, it is (1); if it is
unchanged, it is (2). Recorded as an open question rather than guessed at.

## What this does not measure

- **Multi-table and multi-record commits.** Both workloads commit one record to
  one table, which is the cheapest shape for `maintain`: one snapshot, one
  previous-version read, one count read and one put. A transaction touching many
  tables pays a count read and a put per table.
- **Deletes.** Only arrivals are exercised. A delete takes the same path with the
  opposite sign.
- **Whether compaction returns the cost.** The same limit as the queue
  measurement records: nothing above `tessari-lsm` can ask for a compaction, so
  the ramp is measured on a store that was never compacted on request.
