# What a range delete costs, and whether it reads the table whole

- date: 2026-09-09
- machine: `macos` / `aarch64`
- build: release
- workload: `retention` (`cargo run -p tessari-bench --release -- --workload retention`)
- goal: **G023**, criterion **S4.2**

## The question

S4.2 asks that retention *"drops a range without reading the table whole"*, and
the specification makes the claim testable in its own words:

> it costs what it removes rather than what it keeps: the conditional form reads
> every record it is going to keep, once per run, forever.

That is a claim about **shape**, not about which arm is faster. So the removed
window is held constant at 1 000 records and the table grows eight times, from
5 000 to 40 000. A cost that follows what is removed stays put; a cost that
follows what is kept grows with the table.

Both arms name the same records — the identity and the field `n` carry the same
number, so `DELETE FROM t:1000..2000 LIMIT ALL` and
`DELETE FROM t WHERE n >= 1000 AND n < 2000 LIMIT ALL` remove one set. Each cell
is built and measured three times on a freshly built table, because a delete is
destructive and the second arm cannot run on what the first left. The span arm
runs first, on the coldest cache in the process — the order is stacked against
the result it produced.

## In memory

| table | span delete p50 µs | conditional delete p50 µs | conditional ÷ span |
|---|---|---|---|
| 5 000 | 1 325.7 | 3 667.0 | 2.8× |
| 10 000 | 1 278.5 | 6 378.1 | 5.0× |
| 20 000 | 1 348.1 | 12 103.6 | 9.0× |
| 40 000 | 1 385.2 | 22 847.9 | **16.5×** |

**The span arm is flat.** Across an eight-fold table it moves 4.5 % end to end,
and it is not monotonic — the 10 000 table is the fastest of the four — so what
is left is noise and not a trend. Per removed record: **1.33 µs**.

**The conditional arm is linear in what it keeps**, and the law fits at every
point. Taking the slope between 10 000 and 40 000 gives

    conditional ≈ 888 µs + 0.549 µs per record kept

which predicts 3 633 µs at 5 000 against 3 667 measured (0.9 %) and 11 868 µs at
20 000 against 12 104 (1.9 %). A four-point fit within two per cent is not a
coincidence of two numbers.

The divergence **is** the finding: the two arms are 2.8× apart on the small table
and 16.5× apart on the large one, and the ratio keeps opening because only one of
them is paying for the records it is going to keep.

## On disk

| table | span delete p50 µs | conditional delete p50 µs | conditional ÷ span |
|---|---|---|---|
| 5 000 | 4 281.0 | 8 216.9 | 1.9× |
| 10 000 | 4 002.3 | 10 858.4 | 2.7× |
| 20 000 | 4 057.9 | 18 848.4 | 4.6× |
| 40 000 | 5 315.7 | 31 668.1 | **6.0×** |

**On disk the span arm is nearly flat and not exactly flat**, and this is stated
rather than rounded away. It holds at about 4 100 µs from 5 000 to 20 000 and
then rises to 5 316 µs at 40 000 — **1.24× across an eight-fold table**, where a
read of the whole table would have cost eight. The rise sits in the median of
three samples rather than in one outlier, so it is probably real; three samples
is not enough to characterise it, and no stronger claim is made here. The
plausible mechanism is that a bounded key range in an LSM store still consults
more files as the store grows, which is a cost of the storage layout and not of
the statement — testing that is a separate wave.

The conditional arm grows **3.85×** over the same range, fitting

    conditional ≈ 3 918 µs + 0.694 µs per record kept

with a looser fit than in memory (10 % at 5 000, 5.6 % at 20 000).

## Correctness, which the timings cannot speak to

At the largest table, both arms left **39 000 records** and the two remainders
are **identical, compared record by record** rather than by count. Two deletes
can leave the same number of different records, and a count would report that as
agreement.

## What this settles, and what it does not

**Settled.** The span form does not read the table whole. In memory its cost is
independent of table size to within measurement noise; on disk it is independent
to within 1.24× across a factor of eight, against 3.85× for the conditional form
over the same range. The specification's sentence is confirmed on both backends,
strictly in memory and with the disk caveat above.

**Not settled, and not by this wave.** Whether retention should be a *declared
clause* rather than a statement — the rest of criterion S4.2 — is a decision and
not a number, and a declared background retention is separately refused in the
specification. S4.2 therefore stays **PARTIAL** with this gap closed and that one
open; see Q-479.

**A sentence in the profile that this makes precise.** `CLAUDE.md` records that
*"`DELETE` still reads the table whole, deliberately"*. That is true of the
conditional form and is exactly what the right-hand column above shows. It is not
true of the span form, which is the point of having one.
