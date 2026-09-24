//! The three cadences a node runs, and why each gets a thread of its own.
//!
//! # Three cadences, three threads
//!
//! A node that has joined a cluster has three things to do on a timer: greet its
//! peers so it knows how current each one is, collect records from whoever it
//! follows, and renew the lease its leadership rests on. They look alike enough
//! to fold into one loop, and folding them is the mistake.
//!
//! They fail differently. A missed greeting costs the *freshness of a reading* —
//! routing gets more conservative, which is the direction it should fail in. A
//! missed collection costs *data*, and the node simply falls further behind. A
//! missed renewal costs *leadership*, and the fence closes whether or not anyone
//! noticed.
//!
//! One loop gives all three a single period, a single failure path, and a single
//! thread's fate. The sharpest consequence is the last: `collect` and `renew`
//! both dial peers, so a collection blocked on a dead peer's TCP connect would
//! hold up a renewal whose fence is closing. The cadence with the tightest
//! deadline would be delayed by the one with the loosest, for no reason beyond
//! their sharing a thread.
//!
//! # Nothing here hands a `Result` to a timer
//!
//! Each cadence answers *what it did* — a cursor, a lease — rather than whether
//! it went well. The caller is a loop that runs again either way, so an error
//! return would only ever be dropped, and a dropped error reads at the call site
//! as if failure were impossible. What each driver owns instead is the rule for
//! **what a failed pass does to the state it holds**, which is the part that is
//! genuinely easy to get wrong.
//!
//! # The pass is a parameter
//!
//! Every driver takes the work as a closure, the way [`crate::Directory`] takes
//! its clock and its greeting. A driver that dialled a socket itself could only
//! be tested by standing up peers, and one that read the clock itself could only
//! have its timing rule tested by waiting.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tessari_encoding::{NODE_ID_LEN, Roles};
use tessari_serve::Stopping;
use tessari_storage::{FailoverStamp, Lease, ReplicaDefinition};
use tessari_types::{Epoch, Reach, Sequence};

use crate::campaign::Stood;
use crate::directory::{Destination, Directory};
use crate::grant::Leadership;
use crate::joining::Seed;

/// How long to wait before the next pass, given when the last one started.
///
/// # A missed tick is not made up
///
/// When a pass overran its period — the peer was slow, the thread was
/// descheduled — this answers [`Duration::ZERO`] and the next pass runs at once.
/// It never answers *run it three times because three periods went by*.
///
/// Catching up would be wrong for each cadence separately. Three greeting rounds
/// back to back dial every peer three times to learn one thing. Three
/// collections are unnecessary because the cursor already carries the position,
/// so one pass fetches as much as the peer's limit allows regardless of how long
/// it has been. And three renewals are three elections where the cluster needed
/// none.
#[must_use]
pub fn due_in(period: Duration, ran_at: Instant, now: Instant) -> Duration {
    period.saturating_sub(now.saturating_duration_since(ran_at))
}

/// Run `pass` on `period` until the node is asked to stop.
///
/// The flag is checked **before** each pass, so a node already stopping runs
/// none. Stopping during the wait takes effect at the end of it: a cadence is
/// not on the shutdown path, and waking it early would buy a fraction of a
/// period at the cost of a second way to interrupt a thread.
///
/// The stop flag is the node's own [`Stopping`] rather than one of this
/// module's. A driver with a private flag gives a process two ways to ask a node
/// to stop, and the state between them — a node that has stopped serving while
/// it goes on dialling peers — is worse than either.
pub fn every(period: Duration, stopping: &Stopping, mut pass: impl FnMut(Instant)) {
    while !stopping.asked() {
        let ran_at = Instant::now();
        pass(ran_at);
        std::thread::sleep(due_in(period, ran_at, Instant::now()));
    }
}

/// The directory the routing side reads, and the greeting side replaces.
///
/// # Why a copy and a swap rather than a lock held across the round
///
/// [`Directory::greet_round`] takes `&mut self` and dials each peer *inside* the
/// walk, so a shared `Mutex<Directory>` would hold the lock for the length of
/// every connection attempt. Every routing read would then wait on the slowest
/// unreachable peer in the cluster — which is exactly the node the directory
/// exists to route around, so the structure would turn one node's failure into
/// every reader's latency.
///
/// Instead the greeting side takes a copy, dials into the copy with no lock
/// held, and swaps the result in under a lock held for the swap alone. Readers
/// see the previous round's answers until the new ones are all in, which is a
/// consistent view rather than a partial one.
///
/// The copy is taken from the *current* directory and not from an empty one, so
/// a peer that was heard two rounds ago and has been silent since is carried
/// forward and goes on ageing. That is what makes W234's rule survive the swap:
/// a silent peer grows old, and starting each round from nothing would instead
/// make every silent peer vanish once per period.
#[derive(Debug)]
pub struct Published {
    current: Mutex<Arc<Directory>>,
}

impl Published {
    /// Publish `directory` as the current answer.
    #[must_use]
    pub fn holding(directory: Directory) -> Self {
        Self {
            current: Mutex::new(Arc::new(directory)),
        }
    }

    /// The directory as it stands.
    ///
    /// The lock is held only long enough to clone a pointer, so a reader never
    /// waits on a greeting round.
    #[must_use]
    pub fn current(&self) -> Arc<Directory> {
        Arc::clone(&self.held())
    }

    /// Run one greeting round against a copy, then publish it.
    pub fn round(&self, greet: impl FnOnce(&mut Directory)) {
        let mut next = (*self.current()).clone();
        greet(&mut next);
        *self.held() = Arc::new(next);
    }

    /// The guard, recovering rather than panicking if a holder died mid-swap.
    ///
    /// What the lock protects is one pointer. A thread that panicked while
    /// holding it left a whole directory behind, never half of one, so the value
    /// is sound and refusing to read it would take routing down over an
    /// unrelated failure elsewhere.
    fn held(&self) -> std::sync::MutexGuard<'_, Arc<Directory>> {
        self.current.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The routing question a bounded read asks, answered from the last round.
///
/// This is the join the whole directory was built for: the dialling thread
/// writes a round every awareness interval, and until now nothing read one. The
/// implementation adds no rule of its own — [`Directory::read_within`] already
/// decides *here first, then freshest qualifying peer*, and repeating any part
/// of that here would be a second place for the routing rule to live.
///
/// # Two things this method does that the directory cannot
///
/// It reads the **clock**. Every method on [`Directory`] takes `now` so its
/// ageing rule can be tested without waiting, which means somebody has to be the
/// edge where real time enters, and a production caller is the only honest
/// candidate for it.
///
/// It passes `mine: None`. The session asks only once its own copy has already
/// failed the bound, so *here* is decided; handing the directory this node's
/// currency as well would invite it to answer `Here` to a question that was only
/// asked because the answer was no.
impl tessari_session::Elsewhere for Published {
    fn within(&self, bound: Duration) -> Option<tessari_session::Peer> {
        let directory = self.current();
        match directory.read_within(None, bound, Instant::now()) {
            // The epoch comes back out of the same reading that chose the
            // endpoint, rather than from a second decision: `Heard.said` is
            // exactly what that peer last claimed about itself, and its `epoch`
            // field is documented as the leadership it believes current. Looking
            // it up here and not widening `Destination` keeps the three-valued
            // routing answer about *where*, which is all it has ever decided.
            //
            // A row chosen by `read_within` is a row that is in the map, so the
            // lookup cannot miss — but it is written as a lookup and not as an
            // unwrap because a panic in a routing decision would take a serving
            // node down over a redirect it could simply decline to issue.
            Destination::There { endpoint, node } => {
                let epoch = directory.at(&endpoint)?.said.epoch;
                Some(tessari_session::Peer {
                    endpoint,
                    node,
                    epoch,
                })
            }
            // `Here` cannot arise with no currency of our own offered, and
            // `Nowhere` is the answer the caller already holds. Both mean *not
            // that I know of*, which is what `None` says.
            Destination::Here | Destination::Nowhere => None,
        }
    }

    /// The other routing question, answered from the same last round.
    ///
    /// No clock and no bound, because leadership does not age into being
    /// slightly wrong the way a currency reading does — see
    /// [`Directory::writable`]. The epoch is looked up out of the same reading
    /// that chose the endpoint, exactly as it is above, and for the same reason:
    /// it is what that peer claimed about itself, and it is what makes the
    /// redirect checkable when the client arrives.
    fn writable(&self) -> Option<tessari_session::Peer> {
        let directory = self.current();
        let (endpoint, node) = directory.writable()?;
        let epoch = directory.at(&endpoint)?.said.epoch;
        Some(tessari_session::Peer {
            endpoint,
            node,
            epoch,
        })
    }
}

/// How far this node has collected in each log, and what a failure does to it.
///
/// # One cursor per log, and not one cursor
///
/// A position counts in ONE log. A node holds one log per home, so a single
/// cursor carried across logs would advance in one space and be spent in
/// another — asking a namespace's log for a position the store's log had
/// reached, which is a gap or a re-send depending only on which log ran ahead.
#[derive(Debug, Default)]
pub struct Collecting {
    at: BTreeMap<Reach, Sequence>,
}

impl Collecting {
    /// A node that has collected nothing yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            at: BTreeMap::new(),
        }
    }

    /// The position this node has reached in `home`, or `None` when it has not
    /// collected there at all.
    ///
    /// `None` rather than a zero, because *I have never collected this log* and
    /// *I collected it and reached the beginning* are different statements and
    /// the caller seeding a cursor has to tell them apart.
    #[must_use]
    pub fn reached(&self, home: Reach) -> Option<Sequence> {
        self.at.get(&home).copied()
    }

    /// One collection of one log. The cursor moves **only** when the pass
    /// answers.
    ///
    /// `seed` is where to start when this log has no cursor yet — ordinarily
    /// this node's own committed tail there plus one. It is read by the caller
    /// and not here, because a log this node has never collected is discovered
    /// by reading the catalog, and the rule `Collector::collect` documents keeps
    /// the feed out of this crate's reach.
    ///
    /// After a pass the cursor holds what the pass REACHED, which is the last
    /// position applied rather than the first one not held — so the next ask
    /// re-fetches one record. That is unchanged from when there was one cursor
    /// and is recorded rather than corrected here (Q-631).
    ///
    /// # Why a failure leaves the cursor alone
    ///
    /// A cursor advanced past records that were never applied skips them
    /// permanently and silently: the next pass asks for what comes after, no
    /// later pass ever asks for the gap, and nothing is in an error state to say
    /// so. Leaving it costs a repeated request when the peer comes back, which
    /// is the failure worth having.
    ///
    /// # Why the refusal is handed back rather than absorbed
    ///
    /// Until W382 this answered a `Sequence` either way, and the error was
    /// dropped at the one point in the program where it was in scope. The
    /// caller was then left with a cursor that had not moved — which is also
    /// what a healthy pass with nothing to fetch produces — so *the peer
    /// refused me* and *the peer had nothing for me* reached an operator as the
    /// same line, and a cluster replicating nothing looked exactly like a
    /// cluster that was already level.
    ///
    /// That is not a reporting nicety. It cost three processes, ninety seconds
    /// and a refuted hypothesis to learn that a follower was being turned away,
    /// because the sentence naming the reason existed only inside this function
    /// and was discarded here.
    ///
    /// # Errors
    ///
    /// Returns whatever `pass` returned. The cursor is already recorded when it
    /// does, so a caller that only wants to go on collecting may discard it —
    /// but it has to discard it deliberately.
    pub fn once<E>(
        &mut self,
        home: Reach,
        seed: Sequence,
        pass: impl FnOnce(Sequence) -> Result<Sequence, E>,
    ) -> Result<Sequence, E> {
        let at = self.at.get(&home).copied().unwrap_or(seed);
        match pass(at) {
            Ok(reached) => {
                self.at.insert(home, reached);
                Ok(reached)
            }
            Err(why) => {
                // The seed, when this log had no cursor at all: a failed first
                // pass still fixes where the next one starts, or the caller's
                // freshly-read tail would be handed to a retry as a new
                // beginning it has not earned.
                self.at.insert(home, at);
                Err(why)
            }
        }
    }
}

/// The peer this node collects from, if it should collect at all.
///
/// # A node that may write follows nobody
///
/// A node holding [`Roles::WRITABLE`] is the origin of what it holds — that is
/// exactly what `Store::current_as_of` says when it answers `Some(0)` — so it
/// has nothing to catch up to. Collecting into it would apply a peer's records
/// beside its own, which is the divergence the log's epoch chain exists to
/// refuse, discovered at apply time rather than prevented at the timer.
///
/// The roles are re-read every round rather than decided at start, so a node
/// that is told to stop writing begins following without being restarted.
///
/// # The catalog says who may be followed; the greeting says who to follow
///
/// ADR-0065. Until W255 this took the one peer the catalog declared `WRITABLE`,
/// which was correct while exactly one node could ever write. ADR-0063 and
/// ADR-0064 together make *every coordinating node also declared writable* the
/// configuration a cluster needs in order to fail over at all — and a rule that
/// reads the declaration then finds two writable rows and gives up, which is
/// what it did: `Error::ManyWritablePeers` once per collection interval, on
/// every node, forever, with nothing replicating and nothing in an error state
/// except a log line.
///
/// The leadership is a lease, so the answer moves at runtime and is not in a
/// row. It is already on the wire: [`crate::Hello::current_as_of`] is the
/// greeter's own `Store::current_as_of`, which answers `Some(0)` **exactly
/// when** its *effective* roles carry `WRITABLE` — a node that may write is the
/// origin of what it holds and has nothing to be stale relative to. So a
/// greeting says *I may write right now* as an effective fact rather than a
/// declared one, and [`crate::Hello::epoch`] says under which leadership.
///
/// The epoch breaks the tie, and the tie is not hypothetical: a leader demoted
/// a moment ago and its successor can both be in this directory, because a
/// greeting is as fresh as the last awareness round and no fresher. ADR-0059's
/// ordering picks the newer leadership, which is the same rule a voter applies
/// to a ballot.
///
/// Nothing is added to the wire and no new cadence is introduced — the
/// awareness round already fills this directory, and until now only the
/// staleness router read it.
///
/// # A peer declared without a node cannot be dialled
///
/// A peer connection demands a certificate valid for a name derived from the
/// peer's **id**, so an endpoint whose id nobody knows cannot be dialled at all
/// — the same wall the seed address runs into. `None` here rather than a
/// half-formed attempt: a row that says who but not where, or where but not
/// who, is a declaration the operator has not finished.
///
/// # A peer this node has never greeted is not followed
///
/// Absence of a greeting is not evidence that a peer may write, so a cold node
/// collects from nobody until its first awareness round has landed. One
/// interval of not collecting, against the alternative of pulling records from
/// whichever address happened to be declared first.
#[must_use]
pub fn upstream(
    mine: Roles,
    declared: &[ReplicaDefinition],
    heard: &Directory,
) -> Option<([u8; NODE_ID_LEN], String)> {
    if mine.has(Roles::WRITABLE) {
        return None;
    }
    declared
        .iter()
        .filter_map(|peer| Some((peer.node?, peer, heard.at(&peer.endpoint)?)))
        .filter(|(_, _, heard)| heard.said.current_as_of == Some(Duration::ZERO))
        .max_by_key(|(_, _, heard)| heard.said.epoch)
        .map(|(node, peer, _)| (node, peer.endpoint.clone()))
}

/// Where a node whose catalog names nobody collects from.
///
/// The bootstrap twin of [`upstream`], and every rule it applies is that
/// function's: a node that may write has no upstream, a greeting is required
/// before anything is followed, `current_as_of == Some(0)` is what *may write
/// right now* looks like on the wire, and the epoch breaks a tie between a
/// demoted leader and its successor. Only the candidate set differs — seeds
/// instead of declared peers — which is why the two are siblings rather than
/// one function with a flag: the set is the whole difference, and a flag would
/// invite a caller to pass both.
///
/// # Only while the catalog is empty
///
/// The caller uses this **instead of** [`upstream`] while the catalog declares
/// no peer, and never as a fallback when `upstream` happens to answer `None`.
/// The distinction matters and it is not stylistic. `upstream` answers `None`
/// for ordinary, temporary reasons — no greeting has landed yet, every declared
/// peer is currently a follower, this node may write — and a seed consulted on
/// any of those would be a node that stops believing its own membership the
/// moment the leader is briefly unreachable, and goes back to an address written
/// on a command line months ago. Empty is the only condition that means *this
/// node has not joined yet*.
///
/// # The seed is spent as soon as it works
///
/// `DEFINE REPLICA` is a catalog write and therefore already a log record, so
/// the membership arrives through the very collection this function starts.
/// After the first successful round the catalog names peers, the caller stops
/// asking this question, and nothing reads the seed again for the life of the
/// node. That is `04_concept.md` §6.3 holding exactly as written — *the seed
/// address is configuration, and everything after first contact lives in the
/// database* — and it needs no frame of its own to be true.
#[must_use]
pub fn bootstrap_from(
    mine: Roles,
    seeds: &[Seed],
    heard: &Directory,
) -> Option<([u8; NODE_ID_LEN], String)> {
    if mine.has(Roles::WRITABLE) {
        return None;
    }
    seeds
        .iter()
        .filter_map(|seed| Some((seed, heard.at(&seed.endpoint)?)))
        .filter(|(_, heard)| heard.said.current_as_of == Some(Duration::ZERO))
        .max_by_key(|(_, heard)| heard.said.epoch)
        .map(|(seed, _)| (seed.node, seed.endpoint.clone()))
}

/// Does the catalog name a peer that is not this node?
///
/// Re-exported from `tessari_storage` and not defined here: the write gate asks
/// the same question of the same rows, and membership defined twice is
/// membership that agrees until it does not. The reasoning — why this is not
/// *is the catalog empty*, and why the gate asks it rather than asking what
/// role the node was given — is on the definition.
pub use tessari_storage::names_a_peer;

/// Whether a leader this node can hear is still leading.
///
/// # A follower that hears a leader does not become a candidate
///
/// ADR-0066, and it is the part with no analogue anywhere else in this engine.
/// [`stands`] asks what the operator declared and the renewal cadence asks about
/// this node's own lease, so until W257 nothing in the campaign path ever looked
/// at whether somebody else was already leading — and a `COORDINATING` node
/// holding no lease has no margin, so it stood on every single tick.
///
/// That is not merely noisy, it is a denial of service against the leader.
/// `Voter::asked` refuses a non-incumbent with `EarlierGrantStillAlive` until a
/// grant it made is certainly dead, one whole `LEASE_TTL` later. A node that
/// stands every second grants an epoch to *itself* every second, so two such
/// followers can refuse the real leader's renewal indefinitely: the cluster
/// elects somebody, loses them a TTL later, and never gets them back. Raft's
/// answer is not a rule about voting at all — a follower that hears from a
/// current leader resets its election timer and does not stand.
///
/// # The signal is the one already on the wire
///
/// [`crate::Hello::current_as_of`] answers `Some(0)` exactly when the greeter's
/// **effective** roles carry `WRITABLE` — *I may write right now* — which is the
/// same fact [`upstream`] routes a follower by (ADR-0065). No new frame, no new
/// cadence, and one more reader of the awareness round.
///
/// # How fresh the hearing has to be
///
/// `within` is the caller's, and the value that makes sense is the lease term: a
/// greeting older than the leader's own lease cannot testify that the leader
/// still holds it. The awareness interval and the lease are the same length by
/// design, so in practice this tolerates one missed round and no more — and a
/// node that hears nothing stands, which is the condition an election is for.
///
/// # Declared peers only, and this node is never one of them
///
/// The same set [`upstream`] chooses from, for the same reason: a greeting from
/// an address this node's catalog does not declare is not a member speaking.
/// The awareness round skips this node's own row, so nothing it said about
/// itself is in the directory to be mistaken for a peer.
/// # The grant is the faster of the two, and it was already being recorded
///
/// `granted` is when this node last granted a ballot **to somebody else** — see
/// [`crate::Voter::granted_elsewhere_at`], whose doc block carries why the
/// *else* is load-bearing rather than tidy. A leader renews against every voter while two
/// round times are left of its usable window, so a voter hears from a live
/// leader about every **six** seconds at today's values, where the directory is
/// refreshed every **ten** and the reading is itself up to that old again. Taking
/// the fresher of the two is what makes detection faster than the awareness
/// round without a new frame, a new cadence, or a connection held open.
///
/// Neither source replaces the other. A node that grants nothing — a follower
/// outside the deciding set — has no grant to read and is answered by the
/// directory exactly as before; a voter in a cluster whose greetings are late
/// still has its own grant. `None` is therefore *no such evidence*, never *no
/// leader*.
#[must_use]
pub fn heard_a_leader(
    declared: &[ReplicaDefinition],
    heard: &Directory,
    granted: Option<Instant>,
    now: Instant,
    within: Duration,
) -> bool {
    let by_grant = granted.is_some_and(|at| now.saturating_duration_since(at) <= within);
    by_grant
        || declared
            .iter()
            .filter_map(|peer| heard.at(&peer.endpoint))
            .any(|seen| {
                seen.said.current_as_of == Some(Duration::ZERO)
                    && now.saturating_duration_since(seen.at) <= within
            })
}

/// The newest failover policy a live peer advertises, when it supersedes this
/// node's own.
///
/// # Why a node behind on the policy does not stand
///
/// The periods in the policy decide how long this cluster waits before it treats
/// a leader as gone. Two nodes that disagree about them are two nodes that can
/// both believe they may write — the split-brain the lease exists to prevent,
/// arriving through the mechanism meant to prevent it. That is the failover
/// row's own argument for existing, and it is the argument for this gate.
///
/// A candidate standing under periods the rest of the cluster has already
/// replaced is that disagreement, in the one moment where it decides an outcome.
/// So this is the sibling of [`heard_a_leader`]: same shape, same call site,
/// same class of reason — a node that can hear evidence it should not stand,
/// does not stand.
///
/// # Why it cannot deadlock a cluster
///
/// The refusal is bounded by **audibility**, not by state. It lasts only while a
/// declared peer is still advertising a superseding stamp inside `within`; a
/// peer that has gone away advertises nothing, and this answers `None`, and the
/// node stands. So the failure this gate could have introduced — a cluster
/// permanently unable to elect because the only node holding the newer policy
/// died — is the one case in which the gate is already open.
///
/// # `None` is behind, not ahead
///
/// A greeter's `None` means *no policy row, running the default* or *a build
/// from before the field*, and neither can supersede anything, so neither
/// silences this node. This node's own `None` is the other side of the same
/// coin and **is** superseded by any stamp: a node that has never been told is
/// behind one that has, and the alternative would make the first policy a
/// cluster ever sets the one policy nothing could act on.
///
/// # Declared peers only
///
/// The same set [`heard_a_leader`] and [`upstream`] read, for the same reason: a
/// greeting from an address this node's catalog does not declare is not a member
/// speaking, and a stranger that could silence a candidate is a denial of
/// service with a one-line implementation.
///
/// The newest is returned rather than a bare `true` so that a caller can say
/// **which** policy it is behind — a gate that refuses without naming what it
/// refused on is one an operator can only investigate with a packet capture.
#[must_use]
pub fn heard_a_newer_policy(
    declared: &[ReplicaDefinition],
    heard: &Directory,
    mine: Option<FailoverStamp>,
    now: Instant,
    within: Duration,
) -> Option<FailoverStamp> {
    declared
        .iter()
        .filter_map(|peer| heard.at(&peer.endpoint))
        .filter(|seen| now.saturating_duration_since(seen.at) <= within)
        .filter_map(|seen| seen.said.policy)
        .filter(|stamp| stamp.supersedes_held(mine.as_ref()))
        .max_by_key(|stamp| (stamp.epoch, stamp.version))
}

/// Whether this node is eligible to stand at all, from its own identity alone.
///
/// The half of [`voters`]'s question that needs no catalog. A node the operator
/// never declared [`Roles::COORDINATING`] stands for nothing whoever its peers
/// turn out to be, so a caller running on a cadence can settle that before
/// opening a transaction to read every replica the catalog declares — a read it
/// would otherwise pay once a tick, for the life of the process, to reach the
/// same `return`.
///
/// [`voters`] asks this rather than repeating the test, so the rule is stated
/// once and an early-out at a call site cannot drift away from the answer the
/// round itself will give.
///
/// # Why the role asked for is `COORDINATING` and not `WRITABLE`
///
/// ADR-0063. The role is documented in `tessari-encoding` as *takes part in
/// deciding, rather than only in storing*, and the set a leader is drawn from is
/// the deciding set. Asking for `WRITABLE` instead left exactly one node able to
/// stand, which makes the demotion in `04_concept.md` §6.1.2 — a leader that
/// gives up its lease before the TTL expires — buy a cluster with no writer at
/// all rather than a cluster with a new one. An automatic failover with one
/// candidate is a contradiction, not a hard problem.
///
/// `WRITABLE` did not stop mattering; it moved. It is what a voter weighs and
/// what the catalog says the operator *wants*, rather than the only thing a node
/// *can* be.
///
/// # A single-node store is untouched, and that is the risk worth naming
///
/// [`Roles::ALONE`] is `SERVING|WRITABLE` and is deliberately **not**
/// `COORDINATING` — *because there is nothing to coordinate with*. So this
/// change makes every existing single-node deployment stand for less than it did
/// before, never more, and no store that has never needed a lease begins taking
/// one.
#[must_use]
pub fn stands(mine: Roles) -> bool {
    mine.has(Roles::COORDINATING)
}

/// ADR-0066's rule on one placed range's line (ADR-0082): whether this node can
/// still hear a leader of `range`, and so must not stand for it.
///
/// The same two sources [`heard_a_leader`] reads, each narrowed to the line: a
/// ballot this node granted somebody else on THAT line, and a peer whose
/// greeting says it holds a live lease on it. A leader of a placed range is a
/// node placed on it, so its greeting's line is that range — the one fact the
/// greeting carries about any line.
#[must_use]
pub fn heard_a_leader_on(
    range: Reach,
    me: [u8; NODE_ID_LEN],
    declared: &[ReplicaDefinition],
    heard: &Directory,
    granted: Option<Instant>,
    now: Instant,
    within: Duration,
) -> bool {
    let by_grant = granted.is_some_and(|at| now.saturating_duration_since(at) <= within);
    by_grant
        || declared
            .iter()
            .filter_map(|peer| heard.at(&peer.endpoint))
            .any(|seen| {
                seen.said.node != me
                    && seen
                        .said
                        .line
                        .is_some_and(|line| line.range == range && line.leading > Epoch::ZERO)
                    && now.saturating_duration_since(seen.at) <= within
            })
}

/// Who to collect a placed range's logs from: the peer whose greeting says it
/// holds a live lease on that range's line (ADR-0082).
///
/// [`upstream`] answers for the store line, and it answers `None` on a node that
/// may write — the store leader is the origin of the store line and follows
/// nobody. A placed range has an origin of its own, so every node but its leader
/// collects it from its leader, the store leader included; without this a
/// range leader's writes, its leadership row among them, reach nobody.
///
/// The greeting and not the leadership row, for [`upstream`]'s reason: the row
/// arrives through the very collection this chooses the source of.
#[must_use]
pub fn leader_of_range(
    range: Reach,
    declared: &[ReplicaDefinition],
    heard: &Directory,
) -> Option<([u8; NODE_ID_LEN], String)> {
    declared
        .iter()
        .filter_map(|peer| Some((peer.node?, peer, heard.at(&peer.endpoint)?)))
        .filter(|(node, _, seen)| {
            seen.said.node == *node
                && seen
                    .said
                    .line
                    .is_some_and(|line| line.range == range && line.leading > Epoch::ZERO)
        })
        .max_by_key(|(_, _, seen)| seen.said.line.map(|line| line.leading))
        .map(|(node, peer, _)| (node, peer.endpoint.clone()))
}

/// The range this node is placed to lead, if a member row bound to it says one
/// (ADR-0082).
///
/// The first row bound to `me`, in the catalog's name order — the rule
/// `Catalog::desired_roles` applies to the same rows, so the row that decides a
/// node's roles and the row that decides its line are one row.
#[must_use]
pub fn stands_for(declared: &[ReplicaDefinition], me: &[u8; NODE_ID_LEN]) -> Option<Reach> {
    declared
        .iter()
        .find(|peer| peer.node.as_ref() == Some(me))
        .and_then(|peer| peer.leads)
}

/// The voting members this node puts a ballot to, if it stands at all.
///
/// # Who stands
///
/// A node the operator declared [`Roles::COORDINATING`], and nobody else —
/// [`stands`] states the rule and this asks it. `04_concept.md` §6.2 fixes the
/// membership shape: three to seven voting members, and everything above that a
/// non-voting follower. A node outside that set promoting itself because a
/// leader went quiet would be taking a decision the catalog is there to hold.
///
/// The roles asked for here are therefore the **declared** ones, never
/// [`tessari_storage::Store::effective_roles`]. The effective set drops
/// `WRITABLE` the instant the lease is spent, so a leader that missed one round
/// would stop campaigning and could never stand again — and the failure would be
/// indistinguishable from a correct demotion, which is what makes it worth
/// naming here rather than leaving to the call site.
///
/// # A node with nobody to coordinate with stands for nothing
///
/// `None` rather than a round against an empty set. [`Roles::ALONE`] says it
/// outright — *not `COORDINATING`, because there is nothing to coordinate with*
/// — and the practical half matters more: every single-node deployment in
/// existence would otherwise begin taking and defending a lease, which gives a
/// store that has never needed one a brand-new way to stop accepting writes the
/// day a thread dies.
///
/// # A peer nobody has identified is not a voter
///
/// The same wall [`upstream`] runs into. A ballot travels on a connection whose
/// certificate must be valid for a name derived from the peer's id, so a row
/// naming where but not who cannot be asked for anything at all.
///
/// # This node is not one of its own voters
///
/// `me` is skipped, exactly as [`Directory::greet_round`] skips it and for a
/// reason that is not symmetry. A node's catalog acquires a row describing
/// **itself** as soon as replication works at all: `DEFINE REPLICA` is a
/// catalog write and therefore a log record, so a follower applying a leader's
/// store log receives the leader's view of the membership — which names every
/// node including the one reading it.
///
/// Without this filter the consequence is not a wasted dial. The membership a
/// round is judged against is `voters.len() + 1` (see [`crate::Standing`]), so
/// a self row makes a cluster of three count itself as four and demand three
/// grants; and the third can never arrive, because the only node that would
/// cast it is the candidate, whose own door refuses the connection with
/// [`crate::Error::ClaimsOurOwnIdentity`]. Every round then fails, every
/// failure raises the epoch, and the cluster campaigns for ever without
/// anything being in an error state — which is precisely what W382 measured
/// against three processes: the store log replicated once, and the election
/// storm that started in the same second stopped anything else from following
/// it.
#[must_use]
pub fn voters(
    mine: Roles,
    declared: &[ReplicaDefinition],
    me: &[u8; NODE_ID_LEN],
) -> Option<Vec<([u8; NODE_ID_LEN], String)>> {
    if !stands(mine) {
        return None;
    }
    let voting: Vec<_> = declared
        .iter()
        .filter(|peer| peer.roles.has(Roles::COORDINATING))
        .filter_map(|peer| Some((peer.node?, peer.endpoint.clone())))
        .filter(|(node, _)| node != me)
        .collect();
    (!voting.is_empty()).then_some(voting)
}

/// The leadership this node holds, and what a lost round does to it.
///
/// # A lost round has to leave a mark, or it is not a round
///
/// ADR-0066. Until W257 this held one field — the leadership won — and derived
/// the next epoch from it. So a candidate that lost stood for the same epoch
/// again, against voters that had already spent it, for the life of the
/// process: three nodes started together elected nobody in ninety seconds, all
/// healthy, nothing in an error state. Losing is the ordinary case in an
/// election and it needs its own memory.
///
/// Three facts, because they answer three different questions. What this node
/// **holds** decides whether it may write. What it has **stood for** stops it
/// re-asking an epoch it has already spent. What it has **heard granted** stops
/// it climbing one epoch per round towards a cluster that is already far above
/// it — a node that never led holds [`Epoch::ZERO`] however long the cluster
/// has been running, and every refusal it gets is already carrying the number.
#[derive(Debug)]
pub struct Renewing {
    standing: Leadership,
    stood: Epoch,
    heard: Epoch,
    /// When this node may open its next round, after losing one.
    ///
    /// `None` means *now*. See [`Renewing::stagger`] for why a fixed wait is not
    /// enough and what this is derived from.
    not_before: Option<Instant>,
}

impl Renewing {
    /// Hold `standing` until something better is won.
    #[must_use]
    pub fn holding(standing: Leadership) -> Self {
        Self {
            stood: standing.epoch,
            standing,
            heard: Epoch::ZERO,
            not_before: None,
        }
    }

    /// The leadership this node is currently standing on.
    #[must_use]
    pub fn standing(&self) -> Leadership {
        self.standing
    }

    /// One renewal round, standing for the epoch after the one held.
    ///
    /// # Why a round that wins nothing changes nothing
    ///
    /// [`crate::Standing::renew`] answers `None` in two circumstances: there was
    /// margin left and nobody was asked, or a round ran and no majority granted
    /// it. Both mean *carry on with the lease you have*, and the first is the
    /// ordinary case — a cadence that ran a little early. Standing down on
    /// `None` would make a healthy leader resign because its timer fired before
    /// its fence needed defending.
    pub fn once(
        &mut self,
        candidate: [u8; NODE_ID_LEN],
        now: Instant,
        pass: impl FnOnce(Lease, Epoch) -> Stood,
    ) -> Leadership {
        if self.not_before.is_some_and(|until| now < until) {
            return self.standing;
        }
        let next = Epoch::new(
            self.standing
                .epoch
                .get()
                .max(self.stood.get())
                .max(self.heard.get())
                .saturating_add(1),
        );
        match pass(self.standing.lease(), next) {
            // Untouched, and that is the ordinary case: a cadence that ran a
            // little early while the fence was still far off. Standing down here
            // would make a healthy leader resign because its timer fired.
            Stood::NotDue => {}
            Stood::Won(won) => {
                self.standing = won;
                self.stood = won.epoch;
                self.not_before = None;
            }
            Stood::Lost { granted } => {
                self.stood = next;
                if granted > self.heard {
                    self.heard = granted;
                }
                // `None` on an instant so far out that the addition cannot be
                // represented, which reads as *stand now*. That is the safe
                // direction: an unrepresentable clock should not be able to
                // stop a node standing for an epoch.
                self.not_before = now.checked_add(Self::stagger(candidate, next));
            }
        }
        self.standing
    }

    /// How long this node waits before standing again, having just lost.
    ///
    /// # Why a wait at all
    ///
    /// Advancing the epoch alone does not break a tie. Three candidates that
    /// lose together stand for the next epoch on the same tick, each grants it
    /// to itself, each refuses the other two, and the deadlock repeats one epoch
    /// higher forever. Raft states the randomised election timeout as the
    /// liveness argument itself rather than as a tuning detail, and this is why.
    ///
    /// # Why it is derived rather than random
    ///
    /// The property needed is only that two candidates do not retry in step. A
    /// value derived from the candidate gives that, and gives one thing a random
    /// one cannot: a test may state the instant a node will stand and assert it,
    /// where randomness can only be observed not to deadlock over some number of
    /// runs. It also keeps this crate's dependency list as it is.
    ///
    /// **The epoch is in the mix and that is what makes it as strong as
    /// randomness.** Two nodes whose ids happen to fall close together collide
    /// at one epoch and not at the next; derived from the id alone, an unlucky
    /// pair would collide at every epoch forever — which is the failure this
    /// exists to remove, reintroduced one level down.
    fn stagger(candidate: [u8; NODE_ID_LEN], epoch: Epoch) -> Duration {
        // A 64-bit mix of the id and the epoch, spread over one round. The
        // constants are SplitMix64's; nothing here needs a distribution better
        // than "two different inputs land in different places", and a named
        // mixer is easier to recognise than an invented one.
        let mut mixed = epoch.get();
        for chunk in candidate.chunks(8) {
            let mut byte = 0_u64;
            for (place, value) in chunk.iter().enumerate() {
                byte |= u64::from(*value) << (place.saturating_mul(8));
            }
            mixed ^= byte;
            mixed = mixed.wrapping_add(0x9e37_79b9_7f4a_7c15);
            mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            mixed ^= mixed >> 31;
        }
        let round = Duration::from_secs(tessari_constants::ROUND_SECONDS);
        let spread = u64::try_from(round.as_millis()).unwrap_or(u64::MAX).max(1);
        Duration::from_millis(mixed.rem_euclid(spread))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use tessari_encoding::{NODE_ID_LEN, NodeVersion, Roles};
    use tessari_serve::Stopping;
    use tessari_storage::Lease;
    use tessari_types::{Epoch, NamespaceId, Reach, Sequence};

    use super::{
        Collecting, FailoverStamp, Published, Renewing, ReplicaDefinition, Seed, Stood,
        bootstrap_from, due_in, every, heard_a_leader, heard_a_leader_on, heard_a_newer_policy,
        leader_of_range, names_a_peer, stands, stands_for, upstream, voters,
    };
    use crate::directory::Directory;
    use crate::grant::Leadership;
    use crate::peer::Hello;

    const NODE: [u8; NODE_ID_LEN] = [7; NODE_ID_LEN];
    /// The log every single-log fixture here counts in.
    const STORE: Reach = Reach::Store;
    const ANOTHER: [u8; NODE_ID_LEN] = [9; NODE_ID_LEN];

    /// A serving peer one second behind.
    fn said() -> Hello {
        Hello {
            node: NODE,
            build: NodeVersion {
                major: 0,
                minor: 1,
                patch: 1,
            },
            epoch: Epoch::new(7),
            roles: Roles::SERVING,
            tail: Sequence::new(4096),
            tail_leadership: Epoch::new(7),
            current_as_of: Some(Duration::from_secs(1)),
            policy: None,
            line: None,
        }
    }

    /// A greeting from a node that may write right now.
    ///
    /// `current_as_of` answering `Some(0)` is exactly what a node says when its
    /// EFFECTIVE roles carry `writable`: it is the origin of what it holds, so
    /// there is nothing for it to be stale relative to.
    fn writing(epoch: Epoch) -> Hello {
        Hello {
            epoch,
            current_as_of: Some(Duration::ZERO),
            ..said()
        }
    }

    /// A greeting from a node that holds somebody else's writes.
    fn following() -> Hello {
        said()
    }

    /// A directory holding one greeting per endpoint, all heard just now.
    fn greeted(rows: &[(&str, Hello)]) -> Directory {
        let mut directory = Directory::new();
        let now = Instant::now();
        for (endpoint, said) in rows {
            directory.heard(endpoint, *said, now);
        }
        directory
    }

    /// A declared peer row naming a node at an address, writable and
    /// coordinating — the shape every member of a cluster that can fail over
    /// carries for every other member.
    fn named(name: &str, endpoint: &str, node: [u8; NODE_ID_LEN]) -> ReplicaDefinition {
        ReplicaDefinition {
            name: name.to_owned(),
            endpoint: endpoint.to_owned(),
            roles: Roles::SERVING.and(Roles::WRITABLE).and(Roles::COORDINATING),
            node: Some(node),
            ..peer(Roles::WRITABLE, Some(node))
        }
    }

    /// A declared peer row, as an operator would have written it.
    fn peer(roles: Roles, node: Option<[u8; NODE_ID_LEN]>) -> ReplicaDefinition {
        ReplicaDefinition {
            name: "leader".to_owned(),
            endpoint: "10.0.0.2:9000".to_owned(),
            roles,
            node,
            // Not read by `upstream` and set anyway: what the peer grants *this*
            // node lives on that peer's own catalog, not on this node's copy of
            // the row, and a value here that mattered would mean the follower
            // was deciding its own subscription.
            replicates: None,
            leads: None,
        }
    }

    #[test]
    fn a_node_that_may_write_collects_from_nobody() {
        // It is the origin of what it holds, which is exactly what
        // `current_as_of` says when it answers zero. Collecting into it would
        // apply a peer's records beside its own — the divergence the epoch chain
        // refuses at apply time, prevented here at the timer instead.
        let declared = [peer(Roles::WRITABLE, Some(NODE))];
        let heard = greeted(&[("10.0.0.2:9000", writing(Epoch::new(7)))]);
        assert_eq!(upstream(Roles::ALONE, &declared, &heard), None);
        assert_eq!(upstream(Roles::WRITABLE, &declared, &heard), None);
        // And the same node with the role taken away follows the same peer, so
        // the rule is the role and not something about the peer.
        assert_eq!(
            upstream(Roles::SERVING, &declared, &heard),
            Some((NODE, "10.0.0.2:9000".to_owned()))
        );
    }

    /// A seed, as an operator would have written it on the command line.
    fn seed(node: [u8; NODE_ID_LEN], endpoint: &str) -> Seed {
        Seed {
            node,
            endpoint: endpoint.to_owned(),
        }
    }

    #[test]
    fn a_joining_node_collects_from_the_seed_that_says_it_may_write_now() {
        // The one round that exists to break a circle: the membership lives in
        // the catalog, the catalog arrives by collecting from a member, and a
        // node that has just been told to join holds neither.
        let seeds = [seed(NODE, "10.0.0.2:9000")];
        let heard = greeted(&[("10.0.0.2:9000", writing(Epoch::new(7)))]);
        assert_eq!(
            bootstrap_from(Roles::SERVING, &seeds, &heard),
            Some((NODE, "10.0.0.2:9000".to_owned()))
        );
    }

    #[test]
    fn a_node_that_may_write_does_not_collect_from_a_seed_either() {
        // The same rule `upstream` applies, and stated separately because the
        // two functions are siblings rather than one with a flag: a rule that
        // held in one of them and not the other would be a node that ignores
        // its peers and follows a command-line address.
        let seeds = [seed(NODE, "10.0.0.2:9000")];
        let heard = greeted(&[("10.0.0.2:9000", writing(Epoch::new(7)))]);
        assert_eq!(bootstrap_from(Roles::ALONE, &seeds, &heard), None);
        assert_eq!(bootstrap_from(Roles::WRITABLE, &seeds, &heard), None);
    }

    #[test]
    fn a_seed_that_has_not_been_greeted_is_not_collected_from() {
        // Absence of a greeting is not evidence that a seed may write, and the
        // consequence here is sharper than it is for a declared peer: this node
        // holds nothing at all, so the first thing it collects is the whole of
        // what it will believe.
        let seeds = [seed(NODE, "10.0.0.2:9000")];
        assert_eq!(
            bootstrap_from(Roles::SERVING, &seeds, &Directory::new()),
            None
        );
    }

    #[test]
    fn a_seed_that_is_a_follower_is_not_collected_from() {
        // A seed is an address an operator wrote down, and which node happens to
        // be leading is not a fact an operator can write down — so a seed
        // pointing at a node that may not write is the ordinary case, not a
        // misconfiguration. The joiner waits rather than pulling from a copy.
        let seeds = [seed(NODE, "10.0.0.2:9000")];
        let heard = greeted(&[("10.0.0.2:9000", following())]);
        assert_eq!(bootstrap_from(Roles::SERVING, &seeds, &heard), None);
    }

    #[test]
    fn among_seeds_the_newer_leadership_is_collected_from() {
        // The same tie `upstream` breaks and for the same reason: a leader
        // demoted a moment ago and its successor can both be in this directory,
        // because a greeting is as fresh as the last awareness round.
        let seeds = [seed(NODE, "10.0.0.2:9000"), seed(ANOTHER, "10.0.0.3:9000")];
        let heard = greeted(&[
            ("10.0.0.2:9000", writing(Epoch::new(7))),
            ("10.0.0.3:9000", writing(Epoch::new(8))),
        ]);
        assert_eq!(
            bootstrap_from(Roles::SERVING, &seeds, &heard),
            Some((ANOTHER, "10.0.0.3:9000".to_owned()))
        );
    }

    #[test]
    fn a_follower_with_no_writable_peer_collects_from_nobody() {
        assert_eq!(upstream(Roles::SERVING, &[], &Directory::new()), None);
    }

    #[test]
    fn a_writable_peer_nobody_has_identified_cannot_be_collected_from() {
        // A peer connection demands a certificate valid for a name derived from
        // the peer's id, so an endpoint whose id nobody knows cannot be dialled
        // at all — the same wall the seed address runs into. `None` rather than
        // a half-formed attempt.
        let declared = [peer(Roles::WRITABLE, None)];
        let heard = greeted(&[("10.0.0.2:9000", writing(Epoch::new(7)))]);
        assert_eq!(upstream(Roles::SERVING, &declared, &heard), None);
    }

    #[test]
    fn a_follower_collects_from_the_peer_that_says_it_may_write_now() {
        // ADR-0065, and the configuration that forced it: ADR-0063 and ADR-0064
        // together make *every coordinating node also declared writable* the
        // only shape in which a failover produces a writer, so two writable ROWS
        // is the normal cluster rather than a misconfiguration. The row says who
        // may be followed; the greeting says which of them is the origin now.
        let declared = [
            named("one", "10.0.0.2:9000", [1; NODE_ID_LEN]),
            named("two", "10.0.0.3:9000", [2; NODE_ID_LEN]),
        ];
        let heard = greeted(&[
            ("10.0.0.2:9000", following()),
            ("10.0.0.3:9000", writing(Epoch::new(4))),
        ]);
        assert_eq!(
            upstream(Roles::SERVING, &declared, &heard),
            Some(([2; NODE_ID_LEN], "10.0.0.3:9000".to_owned())),
            "both rows are declared writable; only one of them said it may write"
        );
    }

    #[test]
    fn the_newer_leadership_wins_a_directory_holding_both() {
        // Not hypothetical. A greeting is as fresh as the last awareness round
        // and no fresher, so a leader demoted a moment ago and its successor are
        // both in this directory saying they may write. ADR-0059's ordering
        // settles it, which is the same rule a voter applies to a ballot.
        let declared = [
            named("old", "10.0.0.2:9000", [1; NODE_ID_LEN]),
            named("new", "10.0.0.3:9000", [2; NODE_ID_LEN]),
        ];
        let heard = greeted(&[
            ("10.0.0.2:9000", writing(Epoch::new(4))),
            ("10.0.0.3:9000", writing(Epoch::new(5))),
        ]);
        assert_eq!(
            upstream(Roles::SERVING, &declared, &heard),
            Some(([2; NODE_ID_LEN], "10.0.0.3:9000".to_owned()))
        );
    }

    #[test]
    fn a_peer_this_node_has_never_greeted_is_not_followed() {
        // Absence of a greeting is not evidence that a peer may write. A cold
        // node collects from nobody until its first awareness round lands —
        // one interval of not collecting, against pulling records from whichever
        // address happened to be declared first.
        let declared = [named("one", "10.0.0.2:9000", [1; NODE_ID_LEN])];
        assert_eq!(upstream(Roles::SERVING, &declared, &Directory::new()), None);
    }

    /// The row a node acquires about ITSELF the moment replication works.
    ///
    /// W382, against three processes. `DEFINE REPLICA` is a catalog write and
    /// therefore a log record, so a follower that applies a leader's store log
    /// receives the leader's membership — which names the follower. Counting it
    /// makes a cluster of three demand three grants and leaves the third
    /// uncastable, because the only node that would cast it is the candidate,
    /// whose own door refuses the connection. Every round then fails and the
    /// epoch climbs for ever with nothing in an error state.
    #[test]
    fn a_row_naming_this_node_is_not_one_of_its_own_voters() {
        let mine = Roles::SERVING.and(Roles::COORDINATING);
        let itself = peer(mine, Some(NODE));
        let other = named("two", "10.0.0.3:9000", ANOTHER);
        assert_eq!(
            voters(mine, std::slice::from_ref(&itself), &NODE),
            None,
            "a node stood a round against nobody but itself"
        );
        assert_eq!(
            voters(mine, &[itself, other], &NODE),
            Some(vec![(ANOTHER, "10.0.0.3:9000".to_owned())]),
            "the membership a round is judged against counted this node twice"
        );
    }

    #[test]
    fn a_node_the_operator_did_not_make_coordinating_stands_for_nothing() {
        // ADR-0063. The deciding set is the set a leader is drawn from, and
        // `COORDINATING` is the role that names it. §6.1 still keeps the two
        // halves apart — the DESIRED role is a catalog record the operator
        // writes, the EFFECTIVE role is a lease the cluster grants — so a node
        // outside the deciding set promoting itself because a leader went quiet
        // would be taking the decision the catalog exists to hold.
        let coordinating = peer(Roles::SERVING.and(Roles::COORDINATING), Some(NODE));
        assert_eq!(
            voters(
                Roles::SERVING,
                std::slice::from_ref(&coordinating),
                &ANOTHER
            ),
            None
        );
        assert_eq!(
            voters(Roles::NONE, std::slice::from_ref(&coordinating), &ANOTHER),
            None
        );
        // Writable is no longer what lets a node stand, and this is the pair
        // that says so: `ALONE` is `SERVING|WRITABLE`, and it stands for
        // nothing; the same node declared `COORDINATING` and never writable
        // stands.
        assert_eq!(
            voters(Roles::ALONE, std::slice::from_ref(&coordinating), &ANOTHER),
            None
        );
        assert_eq!(
            voters(
                Roles::SERVING.and(Roles::COORDINATING),
                &[coordinating],
                &ANOTHER
            ),
            Some(vec![(NODE, "10.0.0.2:9000".to_owned())])
        );
    }

    #[test]
    fn a_single_node_deployment_does_not_begin_campaigning() {
        // The guard rail ADR-0063 names as the risk a reader looks for first,
        // asserted rather than argued. `Roles::ALONE` is documented as *not
        // `COORDINATING`, because there is nothing to coordinate with*, so
        // widening who may stand makes an existing single-node store stand for
        // LESS than it did before — it never hands a store that has never needed
        // a lease a new way to stop accepting writes.
        assert!(!stands(Roles::ALONE));
        assert!(!stands(Roles::WRITABLE));
        assert!(stands(Roles::SERVING.and(Roles::COORDINATING)));
    }

    #[test]
    fn a_node_with_nobody_to_coordinate_with_stands_for_nothing() {
        // A member of the deciding set that can reach no other member is not a
        // round of one, it is a node with nothing to decide. The mine here is
        // `COORDINATING` on purpose: with `ALONE` this test would pass on the
        // eligibility rule above and stop testing the membership rule it names.
        let mine = Roles::SERVING.and(Roles::COORDINATING);
        assert_eq!(voters(mine, &[], &ANOTHER), None);
        // A declared peer that does not coordinate is not a voter either — it
        // replicates, which is a different grant entirely.
        assert_eq!(
            voters(mine, &[peer(Roles::SERVING, Some(NODE))], &ANOTHER),
            None
        );
    }

    #[test]
    fn a_voting_peer_nobody_has_identified_cannot_be_balloted() {
        // The wall `upstream` runs into, at the other cadence. A ballot travels
        // on a connection whose certificate must be valid for a name derived
        // from the peer's id, so a row naming where but not who cannot be asked
        // for anything — and dropping it from the membership matters twice over,
        // because a majority counted over members that cannot be asked is a
        // majority of a fiction.
        let named = peer(Roles::SERVING.and(Roles::COORDINATING), Some(NODE));
        let nameless = peer(Roles::SERVING.and(Roles::COORDINATING), None);
        let mine = Roles::SERVING.and(Roles::COORDINATING);
        assert_eq!(
            voters(mine, std::slice::from_ref(&nameless), &ANOTHER),
            None
        );
        assert_eq!(
            voters(mine, &[nameless, named], &ANOTHER),
            Some(vec![(NODE, "10.0.0.2:9000".to_owned())])
        );
    }

    #[test]
    fn a_failed_collection_retries_from_the_same_position() {
        let mut collecting = Collecting::new();
        let seed = Sequence::new(5);
        let refused = collecting.once(STORE, seed, |_| Err::<Sequence, ()>(()));
        assert_eq!(
            refused,
            Err(()),
            "a failed pass answered as though it landed"
        );
        assert_eq!(collecting.reached(STORE), Some(Sequence::new(5)));

        let asked = RefCell::new(Vec::new());
        // A seed the retry must NOT take: the cursor exists now, so a pass that
        // read the seed again would be a pass that forgot where it failed.
        let reached = collecting.once(STORE, Sequence::new(99), |at| {
            asked.borrow_mut().push(at);
            Ok::<Sequence, ()>(Sequence::new(at.get() + 10))
        });
        assert_eq!(
            *asked.borrow(),
            vec![Sequence::new(5)],
            "the retry asked from somewhere other than where it failed"
        );
        assert_eq!(reached, Ok(Sequence::new(15)));
    }

    #[test]
    fn a_collection_that_lands_advances_the_cursor() {
        let mut collecting = Collecting::new();
        assert_eq!(
            collecting.once(STORE, Sequence::new(1), |_| Ok::<Sequence, ()>(
                Sequence::new(9)
            )),
            Ok(Sequence::new(9))
        );
        assert_eq!(collecting.reached(STORE), Some(Sequence::new(9)));
    }

    /// The whole reason the cursor is a map: two logs, two counters.
    #[test]
    fn a_position_reached_in_one_log_is_not_a_position_in_another() {
        let prod = Reach::Namespace(NamespaceId::new(1));
        let mut collecting = Collecting::new();
        assert_eq!(collecting.reached(prod), None, "nothing collected anywhere");

        assert_eq!(
            collecting.once(STORE, Sequence::new(1), |_| Ok::<Sequence, ()>(
                Sequence::new(40)
            )),
            Ok(Sequence::new(40))
        );
        // The seed is what a namespace log with no cursor starts from, and the
        // store's log reaching 40 must not spend it: a single cursor would have
        // asked this log for position 40 and been answered a gap or a re-send
        // depending only on which log ran ahead.
        let asked = RefCell::new(Vec::new());
        assert_eq!(
            collecting.once(prod, Sequence::new(1), |at| {
                asked.borrow_mut().push(at);
                Ok::<Sequence, ()>(Sequence::new(3))
            }),
            Ok(Sequence::new(3))
        );
        assert_eq!(*asked.borrow(), vec![Sequence::new(1)]);
        assert_eq!(collecting.reached(STORE), Some(Sequence::new(40)));
        assert_eq!(collecting.reached(prod), Some(Sequence::new(3)));
    }

    /// The deadlock W256 measured against three processes, as a unit.
    #[test]
    fn a_lost_round_stands_higher_the_next_time_it_stands() {
        let mut renewing = Renewing::holding(Leadership {
            epoch: Epoch::ZERO,
            from: Instant::now(),
        });
        let stood_for = RefCell::new(Vec::new());
        let ask = |renewing: &mut Renewing, now: Instant| {
            renewing.once(NODE, now, |_, next| {
                stood_for.borrow_mut().push(next);
                Stood::Lost {
                    granted: Epoch::ZERO,
                }
            });
        };

        let opened = Instant::now();
        ask(&mut renewing, opened);
        // Far enough past any stagger that the wait is not what is being tested.
        ask(&mut renewing, opened + Duration::from_secs(30));
        ask(&mut renewing, opened + Duration::from_secs(60));

        assert_eq!(
            *stood_for.borrow(),
            vec![Epoch::new(1), Epoch::new(2), Epoch::new(3)],
            "a candidate that lost stood for the same epoch again, against \
             voters that had already spent it — which is how three healthy \
             nodes elect nobody forever"
        );
    }

    /// The number is already in the answer the candidate is given.
    #[test]
    fn a_refusal_that_names_a_granted_epoch_is_learned_from() {
        let mut renewing = Renewing::holding(Leadership {
            epoch: Epoch::ZERO,
            from: Instant::now(),
        });
        let opened = Instant::now();
        renewing.once(NODE, opened, |_, _| Stood::Lost {
            granted: Epoch::new(50),
        });
        let stood_for = RefCell::new(None);
        renewing.once(NODE, opened + Duration::from_secs(30), |_, next| {
            *stood_for.borrow_mut() = Some(next);
            Stood::Lost {
                granted: Epoch::ZERO,
            }
        });
        assert_eq!(
            *stood_for.borrow(),
            Some(Epoch::new(51)),
            "a node that never led holds Epoch::ZERO however far the cluster \
             has got, so without adopting what the refusals report it would \
             climb one epoch per round to reach the conversation"
        );
    }

    /// A lost round is not retried on the same tick as everyone else's.
    #[test]
    fn a_candidate_that_just_lost_waits_before_standing_again() {
        let mut renewing = Renewing::holding(Leadership {
            epoch: Epoch::ZERO,
            from: Instant::now(),
        });
        let opened = Instant::now();
        renewing.once(NODE, opened, |_, _| Stood::Lost {
            granted: Epoch::ZERO,
        });
        let asked = RefCell::new(false);
        renewing.once(NODE, opened, |_, _| {
            *asked.borrow_mut() = true;
            Stood::Lost {
                granted: Epoch::ZERO,
            }
        });
        assert!(
            !*asked.borrow(),
            "a candidate stood again on the same instant it lost, which is how \
             three of them split every epoch as reliably as they split the first"
        );
    }

    /// And the wait is different per node, which is the whole of the property.
    #[test]
    fn two_candidates_do_not_come_back_at_the_same_instant() {
        let opened = Instant::now();
        let waited = |candidate: [u8; NODE_ID_LEN]| {
            let mut renewing = Renewing::holding(Leadership {
                epoch: Epoch::ZERO,
                from: opened,
            });
            renewing.once(candidate, opened, |_, _| Stood::Lost {
                granted: Epoch::ZERO,
            });
            // The first instant at which it will ask again, found by asking.
            (0..2000)
                .map(|millis| opened + Duration::from_millis(millis))
                .find(|at| {
                    let asked = RefCell::new(false);
                    renewing.once(candidate, *at, |_, _| {
                        *asked.borrow_mut() = true;
                        Stood::NotDue
                    });
                    *asked.borrow()
                })
                .expect("a stagger inside one round time")
        };
        assert_ne!(
            waited([1; NODE_ID_LEN]),
            waited([2; NODE_ID_LEN]),
            "two candidates that lost together came back together"
        );
    }

    /// The epoch is in the mix, so an unlucky pair does not collide forever.
    #[test]
    fn a_pair_that_collides_at_one_epoch_is_not_condemned_to_collide_at_every_one() {
        let offsets = |epoch: Epoch| {
            (0_u8..64)
                .map(|seed| Renewing::stagger([seed; NODE_ID_LEN], epoch))
                .collect::<Vec<_>>()
        };
        assert_ne!(
            offsets(Epoch::new(1)),
            offsets(Epoch::new(2)),
            "the offsets did not move with the epoch, so a pair whose ids fall \
             close together would collide at every epoch there is"
        );
    }

    #[test]
    fn a_node_that_can_hear_a_leader_does_not_stand_against_it() {
        let leader = [9; NODE_ID_LEN];
        let declared = [named("leader", "10.0.0.1:9000", leader)];
        let heard = greeted(&[("10.0.0.1:9000", writing(Epoch::new(7)))]);
        assert!(
            heard_a_leader(
                &declared,
                &heard,
                None,
                Instant::now(),
                tessari_storage::LEASE_TTL
            ),
            "a follower stood against a leader it had just heard from, and its \
             own self-vote then refuses that leader's renewal for a whole lease"
        );
    }

    #[test]
    fn a_greeting_older_than_the_lease_holds_nobody_back() {
        let leader = [9; NODE_ID_LEN];
        let declared = [named("leader", "10.0.0.1:9000", leader)];
        let mut heard = Directory::new();
        let long_ago = Instant::now();
        heard.heard("10.0.0.1:9000", writing(Epoch::new(7)), long_ago);
        assert!(
            !heard_a_leader(
                &declared,
                &heard,
                None,
                long_ago + tessari_storage::LEASE_TTL + Duration::from_secs(1),
                tessari_storage::LEASE_TTL
            ),
            "a greeting older than the leader's own lease cannot testify that \
             the leader still holds it, and a node that hears nothing has to \
             stand — that is what an election is for"
        );
    }

    #[test]
    fn a_peer_that_is_not_the_origin_is_not_a_leader() {
        let peer = [9; NODE_ID_LEN];
        let declared = [named("peer", "10.0.0.1:9000", peer)];
        // `following()` carries a non-zero `current_as_of`: it holds somebody
        // else's writes, so it is not leading whatever its catalog row says.
        let heard = greeted(&[("10.0.0.1:9000", following())]);
        assert!(
            !heard_a_leader(
                &declared,
                &heard,
                None,
                Instant::now(),
                tessari_storage::LEASE_TTL
            ),
            "a follower was mistaken for a leader, so a cluster whose leader \
             died would never elect another"
        );
    }

    #[test]
    fn a_grant_this_node_made_holds_it_back_when_the_directory_is_already_stale() {
        let leader = [9; NODE_ID_LEN];
        let declared = [named("leader", "10.0.0.1:9000", leader)];
        let mut heard = Directory::new();
        let long_ago = Instant::now();
        heard.heard("10.0.0.1:9000", writing(Epoch::new(7)), long_ago);
        // One whole lease after the greeting, which is the moment the directory
        // stops testifying — and four seconds after a renewal this node granted,
        // which is the whole point: a leader renews about every six seconds and
        // the directory is refreshed every ten.
        let now = long_ago + tessari_storage::LEASE_TTL + Duration::from_secs(1);
        let granted = now - Duration::from_secs(4);
        assert!(
            heard_a_leader(
                &declared,
                &heard,
                Some(granted),
                now,
                tessari_storage::LEASE_TTL
            ),
            "the directory had aged out but this node had granted that leader a \
             renewal four seconds ago — standing against a leader it just \
             acknowledged is exactly what the quiet-cluster gate exists to stop"
        );
    }

    #[test]
    fn a_grant_older_than_the_lease_holds_nobody_back_either() {
        let declared = [named("leader", "10.0.0.1:9000", [9; NODE_ID_LEN])];
        let now = Instant::now();
        let granted = now - tessari_storage::LEASE_TTL - Duration::from_secs(1);
        assert!(
            !heard_a_leader(
                &declared,
                &Directory::new(),
                Some(granted),
                now,
                tessari_storage::LEASE_TTL
            ),
            "a grant older than the lease it granted cannot testify that the \
             holder still has it, and a node that hears nothing has to stand"
        );
    }

    /// A greeting from a node running the policy set at `(epoch, version)`.
    fn running(epoch: u64, version: u64) -> Hello {
        Hello {
            policy: Some(FailoverStamp {
                epoch: Epoch::new(epoch),
                version,
            }),
            ..said()
        }
    }

    fn stamp(epoch: u64, version: u64) -> FailoverStamp {
        FailoverStamp {
            epoch: Epoch::new(epoch),
            version,
        }
    }

    #[test]
    fn a_peer_running_a_newer_failover_policy_holds_this_node_back() {
        let declared = [named("peer", "10.0.0.1:9000", [9; NODE_ID_LEN])];
        let heard = greeted(&[("10.0.0.1:9000", running(4, 0))]);
        assert_eq!(
            heard_a_newer_policy(
                &declared,
                &heard,
                Some(stamp(3, 9)),
                Instant::now(),
                tessari_storage::LEASE_TTL
            ),
            Some(stamp(4, 0)),
            "a candidate timing itself by a policy the cluster has replaced was              not held back, which is the disagreement the policy row exists to              remove arriving at the moment it decides an outcome"
        );
    }

    #[test]
    fn an_equal_or_older_policy_holds_nobody_back() {
        let declared = [named("peer", "10.0.0.1:9000", [9; NODE_ID_LEN])];
        let now = Instant::now();
        for (peer, mine, why) in [
            (
                running(3, 9),
                stamp(3, 9),
                "an equal pair is the ordinary state of an agreeing cluster and                  must never stop an election",
            ),
            (
                running(3, 8),
                stamp(3, 9),
                "a lower version is a peer that is behind, which is the ordinary                  state of a follower and not a reason to refuse",
            ),
            (
                running(2, 99),
                stamp(3, 0),
                "a superseded leadership does not win on version — this is the                  partitioned ex-leader reconnecting, and letting it silence a                  candidate would hand it the outcome it lost",
            ),
        ] {
            assert_eq!(
                heard_a_newer_policy(
                    &declared,
                    &greeted(&[("10.0.0.1:9000", peer)]),
                    Some(mine),
                    now,
                    tessari_storage::LEASE_TTL
                ),
                None,
                "{why}"
            );
        }
    }

    #[test]
    fn a_node_that_holds_no_policy_at_all_is_behind_one_that_does() {
        let declared = [named("peer", "10.0.0.1:9000", [9; NODE_ID_LEN])];
        let heard = greeted(&[("10.0.0.1:9000", running(1, 0))]);
        // The first policy a cluster ever sets is the case this covers. Treating
        // *no policy* as unbeatable would make that first one the single policy
        // nothing could ever act on.
        assert_eq!(
            heard_a_newer_policy(
                &declared,
                &heard,
                None,
                Instant::now(),
                tessari_storage::LEASE_TTL
            ),
            Some(stamp(1, 0))
        );
        // And the other direction: a peer that says nothing supersedes nothing,
        // so a build from before the field cannot silence the cluster it joins.
        assert_eq!(
            heard_a_newer_policy(
                &declared,
                &greeted(&[("10.0.0.1:9000", said())]),
                None,
                Instant::now(),
                tessari_storage::LEASE_TTL
            ),
            None,
            "a greeting carrying no policy silenced a candidate, which would              make a rolling upgrade an outage"
        );
    }

    #[test]
    fn a_greeting_older_than_the_lease_cannot_hold_a_candidate_back() {
        // This is what makes the gate incapable of deadlocking a cluster: the
        // refusal is bounded by audibility, so the node holding the newer policy
        // going away opens the gate rather than closing it forever.
        let declared = [named("peer", "10.0.0.1:9000", [9; NODE_ID_LEN])];
        let mut heard = Directory::new();
        let long_ago = Instant::now();
        heard.heard("10.0.0.1:9000", running(4, 0), long_ago);
        let now = long_ago + tessari_storage::LEASE_TTL + Duration::from_secs(1);
        assert_eq!(
            heard_a_newer_policy(&declared, &heard, None, now, tessari_storage::LEASE_TTL),
            None,
            "a peer nobody has heard from in longer than a lease was still              silencing this node, so a cluster that lost the one node holding              the newer policy could never elect again"
        );
    }

    #[test]
    fn a_policy_advertised_by_an_undeclared_address_is_not_a_member_speaking() {
        // The same rule `heard_a_leader` and `upstream` hold: a greeting from an
        // address this node's catalog does not declare is a stranger, and a
        // stranger that can silence a candidate is a denial of service with a
        // one-line implementation.
        let declared = [named("peer", "10.0.0.1:9000", [9; NODE_ID_LEN])];
        let heard = greeted(&[("10.0.0.9:9000", running(4, 0))]);
        assert_eq!(
            heard_a_newer_policy(
                &declared,
                &heard,
                None,
                Instant::now(),
                tessari_storage::LEASE_TTL
            ),
            None
        );
    }

    #[test]
    fn the_newest_policy_heard_is_the_one_reported() {
        // Reported rather than merely detected, so an operator reading the log
        // line knows WHICH policy this node is behind. With several peers at
        // several stamps, the answer has to be the newest or the report names a
        // policy that is itself superseded.
        let declared = [
            named("one", "10.0.0.1:9000", [9; NODE_ID_LEN]),
            named("two", "10.0.0.2:9000", [8; NODE_ID_LEN]),
            named("three", "10.0.0.3:9000", [7; NODE_ID_LEN]),
        ];
        let heard = greeted(&[
            ("10.0.0.1:9000", running(4, 1)),
            ("10.0.0.2:9000", running(5, 0)),
            ("10.0.0.3:9000", running(4, 9)),
        ]);
        assert_eq!(
            heard_a_newer_policy(
                &declared,
                &heard,
                Some(stamp(3, 0)),
                Instant::now(),
                tessari_storage::LEASE_TTL
            ),
            Some(stamp(5, 0))
        );
    }

    #[test]
    fn a_node_that_has_granted_nothing_is_answered_by_the_directory_exactly_as_before() {
        let declared = [named("leader", "10.0.0.1:9000", [9; NODE_ID_LEN])];
        let heard = greeted(&[("10.0.0.1:9000", writing(Epoch::new(7)))]);
        // `None` is *no such evidence*, never *no leader*. A follower outside
        // the deciding set grants nothing and must still be held back by a
        // greeting, or this change would make every non-voter campaign.
        assert!(
            heard_a_leader(
                &declared,
                &heard,
                None,
                Instant::now(),
                tessari_storage::LEASE_TTL
            ),
            "a node with no grant to read was not held back by a fresh greeting"
        );
    }

    #[test]
    fn a_renewal_that_wins_nothing_keeps_the_lease_it_holds() {
        let held = Leadership {
            epoch: Epoch::new(4),
            from: Instant::now(),
        };
        let mut renewing = Renewing::holding(held);
        let standing = renewing.once(NODE, Instant::now(), |_, _| Stood::NotDue);
        assert_eq!(standing, held, "a round that won nothing changed the lease");
        assert_eq!(renewing.standing(), held);
    }

    #[test]
    fn a_renewal_stands_for_the_epoch_after_the_one_it_holds() {
        let from = Instant::now();
        let mut renewing = Renewing::holding(Leadership {
            epoch: Epoch::new(4),
            from,
        });
        let stood_for = RefCell::new(Vec::new());
        let won = Leadership {
            epoch: Epoch::new(5),
            from,
        };
        let standing = renewing.once(NODE, from, |_: Lease, next| {
            stood_for.borrow_mut().push(next);
            Stood::Won(won)
        });
        assert_eq!(*stood_for.borrow(), vec![Epoch::new(5)]);
        assert_eq!(standing, won, "a round that was won was not taken up");
    }

    #[test]
    fn a_delayed_cadence_runs_once_however_many_periods_it_missed() {
        let period = Duration::from_secs(10);
        let ran_at = Instant::now();
        let late = ran_at
            .checked_add(Duration::from_secs(35))
            .expect("an instant 35s from now");
        assert_eq!(
            due_in(period, ran_at, late),
            Duration::ZERO,
            "a pass that overran by three periods asked for more than one catch-up"
        );
    }

    #[test]
    fn a_cadence_that_is_early_waits_out_the_remainder() {
        let period = Duration::from_secs(10);
        let ran_at = Instant::now();
        let soon = ran_at
            .checked_add(Duration::from_secs(3))
            .expect("an instant 3s from now");
        assert_eq!(due_in(period, ran_at, soon), Duration::from_secs(7));
    }

    #[test]
    fn a_cadence_runs_no_pass_once_the_node_is_asked_to_stop() {
        let stopping = Stopping::new();
        stopping.refuse_new();
        let passes = RefCell::new(0_usize);
        every(Duration::ZERO, &stopping, |_| {
            *passes.borrow_mut() += 1;
        });
        assert_eq!(
            *passes.borrow(),
            0,
            "a node already stopping still ran a cadence pass"
        );
    }

    #[test]
    fn the_published_directory_answers_the_routing_question_a_read_asks() {
        // The join this whole module was built for. Until this wave the rounds
        // were written and read by nobody, so the test asserts the *reading*:
        // what a session gets back when it asks the published answer, not what
        // the greeting side put there.
        use tessari_session::Elsewhere as _;

        let mut directory = Directory::new();
        directory.heard("two.example:9080", said(), Instant::now());
        let published = Published::holding(directory);

        let found = published
            .within(Duration::from_secs(30))
            .expect("a peer one second behind is within thirty");
        assert_eq!(found.endpoint, "two.example:9080");
        assert_eq!(
            found.node, NODE,
            "the redirect must carry who is there, or it cannot be checked on arrival"
        );
    }

    #[test]
    fn a_peer_beyond_the_bound_is_not_a_peer_the_routing_question_offers() {
        // §C-05's *exclude, never mark*, at the surface a read actually asks.
        // An implementation that answered with its freshest peer regardless
        // would turn the bound from a promise back into a hope, and the caller
        // has no way to tell the two apart.
        use tessari_session::Elsewhere as _;

        let mut directory = Directory::new();
        directory.heard("two.example:9080", said(), Instant::now());
        let published = Published::holding(directory);

        assert!(
            published.within(Duration::from_millis(500)).is_none(),
            "a copy a second behind was offered to a read that would take half of one"
        );
    }

    #[test]
    fn a_node_that_has_greeted_nobody_offers_nowhere() {
        // The single-node case, which is every deployment that was never told
        // about peers. It must answer *not that I know of* rather than
        // inventing a candidate, because the caller turns that answer straight
        // into a refusal.
        use tessari_session::Elsewhere as _;

        let published = Published::holding(Directory::new());
        assert!(published.within(Duration::from_secs(86_400)).is_none());
    }

    #[test]
    fn a_greeting_round_carries_previous_readings_forward() {
        let published = Published::holding(Directory::new());
        let first = Instant::now();
        published.round(|directory| directory.heard("one:9080", said(), first));

        published.round(|directory| directory.heard("two:9080", said(), first));

        let current = published.current();
        assert!(
            current.age_of("one:9080", first).is_some(),
            "the peer heard in the first round vanished in the second"
        );
        assert!(current.age_of("two:9080", first).is_some());
    }

    #[test]
    fn a_greeting_round_publishes_nothing_until_it_is_done() {
        let published = Published::holding(Directory::new());
        let at = Instant::now();
        let during = RefCell::new(None);
        published.round(|directory| {
            directory.heard("one:9080", said(), at);
            *during.borrow_mut() = Some(Arc::clone(&published.current()));
        });
        let seen = during.borrow().clone().expect("the round ran");
        assert!(
            seen.age_of("one:9080", at).is_none(),
            "a reader saw a half-finished round"
        );
        assert!(published.current().age_of("one:9080", at).is_some());
    }

    #[test]
    fn an_empty_catalog_names_no_peer() {
        assert!(!names_a_peer(&[], &NODE));
    }

    #[test]
    fn a_catalog_naming_another_node_names_a_peer() {
        let declared = [named("leader", "10.0.0.2:9000", ANOTHER)];
        assert!(names_a_peer(&declared, &NODE));
    }

    /// The shape W260 found, and the reason this predicate is not `is_empty`.
    ///
    /// A cluster admits a newcomer by writing a row that describes the
    /// NEWCOMER, so the first row the newcomer ever collects is its own. It
    /// names a member — itself — and answers nothing about who to follow, since
    /// `upstream` and `greet_round` both skip it.
    #[test]
    fn a_catalog_holding_only_this_nodes_own_row_names_no_peer() {
        let declared = [named("joiner", "10.0.0.9:9000", NODE)];
        assert!(!names_a_peer(&declared, &NODE));
    }

    #[test]
    fn a_row_naming_no_node_names_no_peer() {
        let declared = [peer(Roles::WRITABLE, None)];
        assert!(!names_a_peer(&declared, &NODE));
    }

    #[test]
    fn one_row_for_somebody_else_is_enough_beside_this_nodes_own() {
        let declared = [
            named("joiner", "10.0.0.9:9000", NODE),
            named("leader", "10.0.0.2:9000", ANOTHER),
        ];
        assert!(names_a_peer(&declared, &NODE));
    }

    #[test]
    fn a_routing_answer_carries_the_epoch_the_named_node_claimed() {
        // The value a redirect is DATED by, and the one thing this node could
        // not have produced from its own state: `said()` claims epoch 7 and this
        // node holds no leadership at all. An implementation that reached for
        // `Db::leading()` here would answer `None` and have to invent a
        // placeholder, which is how an undateable redirect gets shipped looking
        // exactly like a dated one.
        use tessari_session::Elsewhere as _;

        let mut directory = Directory::new();
        directory.heard("10.0.0.2:9000", said(), Instant::now());
        let published = Published::holding(directory);

        let peer = published
            .within(Duration::from_secs(30))
            .expect("a serving peer one second behind is inside a thirty-second bound");

        assert_eq!(peer.endpoint, "10.0.0.2:9000");
        assert_eq!(peer.node, NODE);
        assert_eq!(
            peer.epoch,
            Epoch::new(7),
            "the routing answer lost the leadership the named node published"
        );
    }

    // ---- G032 S4: a placed range's leader, heard and followed ---------------

    fn shard(n: u32) -> Reach {
        Reach::Shard(
            NamespaceId::new(1),
            tessari_types::DatabaseId::new(1),
            tessari_types::TableId::new(1),
            tessari_types::ShardId::new(n),
        )
    }

    /// A greeting from `node` standing for `range`, leading it at `leading`.
    fn on_a_line(node: [u8; NODE_ID_LEN], range: Reach, leading: u64) -> Hello {
        Hello {
            node,
            line: Some(crate::peer::Line {
                range,
                leading: Epoch::new(leading),
                tail: Sequence::new(3),
                tail_leadership: Epoch::new(1),
            }),
            ..following()
        }
    }

    #[test]
    fn a_placed_ranges_leader_is_the_peer_whose_greeting_holds_its_line_live() {
        let declared = [
            named("a", "10.0.0.1:9000", NODE),
            named("b", "10.0.0.2:9000", ANOTHER),
        ];
        let heard = greeted(&[
            ("10.0.0.1:9000", on_a_line(NODE, shard(2), 3)),
            ("10.0.0.2:9000", on_a_line(ANOTHER, shard(2), 0)),
        ]);
        assert_eq!(
            leader_of_range(shard(2), &declared, &heard),
            Some((NODE, "10.0.0.1:9000".to_owned()))
        );
        assert_eq!(leader_of_range(shard(3), &declared, &heard), None);
        // A lapsed line alone leads nothing — asked without a live greeting
        // beside it, which the choice of the highest epoch would absorb.
        let lapsed = greeted(&[("10.0.0.2:9000", on_a_line(ANOTHER, shard(2), 0))]);
        assert_eq!(leader_of_range(shard(2), &declared, &lapsed), None);
        // A greeting under somebody else's row names nobody.
        let crossed = greeted(&[("10.0.0.2:9000", on_a_line(NODE, shard(2), 3))]);
        assert_eq!(leader_of_range(shard(2), &declared, &crossed), None);
    }

    #[test]
    fn a_node_hears_a_ranges_leader_only_on_that_ranges_line() {
        let declared = [
            named("a", "10.0.0.1:9000", NODE),
            named("me", "10.0.0.2:9000", ANOTHER),
        ];
        let now = Instant::now();
        let within = tessari_storage::LEASE_TTL;
        let heard = greeted(&[("10.0.0.1:9000", on_a_line(NODE, shard(2), 3))]);
        assert!(heard_a_leader_on(
            shard(2),
            ANOTHER,
            &declared,
            &heard,
            None,
            now,
            within
        ));
        assert!(!heard_a_leader_on(
            shard(3),
            ANOTHER,
            &declared,
            &heard,
            None,
            now,
            within
        ));
        // Its own greeting is not a leader it can hear.
        let mine = greeted(&[("10.0.0.2:9000", on_a_line(ANOTHER, shard(2), 3))]);
        assert!(!heard_a_leader_on(
            shard(2),
            ANOTHER,
            &declared,
            &mine,
            None,
            now,
            within
        ));
        // And a grant to somebody else on the line is heard without a greeting.
        assert!(heard_a_leader_on(
            shard(3),
            ANOTHER,
            &declared,
            &Directory::new(),
            Some(now),
            now,
            within
        ));
    }

    #[test]
    fn a_node_stands_for_the_range_its_own_row_places() {
        let mut placed = named("a", "10.0.0.1:9000", NODE);
        placed.leads = Some(shard(2));
        let declared = [placed, named("b", "10.0.0.2:9000", ANOTHER)];
        assert_eq!(stands_for(&declared, &NODE), Some(shard(2)));
        assert_eq!(stands_for(&declared, &ANOTHER), None);
    }
}
