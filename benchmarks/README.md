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
