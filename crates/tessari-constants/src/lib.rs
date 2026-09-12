//! Tunable constants for TessariDB.
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
///
/// The budget is a count and not a duration, which only works because the
/// attempts are spread apart by [`COMMIT_BACKOFF_STEP`]. Re-racing immediately
/// makes a count budget a measure of how fast the machine is rather than of how
/// contended the store is — see that constant.
pub const MAX_COMMIT_ATTEMPTS: u32 = 8;

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
/// `docs/tessariql.md` rather than left to be discovered.
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
/// keep (`docs/tessariql.md` §8).
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

/// The largest single WebSocket frame this node will read from a client.
///
/// Unit: bytes.
///
/// A frame header declares its own payload length before a byte of that payload
/// arrives, and a reader that believes the declaration allocates whatever a
/// stranger asked it to. That is a memory-exhaustion bug with a polite name, so
/// the ceiling is checked against the declared length and the frame is refused
/// before anything is reserved for it.
///
/// Sixty-four kilobytes because of what a client actually sends on this route: a
/// subscription request naming a position and a table, which is tens of bytes. A
/// frame three orders of magnitude larger than the only message the protocol
/// defines is a client fault or an attack, and either way the honest answer is a
/// close rather than an allocation.
pub const SOCKET_MAX_FRAME_BYTES: usize = 64 * 1024;

/// How many connections one serving surface holds open at once.
///
/// Unit: connections.
///
/// # Why a ceiling exists at all
///
/// A thread per connection is this node's deliberate model, and the paragraph
/// that justifies it does not bound it. Without a ceiling the node has no point
/// at which it refuses: it degrades, and then fails to spawn a thread, and the
/// failure arrives at whichever connection happened to be next rather than at
/// the one that caused it. A number here turns that into an answer a client can
/// read.
///
/// # Why this number
///
/// Each connection costs one operating-system thread, which reserves stack
/// address space — eight megabytes by default on Linux and macOS — plus a
/// scheduler slot and whatever the session holds. Four hundred is comfortably
/// inside what a modest machine runs without the scheduler becoming the cost,
/// and comfortably above what an embedded caller or a small deployment reaches.
/// The crate documentation for the wire protocol names the trigger for changing
/// the *model* — idle subscribers in the tens of thousands — and this constant
/// is what makes reaching that trigger visible rather than fatal.
///
/// It is per **surface** rather than per process: the wire protocol and the HTTP
/// endpoint each hold their own door, so a flood of one cannot starve the other
/// of the places it needs to answer a health check.
///
/// Provisional in the same sense as [`MAX_COMMIT_ATTEMPTS`] — chosen to be
/// obviously safe rather than measured, and the refusal count is what a
/// deployment tunes it from.
pub const MAX_CONNECTIONS: usize = 400;

/// How long a freshly accepted connection has to send its greeting.
///
/// Unit: seconds.
///
/// # The attack this closes
///
/// `accept` returns, the node blocks reading the greeting, and a client that
/// sends **nothing at all** holds that thread for the life of the process. It
/// costs the client one socket and no traffic, which is why it is the cheapest
/// way to take a thread-per-connection node down, and why it needs no
/// credential. A deadline on the first read is what makes the cost symmetric.
///
/// # Why only the greeting
///
/// The deadline is cleared once the greeting arrives, and deliberately so. After
/// it, the node is reading a *statement*, and a session that is idle between
/// statements is the ordinary state of an interactive prompt — a deadline there
/// would disconnect the normal case in order to bound the abnormal one, which
/// [`MAX_CONNECTIONS`] already bounds.
///
/// Ten seconds because a greeting is a handful of bytes and any network that
/// cannot deliver them in ten seconds cannot carry a query either.
pub const GREETING_SECONDS: u64 = 10;

/// How often a node is expected to learn something about its peers.
///
/// Unit: seconds.
///
/// This is the period a node greets its peers on: the serving process runs a
/// cadence at exactly this interval, dialling every peer the catalog declares.
/// It is a **declaration** rather than a measurement, which is the same thing
/// MongoDB's `heartbeatFrequencyMS` is: the number the floor below is derived
/// from is configured, never observed.
///
/// Ten seconds for the reason [`GREETING_SECONDS`] is ten: it is short enough
/// that a failure is noticed while somebody still cares, and long enough that a
/// cluster of any size is not spending its bandwidth on being sure of itself.
pub const AWARENESS_SECONDS: u64 = 10;

/// How often a follower collects the records it does not hold.
///
/// Unit: seconds.
///
/// # Its own constant, because it fails differently
///
/// It is numerically equal to [`AWARENESS_SECONDS`] today and it is not that
/// constant: a missed greeting costs the freshness of a routing reading, and a
/// missed collection costs data. Two mechanisms whose failures differ get two
/// periods, so that changing one is not silently changing the other.
///
/// # Where the number comes from
///
/// The API refuses a staleness bound tighter than [`STALENESS_FLOOR_SECONDS`],
/// so a follower has to be able to satisfy the tightest bound the API admits.
/// One collection period plus the transfer has to fit inside that floor; at half
/// of it, a follower that misses a whole round is still inside the promise. A
/// period at or above the floor would mean advertising a bound this node cannot
/// meet even when everything is working.
pub const COLLECTION_SECONDS: u64 = AWARENESS_SECONDS;

/// The most records one collection carries.
///
/// Unit: records.
///
/// It is also what makes *level* observable: a peer serves `min(limit,
/// available)`, so an answer shorter than this is the peer saying it had no
/// more, and an answer exactly this long is contact rather than arrival. A
/// ceiling too high would make a catching-up follower hold one connection for a
/// whole log; too low and a follower that fell behind never catches up, because
/// each round carries less than the interval produced.
pub const COLLECTION_RECORDS: u64 = 1024;

/// How long a canvass of the voting members takes on this network, end to end.
///
/// Unit: seconds.
///
/// # It is a deadline, not a measurement
///
/// A candidate stands when its remaining writable window has shrunk to two of
/// these, because a round that yields a lease dated from when it **opened** has
/// to be in hand before the old fence shuts — and one round time lands exactly
/// on the fence with nothing left for a round that is refused, lost or slow. Two
/// is the latest opening that still allows one complete retry.
///
/// One second is a canvass of three to seven members on a local network, each a
/// TLS handshake and one frame each way. It is the value this build ships and
/// not a property of the engine: the day a cluster spans a region, this is the
/// number that moves, and everything derived from it moves with it.
pub const ROUND_SECONDS: u64 = 1;

/// How often a leader checks whether it is time to stand again.
///
/// Unit: seconds.
///
/// # Where the number comes from
///
/// It is bounded by the window between *time to stand* and *the fence shuts*,
/// and that window is exactly `2 × ROUND_SECONDS`: a holder's usable span begins
/// at `LEASE_TTL - LEASE_GUARD` and standing opens when two round times are left
/// of it. A cadence slower than that window can step straight over the moment it
/// was supposed to act on, and a leader would then lose a lease it could have
/// renewed — while nothing anywhere reported a failure, because no round was
/// ever attempted.
///
/// So the period is half the window, which leaves room for one tick to be late.
/// A check costs nothing when there is margin left: the decision to stand is
/// taken **before** any socket is opened, precisely so that a frequent cadence is
/// not a frequent canvass.
pub const CAMPAIGN_SECONDS: u64 = ROUND_SECONDS;

/// The tightest staleness bound a read may ask for.
///
/// Unit: seconds.
///
/// # Why a floor exists at all
///
/// A read may say how far behind a node answering it is allowed to be. A bound
/// tighter than the interval at which this node learns anything about its peers
/// is a promise nothing can check — it would be enforced against a picture whose
/// age exceeds the tolerance it is being compared to. Refusing it, with the
/// floor named, is what keeps the bound a guarantee rather than a hope.
///
/// # Twice the interval, and why not MongoDB's ninety
///
/// One interval to learn something, and one more to notice that we did not.
///
/// MongoDB refuses a `maxStalenessSeconds` below **90 seconds**, and that number
/// comes from its client-side topology refresh and its idle-write period — two
/// mechanisms this engine does not have. Taking the 90 would be taking a value
/// whose derivation is absent, so what is taken is the **shape**: a floor
/// derived from the interval at which the system learns, published in the
/// refusal, and refused rather than silently raised.
pub const STALENESS_FLOOR_SECONDS: u64 = AWARENESS_SECONDS * 2;

/// The largest reassembled WebSocket message this node will read from a client.
///
/// Unit: bytes.
///
/// Separate from the frame ceiling because a message may legally arrive as many
/// fragments, so bounding one frame bounds nothing: a sender can fragment
/// without limit and a reader that only checks each piece accumulates forever.
/// The two ceilings answer two different questions and neither implies the
/// other.
///
/// Equal to the frame ceiling at this size, deliberately — the only message this
/// route defines fits in one frame, so fragmentation here is a proxy's doing
/// rather than a client's need, and a proxy does not enlarge what it forwards.
pub const SOCKET_MAX_MESSAGE_BYTES: usize = 64 * 1024;

/// How many cells a spatial index writes per record's geometry.
///
/// Unit: cells.
///
/// A record's covering is one entry per cell, so this is directly the index's
/// write amplification for a geometry: a point produces one entry whatever the
/// budget, and a country produces up to this many. It bounds the *store* rather
/// than the query, and the two want opposite things — more cells approximate the
/// shape more tightly and cost more to write, so the trade is per workload and
/// this is the workload-free starting point.
///
/// Sixteen because the covering halves its error roughly per level and stops as
/// soon as the next subdivision would exceed the budget, so a budget of sixteen
/// buys two full levels of refinement past the first cell that meets the box.
/// Eight was the alternative and refines one level less, which for an elongated
/// shape — a river, a road, a coastline, the shapes a bounding box already
/// serves worst — leaves the covering close to the box it started from.
///
/// It is a bound and not a target. A geometry needing fewer cells writes fewer,
/// and the covering keeps a **coarser** cell rather than dropping a finer one
/// when the budget runs out, so exceeding it costs candidates to refine and
/// never rows.
pub const SPATIAL_INDEX_CELLS_PER_RECORD: usize = 16;

/// How many cells a **query** box is covered by.
///
/// Unit: cells.
///
/// The other side of the same trade, and it is not the same number for the same
/// reasons. A record's covering is paid once per record at write time and
/// forever after in space; a query's is paid once per read and in nothing else,
/// so a query can afford to be finer. What it cannot afford is unboundedly
/// finer: each cell of a query covering costs one range scan **plus one lookup
/// per level above it**, so the read's fixed cost is linear in this number while
/// the candidates it saves are not.
///
/// Sixteen, which is the record budget, and deliberately so until something is
/// measured. Symmetry is the honest starting point when the only argument for
/// asymmetry is that a query covering is cheaper — that says the number could be
/// larger, not what it should be. The candidate-to-result ratio is instrumented
/// precisely so this can be moved on evidence rather than on the intuition in
/// this paragraph.
///
/// It is a bound and not a target, with the same guarantee: the covering keeps a
/// coarser cell rather than dropping a finer one, so exhausting the budget costs
/// candidates to refine and never rows.
pub const SPATIAL_QUERY_CELLS: usize = 16;

/// How many entries a nearest-first walk will read from a cell's whole subtree
/// before it descends into that subtree instead.
///
/// Unit: index entries.
///
/// The walk over cells is best-first, and the tree it walks is **implicit**:
/// every cell exists at every level whether or not anything was ever written
/// there. Without a cut-off, reaching one record a kilometre away in an empty
/// region means opening a cell at each of the thirty-two levels on the way down,
/// and each of those is a seek that finds one entry or none.
///
/// So a cell is first read as a whole subtree, with a limit one above this
/// number. A short answer means the scan was not truncated — every entry under
/// that cell is in hand, the walk ranks them all and never descends. Only a
/// subtree that fills the limit is worth splitting into four.
///
/// Sixty-four, because four levels of descent cost four seeks and four scans to
/// find what one scan of sixty-four entries returns outright, and a region
/// holding fewer than this many records is not a region a walk needs to be
/// clever about. Larger wastes reads inside a dense cell that pruning would have
/// skipped; smaller reinstates the deep chain this exists to cut.
pub const SPATIAL_WALK_SUBTREE_ENTRIES: usize = 64;

/// How many password verifications this process will run at once.
///
/// Unit: concurrent verifications.
///
/// # What this bounds, and why it is not the connection ceiling
///
/// A password hash is deliberately expensive — [`PASSWORD_HASH_MEMORY_KIB`] of
/// memory and tens of milliseconds of CPU, by design, because that is what makes
/// an offline crack of a stolen catalog slow. Online and unbounded, the same
/// property makes an **amplifier**: one TCP write costs the attacker nothing and
/// costs this node nineteen mebibytes, and **no attempt has to be valid**, so no
/// credential is needed to spend the memory.
///
/// [`MAX_CONNECTIONS`] does not close it. Four hundred connections all
/// presenting a credential is several gigabytes, which is the multiplication
/// rather than the bound.
///
/// # Why twenty-four, and why a refusal
///
/// The number is derived rather than chosen: the budget this node is willing to
/// lose to authentication is **512 MiB** — enough to matter, small enough to
/// leave the store's cache standing — and one verification costs
/// [`PASSWORD_HASH_MEMORY_KIB`], so the budget buys twenty-six and twenty-four
/// is that with room left. A test asserts the product against the budget, so the
/// two cannot drift apart quietly.
///
/// It was eight first, which was a number nobody derived, and being a third of
/// its own stated budget it refused sign-ins a database has no business
/// refusing: nine clients authenticating at once is an ordinary Tuesday, not an
/// attack. A bound set below ordinary use is not a security control, it is an
/// outage that only fires under load.
///
/// Refused rather than queued, for the reason the door itself gives: a queue
/// moves the unbounded growth instead of removing it, and a client told to come
/// back can, while a client parked in a queue cannot even tell that it is
/// waiting.
///
/// The cost is real and belongs here rather than in a surprise: a fleet
/// reconnecting all at once signs in twenty-four at a time and the rest are
/// refused and retry. That is the trade a bound is.
pub const MAX_SIGN_IN_VERIFICATIONS: usize = 24;

/// How long a session token stays good for.
///
/// Unit: seconds.
///
/// Twelve hours, which covers a working day without covering the night after
/// it. The number is a trade between two costs that pull opposite ways: a short
/// life sends every client back through the memory-hard sign-in
/// [`MAX_SIGN_IN_VERIFICATIONS`] exists to bound, and a long one widens the
/// window in which a token copied off the wire is still worth having.
///
/// It bounds the window and not the damage. What actually revokes a token is
/// the user record changing under it — a rotated password, a corrected role, a
/// removal — and that takes effect on the very next request rather than at
/// expiry. This constant is what covers the case nobody noticed and so nobody
/// revoked.
pub const SESSION_TOKEN_SECONDS: u64 = 12 * 60 * 60;

/// How many session tokens one node will hold at once.
///
/// Unit: tokens.
///
/// A bound on memory a caller who *does* hold a valid credential could
/// otherwise grow without limit: signing in successfully is not throttled — only
/// failing is — so nothing else stands between one account and an unbounded
/// table.
///
/// Ten per connection slot ([`MAX_CONNECTIONS`]), because a client that
/// reconnects gets a new connection and may reasonably still hold its old
/// token. At roughly two hundred bytes an entry the whole table is under a
/// mebibyte, which is the point: it is cheap enough that the bound can be
/// generous and still be a bound.
///
/// Reaching it **refuses to issue** rather than evicting somebody else's live
/// token. Eviction would make minting tokens a way to sign other people out.
pub const MAX_SESSION_TOKENS: usize = MAX_CONNECTIONS * 10;

/// How many times one identity may fail to sign in before it is made to wait.
///
/// Unit: consecutive failures.
///
/// Three, because a person mistyping a password twice is ordinary and a third
/// consecutive miss is where a human stops guessing and goes to look the
/// password up. A success clears the count, so the allowance is renewed by
/// getting it right rather than by waiting.
pub const FREE_SIGN_IN_FAILURES: u32 = 3;

/// How long an identity waits after its first throttled failure, doubling with
/// each further one.
///
/// Unit: milliseconds.
///
/// # Why doubling, and why this is a delay rather than a lockout
///
/// A fixed delay is a fixed guess rate, and a fixed guess rate is still a rate:
/// an attacker willing to spend a week gets a week's worth of guesses out of it.
/// Doubling makes the total attempts available in any window logarithmic instead
/// of linear, which is the difference between slowing an attack and ending it.
///
/// It is not a lockout, and that is deliberate. A permanent lockout hands an
/// attacker a denial of service against any account whose name they know — they
/// need no password to fail three times. A delay that decays to nothing when the
/// attack stops costs a real user a pause and costs an attacker the attack.
///
/// A quarter of a second is below the point where a person retrying by hand
/// notices, and it is already five times the cost of the verification it is
/// standing in front of.
pub const SIGN_IN_BACKOFF_MILLIS: u64 = 250;

/// The longest an identity waits between sign-in attempts, however many it has
/// missed.
///
/// Unit: milliseconds.
///
/// Doubling without a ceiling reaches days, and a real user who mistyped a
/// password six times would be locked out in everything but name — which is
/// exactly what [`SIGN_IN_BACKOFF_MILLIS`] argues against. Thirty seconds is long
/// enough that a sustained attack is measured in attempts per hour and short
/// enough that a person who went to find their password comes back to a store
/// that will talk to them.
pub const SIGN_IN_BACKOFF_CEILING_MILLIS: u64 = 30_000;

/// How much memory one password hash is made to cost.
///
/// Unit: kibibytes.
///
/// # Why this number is written down at all
///
/// It is Argon2id's `m` parameter, and it was previously whatever the hashing
/// crate's `Default` said. That is the thing **LR-DB-004** forbids: a default
/// that was never read is not evidence, and a routine dependency upgrade that
/// moved it would change this store's password-hashing posture with nothing in
/// the repository, the decision records or the tests showing that it had
/// happened. Read from `argon2` 0.5.3 and recorded in ADR-0043, along with the
/// two below.
///
/// Nineteen mebibytes is the second of OWASP's Argon2id options — the one paired
/// with two passes — and memory is the parameter that actually resists a GPU,
/// because a GPU has thousands of cores and not thousands of memory channels.
///
/// It is also, directly, the amplification factor
/// [`MAX_SIGN_IN_VERIFICATIONS`] exists to bound: raising it makes a stolen
/// catalog harder to crack **and** makes an unauthenticated attempt more
/// expensive to serve, so the two constants are read together or neither is
/// understood.
pub const PASSWORD_HASH_MEMORY_KIB: u32 = 19_456;

/// How many passes one password hash is made to take over that memory.
///
/// Unit: passes.
///
/// Argon2id's `t`. Two, which is what OWASP pairs with nineteen mebibytes: at a
/// fixed cost budget, memory buys more resistance than iterations do, so the
/// passes are the parameter kept low. Recorded with the crate version in
/// ADR-0043 for the reason [`PASSWORD_HASH_MEMORY_KIB`] gives.
pub const PASSWORD_HASH_PASSES: u32 = 2;

/// How many lanes one password hash is spread across.
///
/// Unit: lanes.
///
/// Argon2id's `p`. One, so a verification is one thread's work. Parallelism here
/// would divide the wall-clock cost of a single hash by spending more of the
/// machine on it, which on a serving node is the wrong direction twice over: the
/// point of the cost is that it is paid, and a node under an authentication
/// flood would multiply its own load by the lane count. Recorded in ADR-0043.
pub const PASSWORD_HASH_LANES: u32 = 1;

/// The shortest prefix `MATCHES PREFIX` will accept.
///
/// Unit: characters, after the field's non-stemming filters have been applied.
///
/// **Contract, not tuning.** A prefix below this is refused by name and the
/// refusal states the limit; it is not merely served slowly. The reason is that
/// the cost of a prefix is the size of its expansion, and the expansion of a
/// short prefix is a large fraction of the whole vocabulary — `a` reaches every
/// word beginning with `a`, which on English prose is roughly one word in
/// fourteen. A reader gains nothing from that answer and the store pays for all
/// of it, on the query a frustrated reader retries.
///
/// Three rather than two, because two is where the fraction stops being small:
/// `th` alone reaches a tenth of an English vocabulary. Three is also the
/// conventional floor in the engines that offer this, which matters less than
/// the reason but is worth not contradicting without one.
pub const SEARCH_PREFIX_MINIMUM: usize = 3;

/// How many distinct terms one prefix may expand to before the index declines
/// to serve it.
///
/// Unit: terms.
///
/// **Not a refusal.** A prefix expanding past this is answered by the scan
/// instead, and `EXPLAIN` reports `scan` — the answer is identical either way,
/// which is the rule this store holds everywhere: *which access path runs is
/// decided by what exists; the answer is not.* A cap that refused would make a
/// query succeed without an index and fail once somebody added one.
///
/// What the cap protects is the **index** path, where an expansion is a union of
/// that many posting lists. Sixty-four is enough for every prefix a person
/// actually types at three characters or more and small enough that the union
/// stays cheaper than the scan it replaces.
pub const SEARCH_PREFIX_EXPANSION_CAP: usize = 64;

/// The most edits `MATCHES FUZZY` will look through.
///
/// Unit: single-character insertions, deletions and substitutions — Levenshtein,
/// not Damerau: a transposition costs two.
///
/// **Contract, not tuning.** A query asking for more is refused by name and the
/// refusal states the limit. Two is the number because the candidate set grows
/// with the alphabet raised to the edit count: at one edit a word of length `n`
/// has about `53n` neighbours over lowercase letters and digits, at two about
/// `1400n`, and at three the neighbourhood is larger than most vocabularies —
/// at which point every query matches something and the operator has stopped
/// discriminating rather than started being generous.
///
/// It is also the point where a match stops being a plausible reading of what
/// somebody meant. `cat` and `dog` are three edits apart.
pub const SEARCH_FUZZY_MAX_EDITS: usize = 2;

/// How many leading characters of a fuzzy query are **not** fuzzy.
///
/// Unit: characters, after the field's non-stemming filters have been applied —
/// the same footing as [`SEARCH_PREFIX_MINIMUM`].
///
/// **Semantics, not an index-side bound**, and this is the distinction that
/// matters. A stored term satisfies a typed word only if it is within
/// [`SEARCH_FUZZY_MAX_EDITS`] edits **and** shares this many first characters,
/// and the scan applies exactly that rule. Implementing the prefix only as a
/// dictionary-walk bound would be cheaper to write and would break the rule this
/// store holds everywhere: *which access path runs is decided by what exists;
/// the answer is not.* The same statement would return one set on a table with
/// no index and a smaller set once somebody added one (ADR-0046).
///
/// The cost is real and belongs in the documentation rather than in a surprise:
/// a mistake inside the first `SEARCH_FUZZY_PREFIX` characters is not found.
/// `xector` does not reach `vector`. That is accepted because the alternative is
/// a walk of the whole term dictionary per word, which is the denial of service
/// this operator would otherwise be — and because a first letter is the part of
/// a word people mistype least, having usually just read it.
pub const SEARCH_FUZZY_PREFIX: usize = 3;

/// How many distinct terms one fuzzy word may match before the index declines to
/// serve it.
///
/// Unit: terms **matched**, not terms examined — the two differ here, which they
/// do not for a prefix walk, and the difference is what G022's S3 measures.
///
/// **Not a refusal**, for the same reason [`SEARCH_PREFIX_EXPANSION_CAP`] is not
/// one: only the index can evaluate it, so a cap that refused would make a
/// statement succeed without an index and fail once somebody added one. Past it
/// the candidate is not offered and the scan answers, which is the identical
/// answer by a different path.
///
/// Sixteen rather than the prefix cap's sixty-four. A prefix expansion is a set
/// a reader chose the size of by typing fewer letters; a fuzzy expansion is a
/// set the *store* chose, and a word with sixteen near-spellings in the corpus
/// is one where the union has stopped being cheaper than reading the records.
pub const SEARCH_FUZZY_EXPANSION_CAP: usize = 16;

/// How many terms one fuzzy word's dictionary walk may **read** before the index
/// declines to serve it.
///
/// Unit: terms examined — the other side of [`SEARCH_FUZZY_EXPANSION_CAP`], and
/// the reason the two exist separately.
///
/// This ceiling is what the design did not have and the implementation needed.
/// Intersecting an edit-distance automaton with a dictionary by **walking a
/// range** reads every term sharing the mandatory prefix and keeps the few
/// inside the budget, so the work is bounded by the popularity of a three-letter
/// beginning rather than by the number of matches. Without a ceiling, a common
/// beginning in a large vocabulary is a denial of service costing the caller one
/// request — which is precisely the failure G022's S3 exists to catch.
///
/// **Not a refusal**, on the same reasoning as the two caps above: past it the
/// candidate is not offered and the scan answers with the identical result.
///
/// A thousand and twenty-four, because a term read is a key decode and a bounded
/// character comparison while the alternative it defers to is a record decode —
/// perhaps two orders of magnitude more work. Examining a thousand terms to
/// avoid reading tens of records is the trade this number makes, and it stops
/// being a good one somewhere past here.
pub const SEARCH_FUZZY_EXAMINATION_CAP: usize = 1024;

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

/// How deep a view may be expanded before the read is refused.
///
/// Unit: view expansions along one chain.
///
/// A view names a read, and that read may name another view, so expansion
/// recurses and needs a floor to stop on. Eight, for two reasons that pull in
/// opposite directions and meet here: deep enough that no view written by hand
/// meets it — a view over a view over a view is already unusual — and shallow
/// enough that the refusal arrives before the parse and materialisation cost of
/// eight nested reads has been paid.
///
/// **A cycle is caught by this and gets no second mechanism.** Two views naming
/// each other cannot avoid the counter, and the refusal prints the chain it
/// followed, so the cycle is legible in the message. A dedicated cycle detector
/// would produce a better sentence for a case this already stops, and would then
/// have to be kept in step with it.
///
/// A value of this layer, like the ceiling on a held read: nothing in a stored
/// definition records it, so a view means the same thing on a node that changes
/// it.
pub const MAX_VIEW_DEPTH: usize = 8;
