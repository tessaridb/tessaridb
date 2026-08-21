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

## What the first baselines said

Recorded 2026-08-22 on macOS / aarch64, 2000 records per workload, release build.
Two of these are worth more than the numbers.

**A nearest-neighbour read costs the same on both backends** — 12.6 ms in memory
and 12.8 ms on disk. It is therefore **CPU-bound in the distance computation**,
not I/O-bound, and an index helps by reducing how many distances are computed
rather than how many pages are read. That is the case for the HNSW index stated
as a measurement instead of an intuition, and it says the benefit does not depend
on the engine underneath.

**A scan is CPU-bound and an index read is I/O-bound.** `filter-scan` is nearly
the same on both backends (880 µs / 1043 µs) because the cost is decoding and
testing two thousand records; `filter-index` doubles on disk (51 µs / 102 µs)
because the cost is reads. The index still wins by 10× on disk.

The rest, for the record:

| Claim | In memory | On disk |
|---|---|---|
| an index beats the scan of the same equality | 880 µs → 51 µs (17×) | 1043 µs → 102 µs (10×) |
| a search index beats the scan of the same term | 1600 µs → 7.2 µs (222×) | 1760 µs → 16 µs (110×) |
| ranking costs little over the search it ranks | 7.2 µs → 11.6 µs | 16 µs → 24 µs |
| durability costs about seven times a write | 7.1 µs | 48.5 µs |

Every one of those was a claim some earlier wave made about cost. None of them
had a number until this harness existed.

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
