# What a claim costs, and what makes it cost more (S5.5)

- backend: `memory` and `disk`
- build: release profile
- machine: `macos` / `aarch64`
- workload: `queue` — `cargo run -p tessari-bench --release -- --workload queue`
- method: a separate queue per depth, prefixes built with the ordinary `CLAIM`
  statement rather than by writing the engine's fields; 20 timed claims per
  depth; drains of 2 000 records with the finishing `DELETE` outside the timer

The design of `DEFINE QUEUE` states the cost in one line and leaves it to be
measured: **the claim walk is O(held + dead-lettered), and only the held half
heals itself.** This is the measurement. It confirms the law, refutes the
implied asymmetry in the cost, and finds a third term the design does not name.

## Claim latency against an unclaimable prefix

| prefix | memory p50 µs | disk p50 µs |
|---|---|---|
| 0 held | 21.0 | 83.8 |
| 0 dead-lettered | 19.2 | 76.3 |
| 1 000 held | 391.8 | 618.9 |
| 1 000 dead-lettered | 368.8 | 615.9 |
| 5 000 held | 1 836.2 | 2 781.1 |
| 5 000 dead-lettered | 1 796.8 | 2 832.3 |
| 20 000 held | 7 365.5 | 10 990.2 |
| 20 000 dead-lettered | 7 387.5 | 10 878.1 |

The memory column was taken three times across two builds of the harness. The
prefix rows agree to within 3% at every depth — 21.0 / 21.2 and 7 365 / 7 389 at
the extremes — so the figures below are a property of the store rather than of
one run.

**The law is linear and the fit is close.** Memory: 20 µs plus **0.368 µs per
record passed over** predicts 388 / 1 860 / 7 376 against 380 / 1 816 / 7 376
measured — within 2.5% at every point. Disk: 80 µs plus **0.543 µs per record**
predicts 623 / 2 795 / 10 934 against 617 / 2 807 / 10 934. Four points rather
than two, because two cannot tell a line from a curve.

**Held and dead-lettered cost the same, and that is the finding.** At 20 000 the
two differ by 0.3% in memory and 1.0% on disk — inside the run-to-run spread. The
walk does not care *why* a record is unclaimable, so the design's asymmetry is
entirely about what happens **next** and not at all about what a claim pays: a
held record's contribution expires by itself, a dead-lettered one's does not.
An operator reading only the design could reasonably have assumed the
dead-lettered path was the expensive one. It is not; it is the *permanent* one.

## The third term: a finished record goes on costing

| drain of 2 000 | memory | disk |
|---|---|---|
| p50 µs | 272.0 | 639.0 |
| max µs | 520.9 | 1 176.3 |

These rows drain a queue that is **kept clean** — every claim is finished with a
`DELETE` before the next one is timed, with the delete outside the timer. A flat
line was expected. The line ramps: from ~20 µs to 520 µs in memory and ~84 µs to
1 176 µs on disk, over 2 000 finished records, which is **0.25 µs and 0.55 µs per
deleted record** — on disk, the same slope a *live* skipped record charges.

So the walk is O(entries in the table's key range), not O(claimable records).
`walk_table` steps the key range and resolves versions, and a record that has
been deleted is still a key in that range until the store compacts it away.

**What this measurement does not establish** is whether compaction returns the
cost. Every figure above is from a single process inside one run; no compaction
was forced and none was observed. The honest statement is that a `DELETE`
does not return the walk's cost *promptly*, and that the recovery of it is
unmeasured. Raised as **Q-468**.

It matters because the design's §7 answers the dead-letter cost with an
operator's retention statement — `DELETE FROM jobs WHERE attempts >= 5` — and
this is the first evidence that the statement does not act on the walk as
immediately as that sentence implies.

## Throughput, and what a batch buys

| drain of 2 000 | memory p50 µs | disk p50 µs |
|---|---|---|
| one at a time | 272.0 | 639.0 |
| 100 at a time | 579.8 | 1 406.1 |

The second row is **one statement claiming a hundred records**. At the same queue
depth, a hundred single claims cost 27.2 ms in memory and 63.9 ms on disk; one
batched claim of a hundred costs 0.58 ms and 1.41 ms. That is **47× and 45×**,
and the mechanism is visible in the numbers rather than assumed: the batch pays
the walk **once per statement** and then a small per-record cost of about 5.8 µs
(memory) and 14.1 µs (disk).

This is the number behind the design's advice that a worker with steady work
should claim a batch. It is now a figure rather than a recommendation.

## The saturating resource

**One core, executing the walk.** The evidence is the memory column: it performs
no I/O at all and shows the same linear law at 0.368 µs per record passed over,
so the cost is the per-record work — one key step, one version resolution, one
payload decode — and not storage. The disk column charges 0.543 µs for the same
step, and the 0.175 µs difference is the block iterator. Had the cost been I/O
bound, the memory line would have been flat.

This is a narrower claim than the one this directory's README declines to make.
The README is right that deciding whether CPU, the block cache or the device ran
out under load needs the engine's own counters; what is named here is only what
bounds **one claim's latency**, and it is named because there is a control for
it — a backend that does no I/O and charges the same linear cost.

Nothing about a claim is parallel: one `CLAIM` is one session on one thread
walking one key range. So a single worker's ceiling is exactly the reciprocal of
the row above it — **47 600 claims/s at an empty head, 135/s behind 20 000
unclaimable records** in memory, 11 900/s and 91/s on disk.

**Under contention the binding constraint is different and it has been measured
separately.** `crates/tessari-cli/tests/queue_broker.rs` ran four consumer
processes against one node: 60 hand-outs cost **~178 refusals**, three refused
statements per successful claim. At that width throughput is bounded by the
write-write conflict rate rather than by the walk, and the batch row above is
the mitigation for both at once — it amortises the walk *and* the contention over
the batch without changing the ordering.

## What to do with these numbers

**A queue whose head stays clean is fast and the depth is what to watch.** 20 µs
is not a cost anybody needs to manage; 7.4 ms is, and the distance between them
is entirely the number of records a claim steps over. The operational figure is
therefore not "claims per second" but **how deep the unclaimable prefix is
allowed to get**, and there is currently nothing in the store that reports it.

**An index over the claimable predicate is the fix if one is ever needed**, and
this measurement is what would justify building it — it was deliberately not
built speculatively. The threshold is legible here: the walk overtakes a
millisecond at roughly 2 700 records in memory and 1 700 on disk.
