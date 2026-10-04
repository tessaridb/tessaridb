//! The three cadences a node runs, and why each gets a task of its own.
//!
//! # Three cadences, three tasks
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
//! task's fate. The sharpest consequence is the last: `collect` and `renew`
//! both dial peers, so a collection blocked on a dead peer's TCP connect would
//! hold up a renewal whose fence is closing. The cadence with the tightest
//! deadline would be delayed by the one with the loosest, for no reason beyond
//! their sharing a loop.
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

mod leadership;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tessari_encoding::{NODE_ID_LEN, NodeVersion, Roles};
use tessari_storage::ReplicaDefinition;
use tessari_types::{Reach, Sequence};
use tokio_util::sync::CancellationToken;

use crate::directory::{Destination, Directory};
use crate::joining::Seed;
pub use leadership::{
    Renewing, campaign_line, campaigns_for, election_timeout, heard_a_leader, heard_a_leader_on,
    heard_a_newer_policy, leader_of_range, preferred_to_yield_to, released, stands, stands_for,
    stands_for_the_store, voters,
};

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
/// The token is checked **before** each pass, so a node already stopping runs
/// none, and it ends the wait between passes the moment it is cancelled — a
/// cadence holds nothing a stop would have to wait for.
///
/// Each pass runs on the runtime's blocking pool, because a pass is store work
/// and a TLS call, both synchronous. The closure is moved there and back, so a
/// pass keeps what it learned between rounds, and one cadence's passes never
/// overlap: the campaign's ballots stay one after another. A pass that panics
/// is re-raised on this task, where the supervisor that started it sees it.
///
/// The token is the node's own stop rather than one of this module's. A driver
/// with a private flag gives a process two ways to ask a node to stop, and the
/// state between them — a node that has stopped serving while it goes on
/// dialling peers — is worse than either.
pub async fn every<P>(period: Duration, stop: &CancellationToken, mut pass: P)
where
    P: FnMut(Instant) + Send + 'static,
{
    while !stop.is_cancelled() {
        let ran_at = Instant::now();
        pass = match tokio::task::spawn_blocking(move || {
            pass(ran_at);
            pass
        })
        .await
        {
            Ok(pass) => pass,
            Err(ended) => match ended.try_into_panic() {
                Ok(payload) => std::panic::resume_unwind(payload),
                // Cancelled: the runtime is shutting down under it.
                Err(_) => return,
            },
        };
        tokio::select! {
            biased;
            () = stop.cancelled() => return,
            () = tokio::time::sleep(due_in(period, ran_at, Instant::now())) => {}
        }
    }
}

/// [`every`], where each pass names how long until the next one, and `wake`
/// starts the next one early.
///
/// For the two rounds whose right period depends on what the last pass found
/// (G053 SG2b): a node that can name no leader greets every round time rather
/// than every awareness interval, because that is when a stale directory costs
/// the most; and a follower whose stream ended collects again the moment the
/// greeting round has found where its leader went, rather than up to a period
/// later. A [`Notify`](tokio::sync::Notify) keeps one permit, so a wake that
/// arrives while a pass is running is not lost — the next wait returns at once.
pub async fn every_paced<P>(stop: &CancellationToken, wake: &tokio::sync::Notify, mut pass: P)
where
    P: FnMut(Instant) -> Duration + Send + 'static,
{
    while !stop.is_cancelled() {
        let ran_at = Instant::now();
        let (returned, period) = match tokio::task::spawn_blocking(move || {
            let period = pass(ran_at);
            (pass, period)
        })
        .await
        {
            Ok(ran) => ran,
            Err(ended) => match ended.try_into_panic() {
                Ok(payload) => std::panic::resume_unwind(payload),
                // Cancelled: the runtime is shutting down under it.
                Err(_) => return,
            },
        };
        pass = returned;
        tokio::select! {
            biased;
            () = stop.cancelled() => return,
            () = wake.notified() => {}
            () = tokio::time::sleep(due_in(period, ran_at, Instant::now())) => {}
        }
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

    /// The named node, if the last round heard it at that address carrying
    /// `SERVING` — a drained node still greets, and sending a client to it is
    /// what draining exists to prevent, exactly as [`Directory::read_within`]
    /// decides. The node id is compared because the address alone is not the
    /// node: a row naming one node at an address another answers from is not a
    /// place this node can vouch for.
    fn serving(&self, endpoint: &str, node: &[u8; NODE_ID_LEN]) -> Option<tessari_session::Peer> {
        let directory = self.current();
        let heard = directory.at(endpoint)?;
        (heard.said.node == *node && heard.said.roles.has(Roles::SERVING)).then(|| {
            tessari_session::Peer {
                endpoint: endpoint.to_owned(),
                node: *node,
                epoch: heard.said.epoch,
            }
        })
    }

    /// Who the last round heard leading `range`'s line.
    fn leading(&self, range: tessari_types::Reach) -> Option<tessari_session::Peer> {
        let (endpoint, node, epoch) = self.current().leading(range)?;
        Some(tessari_session::Peer {
            endpoint,
            node,
            epoch,
        })
    }

    /// What the last round heard the node at `endpoint` greet as.
    fn build_at(&self, endpoint: &str) -> Option<NodeVersion> {
        self.current().at(endpoint).map(|heard| heard.said.build)
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

#[cfg(test)]
mod tests;
