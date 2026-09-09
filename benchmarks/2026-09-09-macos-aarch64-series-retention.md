# The declared retention clause, against the condition that expresses it

- machine: `macos` / `aarch64`
- build: **release**
- backends: `memory` and `disk`, run separately, same binary
- workload: `series` (`crates/tessari-bench/src/series.rs`)
- samples: three per cell, each on its own freshly built table
- wave: W182 · goal **G023**, criterion **S4.2**

## The question

S4.2 asks whether retention *"exists as a declared clause and drops a range
without reading the table whole"*, validated *"measured against `DELETE
WHERE`"*.

W168 answered the same question for the **statement** form. What is measured
here is the **declared clause** — `DEFINE SERIES … RETAIN`, with
`Store::expire_series` as the pass that empties it — which is what the criterion
actually names and which had never been run against a growing table.

## The method, and why it is not a head-to-head

A single comparison at one table size cannot answer *"without reading the table
whole"*: whichever arm wins, both numbers are consistent with both arms reading
everything. So **the removed window is held constant at 1 000 records and what
the table keeps grows eight times.** A flat arm and a linear arm are then
distinguishable by eye and need no profiler.

Both arms name every identity explicitly — the removed window at instants in
2024, the kept records at instants seconds old, against a `RETAIN 1h`. That does
two things: it puts the two populations two years either side of the floor, so
nothing can drift across it while a cell builds, and it gives the two arms the
**same identities**, so what remains can be compared record by record.

The clause arm runs first, on the coldest cache the process has, and the
conditional arm runs afterwards on a store two passes have warmed. The order is
stacked against the expected result on purpose.

## Memory

| what the table keeps | declared clause, p50 µs | conditional delete, p50 µs | ratio |
|---|---|---|---|
| 5 000 | 808.8 | 4 104.5 | 5.1× |
| 10 000 | 763.9 | 7 116.2 | 9.3× |
| 20 000 | 774.8 | 12 266.2 | 15.8× |
| 40 000 | 801.8 | 23 120.7 | 28.8× |

**The clause is flat.** 808.8 → 763.9 → 774.8 → 801.8 µs across an eight-fold
table: a 5.9 % spread, **not monotonic**, and the largest table is 0.9 % cheaper
than the smallest. It costs what it removes.

**The condition is linear in what it keeps.** It fits
`1 388 µs + 0.543 µs per record kept` within 4.1 % at all four points.

That slope is worth one line of its own: **W168 measured the same conditional
form in a different workload and fitted `888 µs + 0.549 µs per record kept`.**
Two independent runs, different tables, different identities, and the per-record
term agrees to about 1 %. The intercepts differ because the records here are
larger and there are a thousand more of them.

## Disk

| what the table keeps | declared clause, p50 µs | conditional delete, p50 µs | ratio |
|---|---|---|---|
| 5 000 | 1 797.6 | 8 521.0 | 4.7× |
| 10 000 | 1 769.5 | 12 149.4 | 6.9× |
| 20 000 | 1 856.4 | 17 440.2 | 9.4× |
| 40 000 | 2 223.5 | 33 169.9 | 14.9× |

**On disk the clause is nearly flat and not exactly flat** — **1.24×** across the
eight-fold table, against **3.89×** for the condition. That is reported rather
than rounded away.

The same 1.24× appeared in W168 for the span form on the same backend, against
the same 3.85× for the condition. Two different mechanisms showing the same
residual slope on the same backend points at the backend rather than at either
mechanism: a larger table is more SST files, so a bounded scan opens more of them
even when the range it reads is unchanged. **That is a hypothesis and not a
result** — nothing here counted files — and it is written down so the next person
tests it rather than re-deriving it.

## What the timings cannot say

Both arms left **40 000 records, identical, compared record by record** rather
than by count. That check is load-bearing: W165 measured two reads over one
window answering the same count and different records, so a count would have
reported that as agreement.

## What these numbers are not

They are the cost of the pass **including its commits**. A removal here is a
`DELETE` like any other — sequenced into the log, carried over the protocol,
visible on the change feed — so this is not a scan figure and must not be read as
one. The pass commits every 512 records, so a thousand-record window is two
commits at every table size; that is constant across the sizes, which is what the
shape question needs.

They also say nothing about **when** the pass should run. Nothing schedules it,
deliberately (Q-497).

## Reproducing

```
cargo run -p tessari-bench --release -- --workload series --backend memory
cargo run -p tessari-bench --release -- --workload series --backend disk
```
