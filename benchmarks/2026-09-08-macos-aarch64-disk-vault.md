# TessariDB benchmark

- backend: `disk`
- build: release build
- machine: `macos` / `aarch64`
- records per workload: 2000
- percentiles: nearest-rank over every retained sample, per phase

A baseline is comparable with another taken on the same machine and the same
build profile, and with nothing else. It is read by a person; it is not a gate.

## vault

a sealed write and a REVEAL against the same operations without a vault — what the audit's committed transaction costs a read

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| vault-write | 500 | 10126 | 89.2 | 133.1 | 200.3 | 277.0 |
| plain-write | 500 | 13073 | 74.0 | 85.6 | 124.5 | 144.2 |
| vault-reveal | 500 | 12419 | 76.1 | 95.9 | 130.1 | 339.5 |
| plain-read | 500 | 78617 | 11.7 | 12.0 | 15.7 | 497.0 |

## The same workload on the memory backend

Taken minutes earlier on the same machine and build, and kept here because the
comparison is what makes the attribution possible — one backend alone gives four
numbers and no reading.

| phase | ops/s | p50 µs | p90 µs | p99 µs |
|---|---|---|---|---|
| vault-write | 35217 | 28.0 | 29.2 | 36.6 |
| plain-write | 62526 | 15.8 | 16.5 | 17.8 |
| vault-reveal | 53950 | 17.5 | 18.3 | 23.9 |
| plain-read | 166122 | 4.9 | 5.2 | 6.6 |

## What these say

**A `REVEAL` is priced like a write.** On disk it costs 76.1 µs against a point
read's 11.7 and an ordinary `CREATE`'s 74.0 — six and a half times a read, and
slightly *more* than a write. That is not a surprise so much as a confirmation:
the audit record is a committed transaction, so a vault read does what a write
does and then some.

**The commit is the dominant term, and the two backends are how we know.** The
overhead of a `REVEAL` over a point read is **64.4 µs on disk and 12.6 µs in
memory**. The cryptography cannot account for that difference — it is the same
code on both, and the write side prices it at 15.2 µs (disk) and 12.2 µs
(memory), which is about as backend-independent as a measurement gets. What is
left after subtracting it scales with the backend by fifty-fold, and the thing
that does that is the commit.

So roughly **three quarters of a vault read's cost on a durable backend is the
audit write**, and that estimate is conservative in the direction that matters:
opening an envelope does less work than sealing one (no key generation, no fresh
wrap), so the true crypto term is smaller than the 15.2 µs subtracted and the
commit's share is larger.

**The saturating resource is the commit path**, shared with every writer. The
consequence is worth stating plainly because it is not what "a read" usually
implies: `REVEAL` throughput does not scale like a read. It contends with the
store's write throughput, and a vault under read load is a write workload.

**Reproducibility.** Two independent disk runs minutes apart: `vault-reveal` p50
76.2 and 76.1 µs, p99 128.5 and 130.1, `plain-read` p50 11.6 and 11.7. The
outlier columns (`max`) are not reproducible and are not read as anything.
