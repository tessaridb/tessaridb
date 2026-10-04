//! What this node knows about the others, and where a bounded read should go.
//!
//! # The one thing this module exists to get right
//!
//! A staleness bound asks *how old may the copy be*. A router answering it from
//! a remembered greeting is holding a measurement that is itself ageing, and a
//! router that forgot to account for that would answer a bound about the past
//! with a reading from further in the past still.
//!
//! So the rule here is one sentence: **a peer's age is what it said plus how
//! long ago it said it.** The compounding is saturating and can only ever report
//! a copy *older* — which is the direction the whole feature errs in, because
//! refusing a read that was marginally admissible is recoverable and admitting
//! one that was not is the thing the bound was written to stop.
//!
//! # Why the key is an address and not a node id
//!
//! A declared peer is a catalog row carrying a name, an endpoint and a role set,
//! and that name is deliberately **not** the node's own identifier — the catalog
//! says so itself: those are sixteen unpredictable bytes a node gives *itself*,
//! and nothing else can know them before the two have spoken. A greeting, on the
//! other hand, is keyed by exactly that identifier.
//!
//! The only key the two halves share is therefore **the address that was
//! dialled**. The declaration names a place, the greeting names a node, and the
//! place is the half a router can act on: it is what a redirect has to hand the
//! client. The node id travels along inside the greeting so the redirect can
//! also say *who* is there, which is what makes it checkable on arrival.
//!
//! # What a silent peer is worth
//!
//! A greeting can fail, and when it does the remembered reading is left alone to
//! go on ageing. It is **not** erased, and the two behaviours differ exactly
//! where it matters. An ageing reading drifts out of tighter bounds first and
//! looser ones later, and comes back the moment the peer answers — a transient
//! fault degrades routing in proportion to how long it lasted. An erased one
//! makes the peer *no known age*, which is outside **every** bound, so a single
//! dropped packet would take a healthy node out of all routing at once and keep
//! it out until a greeting got through.
//!
//! Erasing also fails in the direction that looks like health: a smaller
//! directory in which every entry is fresh. So: **a silent peer grows old, it
//! does not vanish.**
//!
//! # Where the clock comes from
//!
//! Every method that needs *now* takes it. This is the same shape W232 arrived at
//! for the collector's cursor, and for the same reason: the thing that decides
//! *when* belongs beside the decision, where a test can hold it still. A module
//! that read the clock itself would be one whose ageing rule could only be tested
//! by waiting.

use core::time::Duration;
use std::collections::BTreeMap;
use std::time::Instant;

use tessari_encoding::{NODE_ID_LEN, Roles};
use tessari_storage::ReplicaDefinition;
use tessari_types::{Epoch, Reach};

use crate::joining::Seed;
use crate::peer::Hello;

/// One greeting, and the instant it arrived.
///
/// The instant is half the record. A greeting without it is a claim with no
/// date, and a claim about staleness with no date is the one kind of claim this
/// module may not keep.
#[derive(Debug, Clone)]
pub struct Heard {
    /// What the peer said about itself.
    pub said: Hello,
    /// When this node heard it.
    pub at: Instant,
}

/// Where a read carrying a staleness bound should be answered.
///
/// Three answers and not an `Option`, because *answer it here* and *go to that
/// node* are both successes and mean opposite things to the caller, while *no
/// copy is within the bound* is neither. Collapsing any two of them would make a
/// router that cannot serve the read indistinguishable from one that can.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Destination {
    /// This node's own copy is within the bound. Nothing is redirected.
    Here,
    /// A peer's copy is within the bound, and this is which one.
    ///
    /// Both halves are carried because a redirect that named only an address
    /// could not be checked on arrival: a client that dialled it and met a
    /// different node would have no way to notice.
    There {
        /// The address to dial — the same string the declaration carried.
        endpoint: String,
        /// Who was last heard there.
        node: [u8; NODE_ID_LEN],
    },
    /// No copy this node knows of is within the bound.
    ///
    /// The read is refused rather than promoted to whoever is freshest. §C-05
    /// excludes, it never marks, and §C-07 settled that no node proxies — so
    /// there is no third thing a router could do with it.
    Nowhere,
}

/// Every peer this node has heard from, and when.
///
/// Keyed by endpoint; see the module header for why that and not the node id.
/// A second greeting from the same address replaces the first, because the point
/// of the record is the *latest* thing that address said.
#[derive(Debug, Default, Clone)]
pub struct Directory {
    seen: BTreeMap<String, Heard>,
}

impl Directory {
    /// An empty directory — this node knows of nobody yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record what was heard at `endpoint`, at the instant `at`.
    pub fn heard(&mut self, endpoint: &str, said: Hello, at: Instant) {
        self.seen.insert(endpoint.to_owned(), Heard { said, at });
    }

    /// What was last heard at `endpoint`, unaged.
    ///
    /// The raw record, for a caller that wants the tail, the epoch or the roles
    /// rather than the currency. Anything asking *how old is that copy* uses
    /// [`Self::age_of`], which is the same fact with the elapsed time added.
    #[must_use]
    pub fn at(&self, endpoint: &str) -> Option<&Heard> {
        self.seen.get(endpoint)
    }

    /// How old the copy at `endpoint` is now: what it said plus how long ago.
    ///
    /// `None` covers two cases that are one answer here — this node has never
    /// heard from that address, and the peer there could not say how old its own
    /// copy was. Both mean *no known age*, and a copy of no known age is outside
    /// every bound rather than inside the ones nobody measured.
    #[must_use]
    pub fn age_of(&self, endpoint: &str, now: Instant) -> Option<Duration> {
        let heard = self.seen.get(endpoint)?;
        let stated = heard.said.current_as_of?;
        Some(stated.saturating_add(now.saturating_duration_since(heard.at)))
    }

    /// Where a read bounded by `bound` should go, given this node's own age.
    ///
    /// `mine` is this node's own answer from `Store::current_as_of` — `None`
    /// when it cannot say, which excludes it exactly as it excludes a peer.
    ///
    /// The order is not arbitrary. **Here first**: when this node satisfies the
    /// bound, a redirect would cost the client a round trip and teach it about a
    /// node it had no reason to learn. Then the **freshest** qualifying peer,
    /// which is deterministic — so the choice is assertable — and is the peer
    /// most likely still inside the bound by the time the client arrives, since
    /// the reading goes on ageing while the client travels. Equal ages go to the
    /// lexicographically first endpoint, because the map is ordered and a tie
    /// still has to resolve the same way twice.
    ///
    /// A peer must carry [`Roles::SERVING`]. A node drained to no roles at all
    /// still holds data and still greets, and sending a client there is
    /// precisely what draining exists to prevent.
    #[must_use]
    pub fn read_within(
        &self,
        mine: Option<Duration>,
        bound: Duration,
        now: Instant,
    ) -> Destination {
        if mine.is_some_and(|age| age <= bound) {
            return Destination::Here;
        }
        let freshest = self
            .seen
            .iter()
            .filter(|(_, heard)| heard.said.roles.has(Roles::SERVING))
            .filter_map(|(endpoint, heard)| {
                let age = self.age_of(endpoint, now)?;
                (age <= bound).then_some((endpoint, heard, age))
            })
            .min_by_key(|(_, _, age)| *age);
        match freshest {
            Some((endpoint, heard, _)) => Destination::There {
                endpoint: endpoint.clone(),
                node: heard.said.node,
            },
            None => Destination::Nowhere,
        }
    }

    /// A peer that says it may write, and which one.
    ///
    /// The other axis. [`Self::read_within`] answers *how old may the copy be*;
    /// this answers *who decides writes*, and the two are not the same question
    /// wearing different units — a follower at zero lag is level, not
    /// authoritative, because being level a moment ago says nothing about a
    /// write committing right now.
    ///
    /// # No age, no bound, and no `Here`
    ///
    /// Nothing here ages, because leadership is not a measurement that decays
    /// into being slightly wrong: a peer either claimed `WRITABLE` when it last
    /// spoke or it did not. A stale claim is handled where it lands — the
    /// redirect carries the epoch that peer published, so a node that has since
    /// lost the leadership refuses the client and names the newer one, which is
    /// a check the arriving node can make and this one cannot.
    ///
    /// There is no *here* answer either. A node reads its own roles out of its
    /// own store, so it never needs a directory to tell it whether it leads, and
    /// the caller asks this only once that has already come back no.
    ///
    /// # Why the highest epoch wins
    ///
    /// Two peers claiming `WRITABLE` at once is not a malformed directory — it
    /// is precisely what a leadership handover looks like from the outside while
    /// one greeting is newer than the other. The higher epoch is the later
    /// claim, so it is the one to send a client to; a tie goes to the
    /// lexicographically first endpoint, because the map is ordered and the same
    /// question has to resolve the same way twice.
    ///
    /// Both halves travel for [`Self::read_within`]'s reason: a client sent to
    /// an address alone cannot notice that it met a different node than the one
    /// it was promised.
    #[must_use]
    pub fn writable(&self) -> Option<(String, [u8; NODE_ID_LEN])> {
        self.seen
            .iter()
            .filter(|(_, heard)| heard.said.roles.has(Roles::WRITABLE))
            .max_by(|(left, one), (right, other)| {
                one.said
                    .epoch
                    .cmp(&other.said.epoch)
                    .then_with(|| right.cmp(left))
            })
            .map(|(endpoint, heard)| (endpoint.clone(), heard.said.node))
    }

    /// The peer heard leading the placed range `range`'s line, at the newest
    /// epoch heard for it — a lapsed line is not a leader anybody can hear.
    #[must_use]
    pub fn leading(&self, range: Reach) -> Option<(String, [u8; NODE_ID_LEN], Epoch)> {
        self.seen
            .iter()
            .filter_map(|(endpoint, heard)| {
                heard
                    .said
                    .line
                    .filter(|line| line.range == range && line.leading > Epoch::ZERO)
                    .map(|line| (endpoint.clone(), heard.said.node, line.leading))
            })
            .max_by_key(|(_, _, leading)| *leading)
    }

    /// Greet every declared peer this node can dial, and record what each said.
    ///
    /// Answers how many peers were reached. Not a `Result`: a round in which
    /// every peer failed is a cluster in trouble, which is a different statement
    /// from an operation that went wrong, and the caller is a timer that has to
    /// run again either way.
    ///
    /// # What is skipped, and why each is skipped rather than refused
    ///
    /// A row whose `node` is `None` is **declared but undiallable**. Opening a
    /// session derives the peer's TLS name from its generated identifier, so
    /// there is no way to reach a peer whose identifier nobody has written down
    /// — and this round cannot invent one. Failing the whole pass over it would
    /// let one incomplete declaration disable routing for every other peer.
    ///
    /// This node's **own** row is skipped too. Its currency is already known
    /// directly, so dialling itself would be a round trip to learn what the
    /// store answers for free, and it would put this node in its own directory,
    /// where [`Self::read_within`]'s *here first* rule has already decided it
    /// does not belong.
    ///
    /// # A failure is not recorded
    ///
    /// When `greet` fails, nothing is written. The reading already held for that
    /// endpoint stays where it is and goes on ageing — see the module header for
    /// why that is the safe direction and erasing is not. One failure does not
    /// end the round, because the peer after the failing one is the one most
    /// likely to still be serving.
    pub fn greet_round<G, E>(
        &mut self,
        declared: &[ReplicaDefinition],
        me: &[u8; NODE_ID_LEN],
        now: Instant,
        greet: G,
    ) -> usize
    where
        G: Fn(&str, [u8; NODE_ID_LEN]) -> Result<Hello, E>,
    {
        let mut reached = 0_usize;
        for replica in declared {
            let Some(node) = replica.node else { continue };
            if node == *me {
                continue;
            }
            if let Ok(said) = greet(&replica.endpoint, node) {
                self.heard(&replica.endpoint, said, now);
                reached = reached.saturating_add(1);
            }
        }
        reached
    }

    /// Greet the seeds, for a node whose catalog names nobody yet.
    ///
    /// Answers how many were reached, the same as [`Self::greet_round`] and for
    /// the same reason.
    ///
    /// # This is the bootstrap round and only the bootstrap round
    ///
    /// A node that has just been told to join holds an **empty** catalog: it has
    /// no `ReplicaDefinition` rows, so [`Self::greet_round`] dials nobody and
    /// [`crate::upstream`] chooses from nothing. The circle is real — the
    /// membership lives in the catalog, and the catalog arrives by collecting
    /// from a member — and the seed is the single pointer that breaks it.
    ///
    /// The caller runs this **instead of** [`Self::greet_round`] while the
    /// catalog is empty, and never alongside it. That is not a saving, it is the
    /// rule: `DEFINE REPLICA` is a catalog write and therefore already a log
    /// record, so the moment collection brings the membership in, the catalog is
    /// the answer and a seed still being dialled would be a second source of
    /// truth about who the members are. This session shipped four decisions
    /// (ADR-0063 to ADR-0066) whose common subject was exactly that failure —
    /// configuration answering a question the protocol had already answered —
    /// and a permanently-consulted seed would be a fifth.
    ///
    /// # A seed is not skipped for want of an id
    ///
    /// The one row [`Self::greet_round`] cannot dial is one whose `node` is
    /// `None`. A [`Seed`] has no such state: it does not parse without an id
    /// (ADR-0067), so every seed this node holds is diallable by construction.
    /// Its own id is still skipped, for [`Self::greet_round`]'s reason — an
    /// operator who seeds a node with itself has written a loop, and dialling it
    /// would put this node in its own directory.
    pub fn greet_seeds<G, E>(
        &mut self,
        seeds: &[Seed],
        me: &[u8; NODE_ID_LEN],
        now: Instant,
        greet: G,
    ) -> usize
    where
        G: Fn(&str, [u8; NODE_ID_LEN]) -> Result<Hello, E>,
    {
        let mut reached = 0_usize;
        for seed in seeds {
            if seed.node == *me {
                continue;
            }
            if let Ok(said) = greet(&seed.endpoint, seed.node) {
                self.heard(&seed.endpoint, said, now);
                reached = reached.saturating_add(1);
            }
        }
        reached
    }
}

#[cfg(test)]
mod tests;
