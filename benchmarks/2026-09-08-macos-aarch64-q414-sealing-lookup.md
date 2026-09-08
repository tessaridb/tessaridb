# What sealing costs a write that is not a vault write (Q-414)

- backend: `memory` and `disk`
- build: release profile
- machine: `macos` / `aarch64`
- workload: `write` — 2000 point writes of a small record, one statement each
- method: the same binary twice, differing only in whether `seal_secrets`
  performs its table lookup; source restored byte-identical before the second
  baseline was taken

`seal_secrets` opens with `Catalog::new(transaction).table(address.table)`, a
point read at the transaction's snapshot, in the session's single record-write
choke point. A vault is rare; the lookup is not. Q-414 asked what it costs a
write that is not a vault write, and said the answer was a measurement.

## Memory backend — where a point read is visible

| | p50 µs, three runs | mean | ops/s |
|---|---|---|---|
| as shipped | 11.8 · 11.8 · 11.9 | **11.83** | 81 127 |
| lookup elided | 10.6 · 10.8 · 10.8 | **10.73** | 91 093 |

**1.1 µs per record write**, against a within-set spread of 0.1 µs — ten times
the spread, so the signal is real. That is 9.3% of an in-memory write.

## Disk backend — where a production store lives

| | p50 µs, three runs | mean | spread |
|---|---|---|---|
| as shipped | 67.6 · 67.1 · 68.0 | 67.57 | 0.9 |
| lookup elided | 65.8 · 63.1 · 67.8 | 65.57 | 4.7 |

**The delta is 2.0 µs and the counterfactual's own spread is 4.7 µs**, so the
term is not resolvable here. On two of the three runs the build *without* the
lookup reported fewer ops/s than the build with it, which is the clearest
possible statement that what is being read is scheduling noise rather than the
lookup.

## What this says

**The cost is real and it is 1.1 µs.** It is 9% of an in-memory write and it
disappears under the commit on a durable one. Both halves matter: quoting the
9% alone would overstate it for a service, and quoting the disk result alone
would claim the lookup is free, which it is not.

**The counterfactual cannot run the vault workload**, and that is a property of
the experiment rather than a gap in it — with sealing elided there is nothing
for `REVEAL` to open. `write` is the honest instrument anyway: it is the write
that is not a vault write, which is exactly the population the question is about.

## What was found beside the number

`writable`, which runs at the top of every record-writing statement, already
performs the identical read — `Catalog::new(transaction).table(address.table)` —
asks `is_bucket`, and drops the definition. `seal_secrets` then reads the same
row again at the same snapshot to ask `is_vault`. So the 1.1 µs is a **duplicate
read** rather than a necessary one, which is a better position than the question
anticipated: removing it needs no cache and introduces no second source of truth.

It was **not removed**, and the reason is the disk column. The change would
thread a `TableDefinition` into the sealing path from somewhere other than the
sealing path's own read, on the one code path whose failure direction is
plaintext at rest, to buy something this measurement cannot see on the backend a
production store runs. Recorded as Q-431 with the safe shape worked out, so it is
a decision waiting on a demand rather than something to rediscover.
