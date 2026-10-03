//! Who leads, who votes, who may stand, and the lease a leader renews.

use crate::campaign::Stood;
use crate::directory::Directory;
use crate::grant::Leadership;
use std::time::{Duration, Instant};
use tessari_encoding::{NODE_ID_LEN, Roles};
use tessari_storage::{FailoverStamp, Lease, ReplicaDefinition};
use tessari_types::{Epoch, Reach};

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

/// The range this node campaigns for: its placement, unless the row is giving
/// it back to the store line (ADR-0098 D3).
///
/// [`stands_for`] still answers the placement: a releasing node holds the
/// range's line and may lead it until its lease runs out, so it keeps saying
/// so and keeps its records — it only stops asking to lead it again.
#[must_use]
pub fn campaigns_for(declared: &[ReplicaDefinition], me: &[u8; NODE_ID_LEN]) -> Option<Reach> {
    declared
        .iter()
        .find(|peer| peer.node.as_ref() == Some(me))
        .filter(|peer| !peer.releasing)
        .and_then(|peer| peer.leads)
}

/// The ranges being given back to the store line: placed, and only by rows
/// that are releasing them (ADR-0098 D3). The store line's leader stands for
/// each on its own line, and folds the placement away once it leads it.
#[must_use]
pub fn released(declared: &[ReplicaDefinition]) -> Vec<Reach> {
    declared
        .iter()
        .filter(|peer| peer.releasing)
        .filter_map(|peer| peer.leads)
        .filter(|range| {
            !declared
                .iter()
                .any(|peer| !peer.releasing && peer.leads == Some(*range))
        })
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// The one placed line a node campaigns on: its own placement, or — when it
/// leads the store line and is placed nowhere — the first range being given
/// back (ADR-0098 D3).
///
/// One at most, as [`stands_for`] keeps it (Q-797): a node never leads two
/// placed lines, so a store leader with a placement of its own takes nothing
/// back until that placement moves, and two released ranges fold one after the
/// other. A voter asks the same question of the candidate (`places`), so the
/// two sides cannot disagree about who may lead a released range.
#[must_use]
pub fn campaign_line(
    declared: &[ReplicaDefinition],
    me: &[u8; NODE_ID_LEN],
    leads_the_store: bool,
) -> Option<Reach> {
    campaigns_for(declared, me).or_else(|| {
        leads_the_store
            .then(|| released(declared).first().copied())
            .flatten()
    })
}

/// Whether this node may stand for the store line, from its own member row.
///
/// The store line's leader writes every table that no placement carves out, so
/// it must hold all of them: a leader never collects, which leaves it with
/// nothing that says it holds less (`served()` is `None`), and a read on it is
/// then answered from whatever it happens to hold as if that were the whole.
/// A row that subscribes this node to something narrower than [`Reach::Store`]
/// — a namespace, a database, one shard — is therefore not a store-line
/// candidate. It still votes, and it still stands for a range its row places
/// ([`stands_for`]).
///
/// The first row bound to `me`, as [`stands_for`] reads it, and a row with no
/// subscription keeps the rule as it was: such a node collects nothing, so
/// whatever it holds is its own.
#[must_use]
pub fn stands_for_the_store(declared: &[ReplicaDefinition], me: &[u8; NODE_ID_LEN]) -> bool {
    declared
        .iter()
        .find(|peer| peer.node.as_ref() == Some(me))
        .and_then(|peer| peer.replicates)
        .is_none_or(|reach| reach == Reach::Store)
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

    /// One renewal round: the epoch held, while nothing says the cluster has
    /// moved past it; otherwise the epoch after everything this node has seen.
    ///
    /// # A renewal keeps its epoch
    ///
    /// A leader renews about every 300 ms (G053 SG2b), and a new epoch per
    /// renewal is a new leadership record per renewal — three a second on an
    /// idle cluster, each one a commit every follower applies, together eating
    /// the retained log in hours. The voter admits the incumbent re-asking its
    /// own epoch, and safety is unchanged: a voter still grants one epoch to one
    /// candidate, so two majorities for one epoch still need a voter that
    /// granted it twice. A fresh epoch is stood for when this node holds none,
    /// when it already stood above what it holds, or when a refusal named a
    /// higher one — each a sign that re-asking the held epoch would be refused.
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
        let held = self.standing.epoch;
        let next = if held > Epoch::ZERO && self.stood == held && self.heard <= held {
            held
        } else {
            Epoch::new(
                held.get()
                    .max(self.stood.get())
                    .max(self.heard.get())
                    .saturating_add(1),
            )
        };
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
    pub(crate) fn stagger(candidate: [u8; NODE_ID_LEN], epoch: Epoch) -> Duration {
        spread(candidate, epoch, tessari_constants::ROUND_MILLIS)
    }
}

/// How long this node goes without hearing a leader before it stands: the
/// lease, plus a spread derived from this node and the last epoch it granted.
///
/// # Why the lease and not less
///
/// A voter refuses everyone but the incumbent until a whole lease after its last
/// grant ([`crate::Voter`]), so standing sooner only spends rounds that cannot
/// win. Standing later than the lease is the whole failover budget going on
/// waiting, which is what the spread is kept small for.
///
/// # Why a spread at all
///
/// Every voter hears the same renewal ballot, so their memories of a dead
/// leader lapse within milliseconds of each other. Two followers standing on
/// the same tick each grant the other the one epoch and both lose it for a whole
/// lease; Raft's randomised election timeout is the liveness argument for
/// exactly this. Derived rather than random for the reason [`Renewing::stagger`]
/// gives — a test can state when a node stands — and with the epoch in the mix
/// so that two ids that collide once do not collide at the next election.
///
/// `lease` is the hold this node's own voter grants for — the installed failover
/// policy's lease (G053 SG2c). A leader's lease is never longer than the hold of
/// a voter that granted it, so a node whose grant has aged past its own hold has
/// outlived any lease that grant could have carried.
#[must_use]
pub fn election_timeout(me: [u8; NODE_ID_LEN], granted: Epoch, lease: Duration) -> Duration {
    lease.saturating_add(spread(
        me,
        granted,
        tessari_constants::ELECTION_JITTER_MILLIS,
    ))
}

/// A duration in `[0, millis)` ms, derived from a node and an epoch.
fn spread(candidate: [u8; NODE_ID_LEN], epoch: Epoch, millis: u64) -> Duration {
    // A 64-bit mix of the id and the epoch. The constants are SplitMix64's;
    // nothing here needs a distribution better than "two different inputs land
    // in different places", and a named mixer is easier to recognise than an
    // invented one.
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
    Duration::from_millis(mixed.rem_euclid(millis.max(1)))
}
