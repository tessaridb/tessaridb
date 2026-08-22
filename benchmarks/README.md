# Benchmarks

Baselines recorded by `bgv-db-bench`. Each file is machine-generated; the harness
writes it and nobody edits it, so a number in one is a number something produced.

```
cargo run -p bgv-db-bench --release -- --list
cargo run -p bgv-db-bench --release -- --baseline benchmarks/<date>-<machine>-memory.md
cargo run -p bgv-db-bench --release -- --backend disk --baseline benchmarks/<date>-<machine>-disk.md
```

## What a baseline is, and what it is not

**Comparable with another taken on the same machine and the same build profile,
and with nothing else.** The file records both, because a number without them
invites a comparison that means nothing.

**Read by a person, deliberately.** These are not a CI gate. A shared runner's
timings vary by more than the regressions worth catching, so an automatic check
would either fail constantly or be loosened until it never failed — and a check
that never fails is worse than none, because it is also believed.

**Percentiles are exact and per phase.** Every sample of a phase is retained,
sorted, and indexed by nearest rank, so a reported latency is one that was
observed. A cumulative histogram reports a healthy p99 straight through a real
regression, which is the failure this avoids rather than economises on.

## What the first baselines said, and what they corrected

Recorded 2026-08-22 on macOS / aarch64, 2000 records per workload, release build.

**The harness found a ten-millisecond bug on its first day, and then disproved
the conclusion drawn from its own first run.** That sequence is worth keeping.

The first run showed a nearest-neighbour read at 12.5 ms and — because it cost
the same in memory as on disk, where every other workload differs by a factor —
the conclusion recorded was "CPU-bound in the distance computation". True about
the CPU, wrong about the distance.

Decomposing it settled the question. Sorting the same 2000 records by a plain
path costs 1.5 ms; the same distance with a **one**-component query vector costs
2.3 ms, and with four or eight components it costs the same again. A short query
is a length mismatch, so the distance returns `+∞` before doing any arithmetic —
and every record scoring `+∞` means the sort has nothing to order. The cost
appeared only at the width where the keys finally differed.

So the time was in the **sort**. The value system defines the order across
numbers by their decimal projections, so comparing two floats converts both — and
a sort compares each key about `log n` times. Two thousand float keys meant some
forty thousand conversions to answer twenty-two thousand questions. Projecting
each key once, eagerly, hands the comparator the identical decimals it would have
computed: **12.5 ms → 3.7 ms**, with a test asserting that every pair of values
compares the same way before and after.

| Claim | In memory | On disk |
|---|---|---|
| an index beats the scan of the same equality | 849 µs → 46 µs (18×) | 1043 µs → 102 µs (10×) |
| and the scan of the same ordered range | 849 µs → 105 µs (8×) | 1013 µs → 273 µs (3.7×) |
| a search index beats the scan of the same term | 1569 µs → 7.0 µs (224×) | 1760 µs → 16 µs (110×) |
| ranking costs little over the search it ranks | 7.0 µs → 11.7 µs | 16 µs → 24 µs |
| durability costs about seven times a write | 7.4 µs | 48.5 µs |
| a nearest-neighbour read over 2000 × 32 dimensions | 3.65 ms | 3.80 ms |
| the same read served by a vector index | 0.62 ms (5.9×) | 0.83 ms (4.6×) |

The scan row is still nearly identical across backends, so the read is CPU-bound
in the distance — which is why an index helps by computing **fewer** of them, and
why its benefit barely depends on the engine underneath. Recall is reported on
every run at 100% of the exact ten, which the scan supplies: the scan's ten *are*
the right ten, so there is nothing to argue about.

**The recall number was 1% for an hour, and the fixture was why.** The generator
took its jitter modulo sixty, which made thousands of records share a vector
exactly — and recall measured over duplicates is a measurement of which tie a sort
broke. It read as a broken index through two wrong fixes before the data was
looked at. A benchmark fixture is an input like any other and deserves the same
suspicion as the thing it measures.

**A scan is CPU-bound and an index read is I/O-bound.** `filter-scan` is nearly
the same on both backends because the cost is decoding and testing two thousand
records; `filter-index` doubles on disk because the cost is reads. The index
still wins by 10× on disk, but the *shape* of the win differs by backend, which a
single-backend harness would have hidden.

## What the `range` workload was built to settle, and what it found instead

Recorded 2026-08-22 on the same machine. The workload reads one indexed range at
four widths — 100, 5 000, 25 000 and 50 000 of 50 000 records — and reports the
process's resident set at each, because the question was whether resolving a
range's entries in bounded fetches is worth doing.

**It is worth doing, and it is not where the memory is.** Three runs each, memory
backend, peak growth over the widest read:

| the entries fetched | peak growth, three runs | p50 |
|---|---|---|
| all at once | 75904 / 74896 / 74320 KiB | 42.4–44.1 ms |
| in batches of 1024 | 71792 / 72624 / 71728 KiB | 42.6–44.8 ms |

About three megabytes, four per cent, at no cost in time. The bands do not
overlap, which is the only reason the difference is reportable at all: a single
run of each would have been inside the spread.

**The batch size is not the lever.** 128 and 1024 differ by less than the
run-to-run spread. What the constant buys is that the entries held at once stop
being proportional to the width of the range — a few per cent at fifty thousand
entries, an order of magnitude at five million. A bound is not a percentage.

**The other ninety-six per cent is the answer itself**, and the workload is what
made it visible: reading fifty thousand records costs about **1.4 KiB of resident
memory per record answered**, for records whose stored form is a couple of
hundred bytes. Every record between the bounds is resolved and held before the
caller sees the first one, and no change inside the storage layer can alter that
— the condition that asked is re-tested above it, so a limit cannot be pushed
down without the planner and the executor consuming the answer as it arrives.
Recorded as an absence in `docs/bgvql.md` §8 with these numbers behind it.

**The `served by` row exists because the phase can silently stop measuring what
it claims.** If a planner change stopped serving the range from the index, the
timings would read as a regression in the range read rather than as the loss of
one. The row names the access path and the record count on every run.

## What batching the record resolution changed

Recorded 2026-08-22 on the same machine, three runs of the widest read (50 000
records) per backend, against the immediately preceding commit — not against the
numbers above, for a reason worth stating: those were taken four waves earlier,
so a difference against them would be attributable to anything landed in
between. A before/after is only a before/after when the two trees are adjacent.

Resolving the records an index range names used to cost **one backend round trip
per record** — 2 002 for 2 000. It now costs **two per entry batch**: 4 for the
same 2 000, one scan for the entries and one batched read for their records.

| the widest read, p50 | before | after |
|---|---|---|
| on disk | 102.4 / 104.9 / 105.0 ms | 68.7 / 68.8 / 68.9 ms |
| in memory | 43.8 / 43.5 / 44.6 ms | 43.5 / 43.5 / 43.5 ms |

**The disk read is about 1.5× faster and the in-memory one is unchanged, which
is the result that says what the cost actually was.** Reading one record is a
range, because records are versioned and the visible one is the newest at or
below the snapshot. On the engine every range read builds an iterator that pins
the store's view while it lives, so fifty thousand records meant fifty thousand
of them; the batched path builds one and seeks it. In memory there is no
iterator to build — the saving is a few lock acquisitions — so the number does
not move, and that it does not is the control.

Resident memory is unchanged on both backends: the bands overlap in every
direction. The bound from the section above still holds and is still the larger
cost — the answer itself, not how it was fetched.

## What is deliberately not measured here

- **Concurrency.** The store is single-writer (ADR-0007), so a concurrent write
  benchmark would mostly measure contention on the applied position — a real
  question, and a different one.
- **The saturating resource.** The harness reports what it can observe from
  inside the process. Deciding whether CPU, the block cache or the device ran out
  needs the engine's own counters and an operator reading them; the readiness
  checklist is where that judgement is recorded, and these numbers are what it is
  made with.
- **Ranking quality.** `k1` and `b` want a labelled relevance set. Latency is not
  relevance.
