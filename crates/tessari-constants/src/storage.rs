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
///
/// The budget is a count and not a duration, which only works because the
/// attempts are spread apart by [`COMMIT_BACKOFF_STEP`]. Re-racing immediately
/// makes a count budget a measure of how fast the machine is rather than of how
/// contended the store is — see that constant.
pub const MAX_COMMIT_ATTEMPTS: u32 = 8;

/// How many commits one group write may carry.
///
/// Unit: commits, counted as the flusher EXAMINES them and checked before it
/// takes the next one — never as it emits them — so a full group never takes one
/// more than this.
///
/// Concurrent commits share one engine write and one device sync (G040 SG4).
/// The group takes whatever is staged when its flush starts and never waits for
/// more, so under a writer that keeps up it is one commit and the bound is never
/// met; it exists for the backlog, where an unbounded group would make one
/// write — and every writer waiting on it — as long as the queue.
///
/// Sixty-four is provisional until measured on the `concurrent` workload.
pub const MAX_GROUP_COMMITS: usize = 64;

/// How many bytes of keys and values one group write may carry.
///
/// Unit: bytes, checked before the next commit is taken, so a group exceeds it
/// by at most the one commit that crossed it.
///
/// Beside [`MAX_GROUP_COMMITS`] because either bound alone leaves the other
/// unbounded: sixty-four commits of large values would make a group of hundreds
/// of megabytes held in memory before it is written.
///
/// Four mebibytes is provisional in the same sense.
pub const MAX_GROUP_BYTES: usize = 4 * 1024 * 1024;

/// How long a commit waits before re-racing for the committed tail, doubling
/// each time it loses.
///
/// Unit: microseconds.
///
/// # Why waiting at all is the point
///
/// Losing the race means another commit landed between reading the tail and
/// applying. Re-reading and re-racing **immediately** is what turns a busy store
/// into a contended one: every loser retries at once, into the same instant, and
/// collides with every other loser. The budget above then runs out — not because
/// the store is saturated, but because nothing ever spread the writers apart.
///
/// The failure that shape produces is worse than slow, because it is
/// **load-dependent**. On an idle machine the interleaving is thin and eight
/// attempts are plenty; on a busy one, each attempt takes longer, more
/// competing commits land inside it, and the same code gives up sooner the
/// busier the host is. A caller then sees a store that refuses writes in
/// proportion to how much else the machine is doing.
///
/// Doubling makes the spread grow to match the contention rather than being
/// guessed in advance, which is the property a fixed pause does not have.
///
/// Fifty microseconds is a starting value: below the cost of the apply it
/// follows, so an uncontended retry is not noticeably delayed, and far enough
/// above thread-scheduling granularity to actually separate two racers.
/// Provisional until measured under a real write workload.
pub const COMMIT_BACKOFF_STEP: u64 = 50;

/// The longest a commit waits between attempts, however many it has lost.
///
/// Unit: microseconds.
///
/// Doubling without a ceiling would put the last attempts of a long run several
/// seconds apart, and a caller blocked that long would rather have been told the
/// store is contended. With the values here, a commit that loses every attempt
/// is refused after roughly twenty-five milliseconds of waiting — long enough to
/// have genuinely tried, short enough that contention is reported while it is
/// still actionable.
pub const COMMIT_BACKOFF_CEILING: u64 = 8_000;

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

/// How many log records one history read may walk before it gives up.
///
/// A history is a **reverse** read: it starts at the newest record and walks
/// back until it has enough events for the object it was asked about. For a
/// record written recently that is a handful of records. For one last touched
/// a million commits ago it is the whole log, finding nothing the entire way —
/// and the caller cannot tell the two cases apart before asking.
///
/// So the walk is bounded rather than the answer. An unbounded read here would
/// be O(log) work behind a screen that looks like a point lookup, which is the
/// shape that is fast in every test and ruinous on the one store that has been
/// running for a year. Exhausting this budget is not an error: it is reported,
/// and the history says it is incomplete.
///
/// Two thousand is a starting value — an order above `SKIP_BATCH_RECORDS`
/// because a history reads to *find* rather than to discard, and a screen that
/// shows nothing is worse than one that took longer. Provisional until measured
/// against a real log.
pub const HISTORY_SCAN_RECORDS: usize = 2_048;

/// How many events one `INFO FOR HISTORY OF` answers with.
///
/// A timeline is read, not processed: the question behind it is *what recently
/// happened to this*, and an answer nobody scrolls to the bottom of costs the
/// reader nothing and the store everything. Fifty is a screenful and then some.
///
/// Separate from [`HISTORY_SCAN_RECORDS`] because they bound different things —
/// this bounds the ANSWER, that bounds the WALK — and a reader who confuses them
/// reads a short answer as a truncated log. Provisional until the panel's
/// timeline has been used against a real store.
pub const HISTORY_EVENTS: usize = 50;

/// How many quarantined messages one Kafka consumer keeps findable (Q-708).
///
/// Past it the oldest of the lowest partition goes. A thousand is generous for a
/// failure an operator is meant to look at, and small enough that a consumer fed
/// nothing but poison costs a bounded corner of the store.
pub const KAFKA_QUARANTINE_HELD: usize = 1_000;

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

/// How many records a table must hold before the planner will decline an index
/// in favour of reading the table.
///
/// Unit: records.
///
/// The planner serves an index only when it can produce at most half the table
/// (§ `plan::worth`). That rule exists because an index read walks entries
/// *and* fetches records, so one returning most of a table has added a walk to
/// a read it did not shorten — measured at **2.1× slower than no index at all**.
///
/// Below this floor there is nothing to protect. A scan of a thousand records
/// is one round trip to the substrate, the ratio the rule reasons about is a
/// ratio between two costs that are both negligible, and the only thing the
/// guard could achieve is to surprise somebody who declared an index and
/// watched it go unused. So the guard does not engage, and a small table plans
/// exactly as it did before the guard existed.
///
/// It is the same magnitude as [`RANGE_SCAN_BATCH_ENTRIES`] and deliberately
/// **not** the same constant: that one is a buffer bound whose value that
/// constant's own doc calls indifferent, and coupling a planning decision to it
/// would mean re-tuning a buffer silently re-planned every query in the store.
pub const PLANNER_SCAN_FLOOR_RECORDS: u64 = 1024;

/// How many of an index's most common values its statistics keep.
///
/// Unit: values.
///
/// A skewed column is the case an even spread gets wrong — four records in ten
/// under one value and the rest under a thousand others — and the common
/// values are what let an equality on the heavy value be estimated as heavy.
/// Sixteen covers the skew that changes a plan; a value outside the sixteen is
/// estimated from the spread of the rest, which is where a rare value belongs.
pub const STATISTICS_COMMON_VALUES: usize = 16;

/// How many equi-depth buckets an index's statistics divide its first field
/// into.
///
/// Unit: buckets.
///
/// A range is estimated by the buckets it covers, a whole bucket for one it
/// covers and half for one it cuts, so the error is at most a bucket at each
/// end: one sixty-fourth of the index twice over, which is well inside the
/// band the scan guard decides in.
pub const STATISTICS_BUCKETS: usize = 64;

/// How many entry changes an index's statistics outlast before they are no
/// longer used, at the least.
///
/// Unit: index entries added or removed.
///
/// A statistic is used while what changed since it was taken is at most a
/// tenth of the entries it counted, or this many, whichever is larger. The
/// floor keeps a small index's statistic from going stale on every handful of
/// writes; past it, a statistic describing a distribution that has moved by
/// more than a tenth is set aside and the planner counts instead, as it did
/// before statistics existed.
pub const STATISTICS_STALE_FLOOR_CHANGES: u64 = 1_000;

/// How many indexes one housekeeping pass refreshes the statistics of, at most.
///
/// Unit: indexes.
///
/// Taking a statistic walks every entry of the index, so a pass that refreshed
/// every stale index at once after a bulk load would be one long walk per
/// index in a single tick. Four per pass spreads that over seconds, and an
/// index waiting its turn is planned by counting — slower, never wrong.
pub const STATISTICS_PER_PASS: usize = 4;

/// How many records each log keeps when nobody configured a number (ADR-0094 D2).
///
/// Unit: log records, per log. Overridden per node by `TESSARIDB_RETAIN_RECORDS`
/// (a number, or `none` for unbounded) and per store by `DEFINE NODE RETAIN`.
///
/// The owner's figure (G049): a bounded log is the default because an unbounded
/// one is a disk that fills, and the routine backup is a state snapshot, which
/// does not need the history below the window.
pub const DEFAULT_LOG_RETENTION_RECORDS: u64 = 100_000;

/// How many records one `CLAIM` may take.
///
/// Unit: records held by a single statement.
///
/// A bound rather than a tuning knob, and the reason is the one the consumer's
/// own batch already gave: without a ceiling, one statement holds the whole
/// queue for the whole timeout and every other worker waits — with nothing
/// anywhere in an error state, because a claim that takes everything is doing
/// exactly what it was asked to do.
///
/// Five hundred, which is not a fresh guess: it is the ingest consumer's
/// `BATCH`, chosen there against the same question — how many records is it
/// reasonable to move in one transaction — and answering one question twice with
/// two numbers is how a store ends up with two answers nobody can tell apart.
///
/// Unlike the search caps above, this one **refuses**. They decline to serve a
/// candidate and fall back to a path that returns the identical result; there is
/// no cheaper path that answers `CLAIM 100000` correctly, so the honest response
/// is to name the ceiling rather than to quietly hand back fewer records than
/// were asked for.
pub const MAX_CLAIM_RECORDS: u64 = 500;
