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

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tessari_encoding::{NODE_ID_LEN, Roles};
use tessari_serve::Stopping;
use tessari_storage::{Lease, ReplicaDefinition};
use tessari_types::{Epoch, Sequence};

use crate::directory::{Destination, Directory};
use crate::grant::Leadership;

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
        match self.current().read_within(None, bound, Instant::now()) {
            Destination::There { endpoint, node } => Some(tessari_session::Peer { endpoint, node }),
            // `Here` cannot arise with no currency of our own offered, and
            // `Nowhere` is the answer the caller already holds. Both mean *not
            // that I know of*, which is what `None` says.
            Destination::Here | Destination::Nowhere => None,
        }
    }
}

/// How far this node has collected, and what a failed collection does to it.
#[derive(Debug)]
pub struct Collecting {
    at: Sequence,
}

impl Collecting {
    /// Start collecting from `at` — the first position this node does not hold.
    #[must_use]
    pub fn from(at: Sequence) -> Self {
        Self { at }
    }

    /// The position this node has reached.
    #[must_use]
    pub fn reached(&self) -> Sequence {
        self.at
    }

    /// One collection. The cursor moves **only** when the pass answers.
    ///
    /// # Why a failure leaves the cursor alone
    ///
    /// A cursor advanced past records that were never applied skips them
    /// permanently and silently: the next pass asks for what comes after, no
    /// later pass ever asks for the gap, and nothing is in an error state to say
    /// so. Leaving it costs a repeated request when the peer comes back, which
    /// is the failure worth having.
    pub fn once<E>(&mut self, pass: impl FnOnce(Sequence) -> Result<Sequence, E>) -> Sequence {
        if let Ok(reached) = pass(self.at) {
            self.at = reached;
        }
        self.at
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
#[must_use]
pub fn voters(
    mine: Roles,
    declared: &[ReplicaDefinition],
) -> Option<Vec<([u8; NODE_ID_LEN], String)>> {
    if !stands(mine) {
        return None;
    }
    let voting: Vec<_> = declared
        .iter()
        .filter(|peer| peer.roles.has(Roles::COORDINATING))
        .filter_map(|peer| Some((peer.node?, peer.endpoint.clone())))
        .collect();
    (!voting.is_empty()).then_some(voting)
}

/// The leadership this node holds, and what a lost round does to it.
#[derive(Debug)]
pub struct Renewing {
    standing: Leadership,
}

impl Renewing {
    /// Hold `standing` until something better is won.
    #[must_use]
    pub fn holding(standing: Leadership) -> Self {
        Self { standing }
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
    pub fn once(&mut self, pass: impl FnOnce(Lease, Epoch) -> Option<Leadership>) -> Leadership {
        let next = Epoch::new(self.standing.epoch.get().saturating_add(1));
        if let Some(won) = pass(self.standing.lease(), next) {
            self.standing = won;
        }
        self.standing
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
    use tessari_types::{Epoch, Sequence};

    use super::{
        Collecting, Published, Renewing, ReplicaDefinition, due_in, every, stands, upstream, voters,
    };
    use crate::directory::Directory;
    use crate::grant::Leadership;
    use crate::peer::Hello;

    const NODE: [u8; NODE_ID_LEN] = [7; NODE_ID_LEN];

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
            id: 1,
            name: "leader".to_owned(),
            endpoint: "10.0.0.2:9000".to_owned(),
            roles,
            node,
            // Not read by `upstream` and set anyway: what the peer grants *this*
            // node lives on that peer's own catalog, not on this node's copy of
            // the row, and a value here that mattered would mean the follower
            // was deciding its own subscription.
            replicates: None,
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
            voters(Roles::SERVING, std::slice::from_ref(&coordinating)),
            None
        );
        assert_eq!(
            voters(Roles::NONE, std::slice::from_ref(&coordinating)),
            None
        );
        // Writable is no longer what lets a node stand, and this is the pair
        // that says so: `ALONE` is `SERVING|WRITABLE`, and it stands for
        // nothing; the same node declared `COORDINATING` and never writable
        // stands.
        assert_eq!(
            voters(Roles::ALONE, std::slice::from_ref(&coordinating)),
            None
        );
        assert_eq!(
            voters(Roles::SERVING.and(Roles::COORDINATING), &[coordinating]),
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
        assert_eq!(voters(mine, &[]), None);
        // A declared peer that does not coordinate is not a voter either — it
        // replicates, which is a different grant entirely.
        assert_eq!(voters(mine, &[peer(Roles::SERVING, Some(NODE))]), None);
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
        assert_eq!(voters(mine, std::slice::from_ref(&nameless)), None);
        assert_eq!(
            voters(mine, &[nameless, named]),
            Some(vec![(NODE, "10.0.0.2:9000".to_owned())])
        );
    }

    #[test]
    fn a_failed_collection_retries_from_the_same_position() {
        let mut collecting = Collecting::from(Sequence::new(5));
        let reached = collecting.once(|_| Err::<Sequence, ()>(()));
        assert_eq!(reached, Sequence::new(5), "a failed pass moved the cursor");
        assert_eq!(collecting.reached(), Sequence::new(5));

        let asked = RefCell::new(Vec::new());
        let reached = collecting.once(|at| {
            asked.borrow_mut().push(at);
            Ok::<Sequence, ()>(Sequence::new(at.get() + 10))
        });
        assert_eq!(
            *asked.borrow(),
            vec![Sequence::new(5)],
            "the retry asked from somewhere other than where it failed"
        );
        assert_eq!(reached, Sequence::new(15));
    }

    #[test]
    fn a_collection_that_lands_advances_the_cursor() {
        let mut collecting = Collecting::from(Sequence::new(1));
        assert_eq!(
            collecting.once(|_| Ok::<Sequence, ()>(Sequence::new(9))),
            Sequence::new(9)
        );
        assert_eq!(collecting.reached(), Sequence::new(9));
    }

    #[test]
    fn a_renewal_that_wins_nothing_keeps_the_lease_it_holds() {
        let held = Leadership {
            epoch: Epoch::new(4),
            from: Instant::now(),
        };
        let mut renewing = Renewing::holding(held);
        let standing = renewing.once(|_, _| None);
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
        let standing = renewing.once(|_: Lease, next| {
            stood_for.borrow_mut().push(next);
            Some(won)
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
}
