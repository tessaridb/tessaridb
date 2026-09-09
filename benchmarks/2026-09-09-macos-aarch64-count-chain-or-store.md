# The record count's disk cost: its own version chain, or the store (Q-481)

- backend: `memory` and `disk`
- build: release profile, `lto = "fat"`, `codegen-units = 1`
- machine: `macos` / `aarch64`
- workload: `spread` — 25 rounds × 10 000 write pairs, so 250 000 records into one
  table and 250 000 across a hundred tables, **alternating write by write**
- store: one, shared by both arms, so the engine's key population, level
  structure and compaction history are identical for them at every point

## The question

The count the planner reads to decide whether an index is worth serving is an
ordinary versioned record, so every write to a table appends a version of that
table's count and nothing removes the old ones, while reading the current value
happens once per commit. W164 priced that step by counterfactual build
(`2026-09-09-macos-aarch64-cardinality-write-path.md`): in memory **1.0 µs and
flat** across a quarter of a million records; on disk **3.9 µs at 2 500 records
rising to 12.3 µs at 250 000**.

Two mechanisms predict that ramp and W164 could not separate them:

1. **the chain** — the count record's own version chain, read once per commit and
   one entry longer after every write;
2. **the store** — the engine holding more keys, so its levels and its compaction
   cost more for every write, the count's own included.

Only the first has a fix inside `cardinality.rs`.

## Method, and why it is not the one the question proposed

Q-481 proposed writing the same records across a hundred tables and comparing
against W164's single-table run. That is two runs against two stores, which is
the blocked shape whose drift W164 had to discover the hard way — between two
blocks the *same* workload moved by a microsecond with no code involved.

This runs both arms **in one store, alternating on every single write**. After
each pair the two arms have written the same number of records, so:

- the store's total key population is the same for both, and so is everything
  downstream of it — levels, compaction, cache;
- the **only** difference is chain length. The `one` arm's count reaches 250 000
  versions; each of the hundred `many` counts reaches 2 500.

So the arms differ by a factor of a hundred in the quantity under test and by
nothing else that the store can see. No counterfactual build is needed.

### Predictions, written before the numbers

- under **the chain**, the `one` arm diverges *upward* from `many` as the run
  proceeds — and by roughly W164's own figure, since W164 measured the count's
  cost growing about **8 µs** between a 2 500-long chain and a 250 000-long one;
- under **the store**, the two arms track each other, and any drift between them
  is the confounder below.

### The confounder, named before the run

The `many` arm resolves a different table per statement and its records carry a
hundred distinct key prefixes. Both make **that** arm more expensive. It is the
one asymmetry this design could not remove, and it runs *against* the chain
hypothesis rather than for it.

## Disk — the decisive table

p50 µs per write, 10 000 samples per arm per round.

| round | records in the store | `one` (chain 10k→250k) | `many` (chains ≤2.5k) | gap |
|---|---|---|---|---|
| 1 | 20 000 | 81.9 | 84.3 | +2.4 |
| 2 | 40 000 | 83.3 | 86.9 | +3.6 |
| 3 | 60 000 | 83.2 | 86.8 | +3.6 |
| 4 | 80 000 | 84.4 | 89.0 | +4.6 |
| 5 | 100 000 | 83.3 | 87.5 | +4.2 |
| 6 | 120 000 | 84.3 | 88.6 | +4.3 |
| 7 | 140 000 | 95.5 | 99.4 | +3.9 |
| 8 | 160 000 | 96.7 | 101.2 | +4.5 |
| 9 | 180 000 | 95.7 | 100.8 | +5.1 |
| 10 | 200 000 | 96.3 | 101.5 | +5.2 |
| 11 | 220 000 | 96.8 | 102.0 | +5.2 |
| 12 | 240 000 | 97.1 | 102.8 | +5.7 |
| 13 | 260 000 | 109.5 | 115.3 | +5.8 |
| 14 | 280 000 | 110.0 | 116.5 | +6.5 |
| 15 | 300 000 | 110.2 | 117.2 | +7.0 |
| 16 | 320 000 | 111.3 | 118.4 | +7.1 |
| 17 | 340 000 | 110.6 | 117.8 | +7.2 |
| 18 | 360 000 | 112.5 | 119.8 | +7.3 |
| 19 | 380 000 | 121.8 | 129.8 | +8.0 |
| 20 | 400 000 | 121.8 | 130.6 | +8.8 |
| 21 | 420 000 | 121.5 | 130.7 | +9.2 |
| 22 | 440 000 | 126.3 | 135.4 | +9.1 |
| 23 | 460 000 | 129.5 | 138.5 | +9.0 |
| 24 | 480 000 | 129.1 | 139.3 | +10.2 |
| 25 | 500 000 | 108.1 | 112.7 | +4.6 |

Round 25 drops on both arms together — an engine event, not an arm effect; the
gap survives it. The r1→r24 figures below exclude it for that reason and the
conclusion does not depend on the choice.

- `one` ramps **81.9 → 129.1 µs**, ×1.58
- `many` ramps **84.3 → 139.3 µs**, ×1.65
- **In 25 of 25 rounds the arm carrying the hundred-times-longer chain was the
  cheaper one.**
- The gap grows **+2.4 → +10.2 µs**: a relative drift of **+7.8 µs onto `many`**.

## Memory — the control

- `one` **15.3 → 15.8 µs** (+0.5 across 250 000 records)
- `many` **16.0 → 17.0 µs** (+1.0)
- gap +0.7 → +1.2, drift **+0.5 onto `many`**, and again **25 of 25** rounds with
  the long chain cheaper

Which agrees with W164: on this backend the count is a constant per write.

## What the numbers say

**The chain hypothesis predicted −8 µs of relative drift onto `one`. The
measurement produced +7.8 µs onto `many`.** That is a sixteen-microsecond
discrepancy from the prediction, on a quantity whose per-round noise is under a
microsecond and whose sign was consistent in 25 of 25 rounds.

So the disk ramp W164 measured is **mechanism (2), the store**: writes get more
expensive as the engine holds more keys, and the count's own write is one of the
writes that gets more expensive. It is not the count record's version chain, and
there is no per-commit cost inside `cardinality.rs` proportional to how many
times a table has been written.

### What this does not establish

That the chain costs exactly zero. The two arms differ in one other way — the
confounder — and it pushes the same direction as a null result, so what is bounded
is `chain < confounder` rather than `chain = 0`. The bound is strong enough for
the decision the question was asked for: **the growth W164 attributed to the ramp
is not the chain**, because a hundred-fold difference in chain length moved the
arms the wrong way by twice the predicted amount.

It also says nothing about whether an unbounded version chain is a good idea. It
is still an unbounded chain, and version reclamation still has no scheduler
(Q-497). What it costs is a **read of the current value**, which this measurement
shows is not proportional to the chain — not a claim that the space is free.

### What it does not touch

**G023 S2.2 is PASS and stays PASS.** The criterion asked that the write-path cost
be measured and set no bound; W164 measured it. This is the finding beside that
row and was never a condition on it.

## Reproduction

```
cargo build -p tessari-bench --release
./target/release/tessari-bench --backend memory --workload spread
./target/release/tessari-bench --backend disk   --workload spread
```

The disk arm writes half a million records into a scratch store the harness
removes. **55.0 s** on this machine and **8.2 s** in memory — both taken twice,
once as wall clock (23:23:54 → 23:24:49) and once by summing the per-round
`ops / ops-per-second` the table itself reports, which agree to the tenth.
