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
#[derive(Debug, Default)]
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
}

#[cfg(test)]
mod tests {
    use super::{Destination, Directory};
    use core::time::Duration;
    use std::time::Instant;
    use tessari_encoding::{NODE_ID_LEN, NodeVersion, Roles};
    use tessari_types::{Epoch, Sequence};

    use crate::peer::Hello;

    const ONE: [u8; NODE_ID_LEN] = [1; NODE_ID_LEN];
    const ANOTHER: [u8; NODE_ID_LEN] = [2; NODE_ID_LEN];

    /// A greeting from `node`, saying its copy is `age` old and that it `serves`.
    fn said(node: [u8; NODE_ID_LEN], age: Option<Duration>, serves: bool) -> Hello {
        Hello {
            node,
            build: NodeVersion {
                major: 0,
                minor: 1,
                patch: 1,
            },
            epoch: Epoch::new(7),
            roles: if serves { Roles::SERVING } else { Roles::NONE },
            tail: Sequence::new(4096),
            current_as_of: age,
        }
    }

    /// A directory holding one serving peer at `two.example:9080`, five seconds
    /// old when it was heard.
    fn one_peer(heard_at: Instant) -> Directory {
        let mut directory = Directory::new();
        directory.heard(
            "two.example:9080",
            said(ANOTHER, Some(Duration::from_secs(5)), true),
            heard_at,
        );
        directory
    }

    #[test]
    fn a_remembered_reading_ages_with_the_clock() {
        // The rule this module exists for. The peer said five seconds; thirty
        // seconds later its copy is thirty-five seconds old, not five. A router
        // that answered five would be measuring staleness with a stale
        // measurement, which is the failure the bound exists to prevent.
        let heard_at = Instant::now();
        let directory = one_peer(heard_at);

        assert_eq!(
            directory.age_of("two.example:9080", heard_at),
            Some(Duration::from_secs(5)),
            "at the instant it was heard, the age is what the peer said"
        );
        assert_eq!(
            directory.age_of(
                "two.example:9080",
                heard_at
                    .checked_add(Duration::from_secs(30))
                    .expect("thirty seconds after an instant this process made")
            ),
            Some(Duration::from_secs(35)),
            "the remembered reading did not age"
        );
        assert_eq!(
            directory.age_of("nobody.example:9080", heard_at),
            None,
            "an address nobody has greeted from reported an age"
        );
    }

    #[test]
    fn a_peer_that_cannot_say_how_old_its_copy_is_is_outside_every_bound() {
        // `None` is a real answer, not an omission, and it gets the same
        // treatment the local `None` already gets: excluded, never marked.
        let heard_at = Instant::now();
        let mut directory = Directory::new();
        directory.heard("two.example:9080", said(ANOTHER, None, true), heard_at);

        assert_eq!(directory.age_of("two.example:9080", heard_at), None);
        assert_eq!(
            directory.read_within(None, Duration::from_secs(86_400), heard_at),
            Destination::Nowhere,
            "a copy of unknown age was admitted by a bound of a whole day, which \
             would make the bound a formality rather than a guarantee"
        );
    }

    #[test]
    fn a_bound_this_node_meets_is_answered_here_and_never_redirected() {
        // Here first: a redirect this node did not need costs the client a round
        // trip and hands it a node it had no reason to learn about. The peer in
        // this directory is FRESHER than we are, and is still not named.
        let heard_at = Instant::now();
        let directory = one_peer(heard_at);

        assert_eq!(
            directory.read_within(
                Some(Duration::from_secs(20)),
                Duration::from_secs(30),
                heard_at
            ),
            Destination::Here,
        );
    }

    #[test]
    fn a_bound_this_node_misses_names_a_peer_that_meets_it() {
        // §C-07 settled that no node proxies, so a redirect must NAME a peer --
        // and it names both halves, because an address alone could not be
        // checked on arrival.
        let heard_at = Instant::now();
        let directory = one_peer(heard_at);

        assert_eq!(
            directory.read_within(
                Some(Duration::from_secs(90)),
                Duration::from_secs(30),
                heard_at
            ),
            Destination::There {
                endpoint: "two.example:9080".to_owned(),
                node: ANOTHER,
            },
        );
    }

    #[test]
    fn a_peer_that_does_not_serve_is_never_named() {
        // A node drained for maintenance still holds data and still greets.
        // Sending a client there is precisely what draining exists to prevent,
        // so its currency is irrelevant -- and here it is the best in the room.
        let heard_at = Instant::now();
        let mut directory = Directory::new();
        directory.heard(
            "drained.example:9080",
            said(ANOTHER, Some(Duration::ZERO), false),
            heard_at,
        );

        assert_eq!(
            directory.read_within(None, Duration::from_secs(30), heard_at),
            Destination::Nowhere,
            "a drained node was offered to a client",
        );
    }

    #[test]
    fn a_bound_no_copy_meets_is_answered_nowhere() {
        // Refused rather than promoted to whoever happens to be freshest. The
        // peer here is only a little outside the bound, which is the case a
        // router would be most tempted to round in its own favour.
        let heard_at = Instant::now();
        let directory = one_peer(heard_at);

        assert_eq!(
            directory.read_within(
                Some(Duration::from_secs(600)),
                Duration::from_secs(4),
                heard_at
            ),
            Destination::Nowhere,
        );
    }

    #[test]
    fn the_freshest_peer_within_the_bound_is_the_one_named() {
        // Deterministic, so the choice is assertable, and the one most likely
        // still inside the bound when the client arrives -- the reading goes on
        // ageing while the client travels. The older peer is named FIRST in the
        // ordered map, so a selection that simply took the first match would
        // pass every other test in this module and fail this one.
        let heard_at = Instant::now();
        let mut directory = Directory::new();
        directory.heard(
            "a-older.example:9080",
            said(ONE, Some(Duration::from_secs(25)), true),
            heard_at,
        );
        directory.heard(
            "b-fresher.example:9080",
            said(ANOTHER, Some(Duration::from_secs(2)), true),
            heard_at,
        );

        assert_eq!(
            directory.read_within(None, Duration::from_secs(30), heard_at),
            Destination::There {
                endpoint: "b-fresher.example:9080".to_owned(),
                node: ANOTHER,
            },
        );
    }
}
