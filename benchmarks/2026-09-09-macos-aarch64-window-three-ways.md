# One window over identity, read three ways (G023 S4.1)

- backend: `memory` and `disk`
- build: release profile
- machine: `macos` / `aarch64`
- workload: `span` — `cargo run -p tessari-bench --release -- --workload span`
- fixture: 50 000 records, identity and the field `n` carrying the same number,
  so the three reads are asking one question; windows of 1 000 (two per cent of
  the table); 100 windows per phase; three runs per backend

The criterion is G023 **S4.1**: *a time or identity window is answerable as a key
range rather than a scan or a redundant index*, evidenced by *the same predicate
measured three ways, the record set identical in all three, and the range path
fastest*. Both halves are here, and the first is the one a timing table cannot
speak to.

## The order of the phases is against the result

The span runs **first**, on the coldest cache there is, and it touches two per
cent of the table. The scan runs second and reads everything, warming it. The
value index runs last, on a table two full passes have already warmed.

That arrangement is the opposite of the flattering one. A span that wins from the
coldest position wins for a reason other than its turn.

## The records answered

One window, all three ways, compared **record by record** rather than by count —
because two reads over the same window can return the same number of different
records, which is what the late-arrival measurement in
`2026-09-09` (ADR-0058) found for a different pair of reads, and a count would
have reported it as agreement.

| | span | value index | scan |
|---|---|---|---|
| records answered | 1 000 | 1 000 | 1 000 |
| the three sets | **identical** | | |

Identical in all six runs.

## What each path costs

p50 µs, three runs per backend.

### memory

| the read | run 1 | run 2 | run 3 | against the span |
|---|---|---|---|---|
| by span | 339.0 | 340.2 | 339.9 | — |
| by value index | 850.0 | 869.8 | 852.8 | **2.5× slower** |
| by scan | 17 181.6 | 17 273.8 | 17 309.5 | **50.9× slower** |

### disk

| the read | run 1 | run 2 | run 3 | against the span |
|---|---|---|---|---|
| by span | 411.8 | 409.1 | 409.8 | — |
| by value index | 1 513.2 | 1 474.3 | 1 483.8 | **3.6× slower** |
| by scan | 20 630.3 | 20 616.5 | 20 641.2 | **50.4× slower** |

The run-to-run spread is under 1% in every cell, so these are properties of the
paths rather than of one run.

## What the numbers say

**The span is the fastest of the three on both backends, and it consults
nothing.** That is the criterion met, and the margin over the scan — about 50× on
both — is the uninteresting half: the scan reads fifty thousand records to answer
a thousand, so it was always going to lose by roughly the ratio of those two
numbers, and it does.

**The interesting margin is against the value index, and it is the smaller one.**
2.5× in memory, 3.6× on disk. An index over `n` is a second copy of an ordering
the primary key already has, and the cost of consulting it is exactly what the
span saves: the index read walks entries and then fetches the records those
entries name, while the span walks the records. That the disk margin is the wider
one is the same mechanism the earlier index work measured — an entry walk and a
record fetch are two reads on a backend where a read costs something.

**The index also has to be built, and the build is not free.** 51.3 ms in memory
and 56.6 ms on disk, once, for a table this size — roughly sixty windows' worth
of span reads before the index has repaid its own construction, and it is still
slower per read afterwards. For a window over identity specifically, the index is
a cost with no return. For the reads it *also* serves, it is a different trade
and this workload does not measure it.

## What is deliberately not measured

- **Whether the index is worth having for other reads.** It serves equalities and
  orders this workload never asks for. The build row is reported so the trade can
  be seen, not so it can be settled here.
- **A window that returns most of the table.** At that width the planner declines
  the index and the scan answers, which is a measured property of the guard
  (`plan::worth_serving`) rather than of these three paths.
- **Late arrival.** A span is a window over arrival order, and whether that is
  the window the caller wanted is a different question, answered in ADR-0058.
