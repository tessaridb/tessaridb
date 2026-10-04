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

/// How long a node waits for another to answer a request it carried there on
/// its caller's behalf (ADR-0108 D1), per read on the peer link.
///
/// Longer than a greeting because the far side runs the caller's statement:
/// a read that takes a minute there is still an answer, and the caller is
/// waiting for it either way. Bounded because a peer that took the request and
/// went silent must not hold the caller's connection forever.
pub const COORDINATED_SECONDS: u64 = 120;

/// How many rounds of the failover policy a transaction across leaders'
/// `PENDING` record stays live before anyone may abort it (ADR-0112 D7).
///
/// In rounds rather than seconds so it moves with the policy an operator set:
/// a cluster tuned to fail over in a second should not hold intents for a
/// minute behind a coordinator that died. More than one round, because the
/// coordinator waits for a majority on every prepare and a slow follower is
/// not a dead coordinator.
pub const ACROSS_LAPSE_ROUNDS: u32 = 4;

/// How long a sign-in waits to ask the cluster's one budget (ADR-0108 D5)
/// before deciding on this node's own count.
///
/// Short, because the caller is waiting and a password hash follows: a leader
/// that does not answer in this long is treated as unreachable for this try,
/// which costs at most one node's allowance while it lasts.
pub const SIGN_IN_ASK_MILLIS: u64 = 500;

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
/// One second, the interval gossip-based clusters commonly refresh on (G053 C2b). Ten, the
/// value until 0.21, made a follower that lost its leader wait up to ten seconds
/// to learn who replaced it, and put the staleness floor at twenty. A round is
/// one TLS handshake and one frame each way per declared peer — three to seven
/// of them — so a second costs a cluster nothing it would notice. A node that
/// can name no leader greets faster than this until it can (see the greeting
/// round), because that is exactly when a stale reading costs the most.
pub const AWARENESS_SECONDS: u64 = 1;

/// How often the store line's leader looks at the shards of tables that split
/// and merge themselves (ADR-0113 D2). A pass walks each such shard up to its
/// bound, so it runs less often than the rounds that only ask a question.
pub const BALANCE_SECONDS: u64 = 5;

/// How long a range's leader waits after handing the range to its preferred
/// candidate before it may do so again (G053 SG5b). A preferred node that keeps
/// failing to hold the range would otherwise have it handed back and forth,
/// each hand-over a lease without a writer.
pub const PREFERENCE_YIELD_SECONDS: u64 = 30;

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
/// Since the leader pushes on a held stream (ADR-0106 D5) this round is the
/// fallback: it joins, re-seeds and meets refusals, and skips every line a
/// stream is carrying. Its period is how soon a follower whose stream ended
/// asks again, so it follows the awareness interval down to a second.
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

/// How long a leader holding a follower's stream stays silent before it sends
/// an empty round anyway (ADR-0106 D5).
///
/// Unit: milliseconds.
///
/// A commit is sent the moment it lands, so this never delays a record. It is
/// the idle heartbeat — what lets a follower on a quiet leader tell *level* from
/// *the link is gone* well inside [`GREETING_SECONDS`], the read deadline it
/// holds the stream under, and what keeps its `quiet_for` and `copy_age`
/// readings current on the leader. A hundred milliseconds, a Raft deployment's
/// heartbeat: it is also the leader's liveness signal a follower can stand on
/// (G053 C2b), and a second is far too long for that.
pub const STREAM_HEARTBEAT_MILLIS: u64 = 100;

/// The most logs one stream ask may name (ADR-0106 D5).
///
/// Unit: logs. A leader answers every log named on every commit it lands, so the
/// count is work a peer can ask for once and have repeated; the frame ceiling
/// alone would admit hundreds of thousands. A follower names one log per
/// namespace, database and shard it holds, so sixteen thousand is far beyond any
/// store this engine has been measured on and still a bound on what one peer can
/// make a leader do.
pub const STREAM_LOGS_MAX: u64 = 16_384;

/// How long a leader keeps the positions it copied a follower to, after the copy
/// ended, against its own retention window (ADR-0094 D3).
///
/// Unit: seconds. Two minutes: the follower's first collect after a copy is due
/// within one collection interval, and a follower that has not asked by then is
/// not coming soon enough to be worth a disk that keeps growing for it. It was
/// twelve collection intervals while an interval was ten seconds; it is stated
/// on its own now, because what it has to cover is a follower installing a large
/// copy, and that did not get faster when the collection round did.
pub const REPLICA_COPY_GRACE_SECONDS: u64 = 120;

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

/// The most bytes of records one collection answer carries.
///
/// Unit: bytes.
///
/// # Why bytes and not only records
///
/// [`COLLECTION_RECORDS`] is what a follower *asks* for, and nothing caps what
/// it may name. A peer asking for the whole log would otherwise make the leader
/// read every record and build one frame out of them, with the frame writer's
/// own ceiling refusing only after the reading had already happened — so the
/// bound that protects the leader has to be checked while the answer is being
/// filled, not when it is sent.
///
/// A record ceiling cannot be that bound. Records differ in size by orders of
/// magnitude, so any count either throttles a follower carrying small commits or
/// fails to protect against one carrying large ones. Both limits apply and
/// whichever is reached first stops the answer.
///
/// Well under the frame ceiling, which this is not a second copy of: the frame
/// ceiling refuses a frame, and this fills one.
pub const COLLECTION_BUDGET_BYTES: usize = 4 * 1024 * 1024;

/// The most records the leader reads from its log in one pass while filling a
/// collection answer.
///
/// Unit: records.
///
/// The answer is bounded by [`COLLECTION_BUDGET_BYTES`], and a budget can only
/// be honoured by a read that stops — so the log is read a page at a time and
/// the pages stop when the budget is full. It bounds the leader's memory to one
/// page plus the answer, whatever a follower names as its own limit.
pub const COLLECTION_PAGE_RECORDS: usize = 256;

/// How long a canvass of the voting members takes on this network, end to end.
///
/// Unit: milliseconds.
///
/// # It is a deadline, not a measurement
///
/// A candidate stands when its remaining writable window has shrunk to two of
/// these, because a round that yields a lease dated from when it **opened** has
/// to be in hand before the old fence shuts — and one round time lands exactly
/// on the fence with nothing left for a round that is refused, lost or slow. Two
/// is the latest opening that still allows one complete retry.
///
/// Two hundred milliseconds is a canvass of three to seven members on a local
/// network, asked at once, each a TLS handshake and one frame each way — tens of
/// milliseconds measured, so the deadline carries several times what it needs.
/// It is the value this build ships and not a property of the engine: the day a
/// cluster spans a region, this is the number that moves, and everything
/// derived from it moves with it. It was a second until 0.21, which put the
/// failover a leader's lease bounds at ten (G053 C2b).
pub const ROUND_MILLIS: u64 = 200;

/// How often a leader checks whether it is time to stand again, and a follower
/// whether it still hears one.
///
/// Unit: milliseconds.
///
/// # Where the number comes from
///
/// It is bounded by the window between *time to stand* and *the fence shuts*,
/// and that window is exactly `2 × ROUND_MILLIS`: a holder's usable span begins
/// at `LEASE_TTL - LEASE_GUARD` and standing opens when two round times are left
/// of it. A cadence slower than that window can step straight over the moment it
/// was supposed to act on, and a leader would then lose a lease it could have
/// renewed — while nothing anywhere reported a failure, because no round was
/// ever attempted.
///
/// So the period is a quarter of the window, which leaves room for a tick to be
/// late twice. It is also how quickly a follower notices that the leader it was
/// hearing has gone quiet, so it is the step an election timeout is measured
/// in. A check costs nothing when there is margin left: the decision to stand is
/// taken **before** any socket is opened, precisely so that a frequent cadence is
/// not a frequent canvass.
pub const CAMPAIGN_MILLIS: u64 = ROUND_MILLIS / 2;

/// The most a follower adds to the lease before it stands against a leader it
/// no longer hears.
///
/// Unit: milliseconds.
///
/// Every voter's grant memory is refreshed by the same renewal ballot, so when a
/// leader dies those memories lapse within milliseconds of one another, and two
/// followers standing on the same tick grant each other the one epoch and both
/// lose it for a whole lease. Raft's answer is a randomised election timeout; this
/// is its spread. Wide enough that a round trip and a TLS handshake (a few
/// milliseconds on a local network) fit between two nodes many times over, narrow
/// enough that the failover it adds to stays under a second (G053 C2b).
pub const ELECTION_JITTER_MILLIS: u64 = 150;

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

/// The most records a gathered read may hold (G033, ADR-0083).
///
/// Unit: records — this node's own part and every gathered part together.
///
/// A read of a split table on a node lacking some shards fetches those shards'
/// records and evaluates the statement over all of them, so the whole answer is
/// in memory before the first record is handed on. A pushed `WHERE` and a
/// `LIMIT` reduce what arrives; a grouping read whose folds merge exactly sends
/// groups instead of records, and this ceiling then counts groups (ADR-0097).
/// Past it the read is REFUSED rather than shortened, because a gathered answer
/// missing the records past a ceiling is exactly the partial answer sharding
/// refuses everywhere else.
///
/// Ten times the ceiling on a held read written by hand, because a gathered read
/// is a table read and not a subquery: a table somebody split is a large one.
pub const GATHER_RECORDS: usize = 100_000;

/// The most bytes of records one gather answer carries.
///
/// Unit: bytes.
///
/// [`COLLECTION_BUDGET_BYTES`]'s reasoning applied to a gather page: the bound
/// that protects the answering leader has to be checked while the answer is
/// filled, and it stays well under the frame ceiling it is not a copy of.
pub const GATHER_PAGE_BYTES: usize = COLLECTION_BUDGET_BYTES;

/// The most records the answering leader reads from a shard in one pass while
/// filling a gather page.
///
/// Unit: records.
pub const GATHER_PAGE_RECORDS: usize = 1024;

/// The most records the answering leader folds into one page of groups.
///
/// Unit: records.
///
/// Larger than [`GATHER_PAGE_RECORDS`] because what such a page sends is one
/// state per group rather than the records, so its size follows the groups; it
/// is bounded at all so that one page is read and folded well inside the
/// peer link's read deadline (`GREETING_SECONDS`).
pub const GATHER_FOLD_RECORDS: usize = 65_536;
