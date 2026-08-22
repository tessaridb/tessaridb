//! Tunable constants for `bgv-db`.
//!
//! Every magic number in the workspace lives here, named, typed, and documented
//! with its unit and the reasoning behind its value. Business and engine code
//! never carries a bare numeric literal.

#![forbid(unsafe_code)]

/// How many times a commit re-attempts after losing the race for the committed
/// tail, before reporting contention to the caller.
///
/// Unit: attempts.
///
/// Each lost attempt means a concurrent commit landed between reading the tail
/// and applying the batch, so the conflict check has to be redone. The budget
/// is bounded rather than unlimited because a caller waiting forever is worse
/// than a caller told the store is contended: contention is information, and a
/// silent spin is not.
///
/// Eight is a starting value chosen to absorb ordinary interleaving on an
/// embedded store while still surfacing sustained contention quickly. It is
/// provisional until measured under a real write workload.
pub const MAX_COMMIT_ATTEMPTS: u32 = 8;

/// How many log records a subscription reads at a time while skipping a backlog
/// it has decided not to receive.
///
/// Unit: log records.
///
/// A skip counts exactly what it discards, by reading it — an inexact drop count
/// is a number nobody can act on. That read is bounded so the memory a skip
/// needs does not scale with how far behind the subscriber fell, which is
/// precisely the situation a skip exists for.
///
/// Two hundred and fifty-six is a starting value: large enough that skipping a
/// long backlog is not dominated by round trips, small enough that one batch of
/// decoded changes is a bounded allocation. Provisional until measured against a
/// real backlog.
pub const SKIP_BATCH_RECORDS: usize = 256;

/// How many index entries a bounded ordered read fetches at a time.
///
/// Unit: index entries.
///
/// A read serving `ORDER BY … LIMIT n` from an index cannot ask for exactly `n`
/// entries: an entry may point at a record the reader cannot see, so the number
/// of entries a bound needs is not known before they are read. Descending adds a
/// second reason — the tie group at the bound has to be drained past it, because
/// walking backwards yields a tie group in the reverse of the order the answer
/// wants. It fetches in batches instead.
///
/// Shared by both directions, which is why it is not named for one of them.
///
/// One hundred and twenty-eight is a starting value: enough that the ordinary
/// case — a bound in the tens, no ties, every entry resolving — finishes in one
/// round trip, and small enough that a degenerate ordering (every record sharing
/// one value) walks the index in bounded steps rather than materialising it.
/// Provisional until measured against a real index.
pub const ORDERED_SCAN_BATCH_ENTRIES: usize = 128;

/// How many index entries a range read fetches at a time.
///
/// Unit: index entries.
///
/// Separate from [`ORDERED_SCAN_BATCH_ENTRIES`] because the two batches are
/// answers to different questions. A bounded descending read may stop early, so
/// its batch is a **guess** at how far it has to walk and a large one is work
/// thrown away. A range read has no early stop — every entry between the bounds
/// is part of the answer — so its batch is only a bound on how many entries are
/// held at once, and fetching more of them per round trip costs nothing but the
/// buffer.
///
/// The value is **measured, and the measurement says the size is not the
/// lever**. Over a fifty-thousand-entry range on the `range` workload, one
/// hundred and twenty-eight and one thousand and twenty-four differ by less than
/// the run-to-run spread, while both sit about three megabytes — four per cent —
/// below the same read taken in a single fetch, at the same latency. What the
/// constant buys is therefore the **bound** and not its value: the entries held
/// at once stop being proportional to the width of the range, which is a few per
/// cent at fifty thousand entries and an order of magnitude at five million.
///
/// A thousand and twenty-four is taken from the indifferent band as the fewer
/// round trips, and it is a few tens of kilobytes.
pub const RANGE_SCAN_BATCH_ENTRIES: usize = 1024;

/// How quickly repeating a term stops improving a BM25 score.
///
/// Unit: dimensionless.
///
/// A relevance score that counted occurrences linearly could be lifted without
/// limit by repeating one word, which is both wrong about language and an open
/// invitation to anyone writing the documents. `k1` is the point at which each
/// further occurrence buys noticeably less than the last: the term's
/// contribution approaches `k1 + 1` times its weight and never exceeds it.
///
/// `1.2` is the value the literature settled on across TREC collections, and it
/// is the value nearly every production engine ships. It is a starting value
/// here for the same reason it is elsewhere — nothing in this project has been
/// measured against a labelled relevance set, and a number chosen without one
/// would be a guess dressed as a decision.
pub const BM25_K1: f64 = 1.2;

/// How much a document's length is held against it in a BM25 score.
///
/// Unit: dimensionless, in `0.0..=1.0`.
///
/// At `0.0` length is ignored entirely, so a long document holding a term once
/// ranks with a short one that does — which favours whichever document happens
/// to be longest. At `1.0` length is fully normalised away, which over-punishes
/// the long document that is genuinely the better answer.
///
/// `0.75` is the standard compromise and the same starting value `BM25_K1` is.
/// Both become options on the index definition when there is a measurement to
/// justify a different value; until then, changing either in a release changes
/// ranking order without any statement changing, which is stated in
/// `docs/bgvql.md` rather than left to be discovered.
pub const BM25_B: f64 = 0.75;

/// How far past its bound an ordered read under a condition may walk the index
/// before giving the order up and taking the scan.
///
/// Unit: multiples of the bound the statement asked for.
///
/// An index-served order under a `WHERE` walks in the sort's order and re-tests
/// each record against the whole condition, because the index narrows and the
/// condition decides. So filling a bound of ten may take more than ten entries,
/// and how many more depends on how selective the condition is over the order —
/// which is exactly the distribution statistic this store deliberately does not
/// keep (`docs/bgvql.md` §8).
///
/// The ceiling is what turns that unknown into a cost rather than a risk. Past
/// it the condition is not selective enough for the order to be worth serving
/// from the index, and the read falls back to the scan it would have taken
/// anyway. **It bounds the cost and never the answer**: every exit is either an
/// ordered answer that filled the bound or the scan.
///
/// `32` because the retries double, so reaching the ceiling costs about twice
/// the ceiling in entries — a few hundred for a bound of ten, against a scan of
/// the whole table. A condition matching one row in thirty-two is still served;
/// one matching one in a thousand is not, and paying a full scan for it is the
/// right answer rather than a walk that reads most of the index in batches.
pub const ORDERED_FILTER_REACH: usize = 32;
