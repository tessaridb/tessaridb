# Commits from several writers at once: racing for the version against queueing for it

- backend: `disk` (and `memory` for the first pair)
- build: release profile
- machine: `macos` / `aarch64`, ten cores
- workload: `concurrent` — `cargo run -p tessari-bench --release -- --workload concurrent --backend disk`
- method: 1, 2, 4, 8 and 16 threads, each inserting 200 distinct records into one table, one commit per
  record, released together by a barrier. Throughput is landed commits over the phase's wall clock; a refusal
  is counted, never timed.

Every commit moves the store-wide version by one and asserts the value it read, so two writers that read the
same version cannot both land. Until this change the writers of one process **raced** for it: the loser threw
its attempt away, waited, and tried again, eight times at most, and was then refused. Now they **queue** for it
at one gate held from the read to the apply.

The two builds below differ in that one thing: the gate's two acquisitions were replaced by a unit value for
the "racing" binary. They were run alternately, three times each, on the same machine in one session.

## Sixteen writers, disk

| run | build | landed | refused | commits/s | p50 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|---|
| 1 | racing | 2 940 | 260 | 8 133 | 126.3 | 7 467.8 | 14 058.9 |
| 1 | queueing | 3 200 | 0 | 11 482 | 1 390.1 | 1 566.4 | 1 710.9 |
| 2 | racing | 2 945 | 255 | 7 992 | 133.8 | 7 674.1 | 16 392.3 |
| 2 | queueing | 3 200 | 0 | 11 435 | 1 393.2 | 1 597.5 | 1 729.5 |
| 3 | racing | 2 962 | 238 | 7 802 | 144.5 | 7 578.7 | 15 389.9 |
| 3 | queueing | 3 200 | 0 | 8 938 | 1 788.2 | 2 200.2 | 3 812.9 |

## Eight writers, disk

| run | build | landed | refused | commits/s | p99 µs |
|---|---|---|---|---|---|
| 1 | racing | 1 551 | 49 | 8 310 | 4 688.8 |
| 1 | queueing | 1 600 | 0 | 11 646 | 820.0 |
| 2 | racing | 1 550 | 50 | 8 505 | 4 614.2 |
| 2 | queueing | 1 600 | 0 | 11 463 | 843.3 |
| 3 | racing | 1 552 | 48 | 8 561 | 5 544.5 |
| 3 | queueing | 1 600 | 0 | 8 979 | 1 216.9 |

## Memory, one run each

| writers | racing: landed / refused, commits/s | queueing: landed / refused, commits/s |
|---|---|---|
| 4 | 799 / 1, 31 964 | 800 / 0, 41 635 |
| 8 | 1 593 / 7, 30 648 | 1 600 / 0, 37 265 |
| 16 | 3 149 / 51, 29 874 | 3 200 / 0, 37 002 |

## What this says

- **The mechanism is certain: refusals go to zero.** A writer in this process can no longer lose its turn to
  another, so nothing is refused for contention on writes that share no record.
- **The magnitude of the throughput change is a range, not a number:** +10 % to +45 % at sixteen writers on
  disk across three alternating runs. The third queueing run is the low one and nothing in this file explains
  why; it is reported rather than dropped.
- **The median moves the other way, and that is the trade taken on purpose.** Racing let a few writers win
  quickly while others lost repeatedly, so its median was low and its tail was the price. Queueing gives every
  writer a turn in order, so the median at sixteen writers is about sixteen service times and the tail is about
  the same — p99 falls from 7.5 ms to 1.6–2.2 ms and the slowest commit from 14–16 ms to 1.7–3.8 ms.
- **Throughput still does not grow with writers.** Commits are serial by construction and each synced batch is
  one device sync; folding several commits into one batch and one sync is the step that would change that, and
  it is not taken here.
