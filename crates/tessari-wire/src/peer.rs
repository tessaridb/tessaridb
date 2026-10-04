//! What one node must prove to another before either believes a word of it.
//!
//! # The certificate says who; the frame says what you have
//!
//! Two nodes meeting have two different questions to settle, and the design
//! keeps them apart on purpose. *Who are you* is answered by the transport,
//! which has already checked a credential neither end can forge. *What do you
//! hold* — which epoch, which roles, how far the log reaches — is answered by
//! the [`Hello`] frame, because none of it is in a credential and none of it
//! should be: a certificate outlives every one of those facts.
//!
//! The `Hello` carries a node id anyway, and it is worth being clear about why,
//! because on its face that looks like the frame answering the first question
//! after all. It is the opposite. The field exists **to be checked against** the
//! credential and for no other purpose: without it the two questions could never
//! be observed to disagree. A node that could name any id it liked in the frame
//! would make the credential decorative — the same failure shape as a membership
//! row that names a node by an operator-chosen name rather than by its own id.
//!
//! # Why this knows nothing about how the identity was proved
//!
//! [`Presented`] is whatever the transport vouches for. Mutual TLS produces it
//! from a verified certificate; a Noise handshake would produce it from a
//! verified static key. These rules never learn which, and that is deliberate:
//! the transport is an open decision, and a rule set that could only be written
//! after it was taken would have to be written twice.
//!
//! # Why these tags are their own space
//!
//! The client protocol's kinds are 1 to 5 and its reader closes the connection
//! on anything else. The peer protocol's kinds start at 6 in a **separate**
//! enum, sharing the length-prefixing and the store's value encoding and nothing
//! else, because the two links differ in every property that matters — who
//! authenticates, by what, against what exposure, carrying whose data. The
//! decisive one is that a peer which cannot prove itself never reaches the
//! framing at all, which is unrepresentable on a port that admits anonymous
//! clients.

mod hello;

use core::time::Duration;

use tessari_encoding::{NODE_ID_LEN, NodeIdentity, NodeVersion, Roles};
use tessari_storage::FailoverStamp;
use tessari_types::{Epoch, Reach, RecordId, Sequence};

use crate::error::{Error, Result};
use crate::frame;

/// What a frame on the peer link is.
///
/// Numbered explicitly and never renumbered, for the reason the client's table
/// is: a byte that once meant one thing and later means another cannot be asked
/// about after the fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PeerFrame {
    /// Who I am and what I hold, sent by both ends.
    Hello,
    /// A candidate asking for one epoch.
    Ballot,
    /// A voting member's answer to one ballot.
    Vote,
    /// A follower asking for the records after the position it holds.
    Collect,
    /// A leader's answer: the records, and the leadership before them.
    Collected,
    /// A leader saying it cannot state what precedes the position asked for.
    ///
    /// Its own tag rather than an empty [`Self::Collected`], because an empty
    /// collection is the answer a follower that is **level** gets. Two
    /// conditions that must never look alike do not share a frame.
    Uncollectable,
    /// A leader saying this node is subscribed to nothing.
    ///
    /// Its own tag for the same reason [`Self::Uncollectable`] has one, one step
    /// further out: *you may not ask*, *I cannot state what precedes you* and
    /// *you are level* are three conditions with three different repairs — a
    /// `DEFINE REPLICA`, a re-bootstrap, and nothing at all. Sharing a frame
    /// would send an operator to the wrong one of the three, and closing the
    /// socket instead would send them to a packet capture.
    Unsubscribed,
    /// A node asking a shard's leader for one page of that shard's records
    /// (G033, ADR-0083).
    Gather,
    /// The leader's answer: the page.
    Gathered,
    /// The leader declining, with the reason — its own tag for the reason
    /// [`Self::Unsubscribed`] has one.
    NotGathered,
    /// A follower asking for its leader's state, because the log it would
    /// collect from has been pruned past it (ADR-0094 D3).
    State,
    /// The first frame of a copy: the reach, the version, each log's position.
    StateHead,
    /// One chunk of the copied state, in the store's own log-record encoding.
    StateChunk,
    /// The last frame of a copy: the counts and the topic heads.
    StateEnd,
    /// A follower holding the connection open to be SENT what its leader
    /// commits, naming every log it follows and the first position it does not
    /// hold in each (ADR-0106 D5). Sent again after each round it applied, so
    /// the positions it names are the ones it has made durable.
    Stream,
    /// The leader's round on a held stream: one answer per log asked, sent the
    /// moment a commit gives it something to send, or empty as a heartbeat.
    Streamed,
    /// A node asking another to answer a request it cannot, for a caller it
    /// verified, under a signed assertion (ADR-0108 D1–D3).
    Coordinate,
    /// The answer, in the caller's surface's shape.
    Coordinated,
    /// The answering node declining, with its reason — its own tag for the
    /// reason [`Self::Unsubscribed`] has one.
    NotCoordinated,
    /// A node asking the store line's leader about a sign-in try, or telling
    /// it how one went (ADR-0108 D5).
    Attempt,
    /// The leader's answer: whether the name may try now.
    Attempted,
    /// A node asking to be bound to the row a join token waits on
    /// (ADR-0108 D9).
    Join,
    /// The answer: whether a row is now bound to the asker.
    Joined,
    /// A node asking a range's leader to write one record of a transaction
    /// across leaders — prepare, decide or resolve — for a caller it verified,
    /// under a signed assertion (ADR-0108, ADR-0112).
    Across,
    /// What the leader wrote, and where.
    AcrossDone,
    /// The leader declining, with its reason.
    NotAcross,
}

impl PeerFrame {
    /// The tag written on the wire.
    ///
    /// Out of the same byte the client's frames are tagged from, and **the rule
    /// is disjointness, not a range**: no tag is claimed by both protocols, so a
    /// peer frame arriving on the client port is closed rather than misread, and
    /// a client frame arriving here is too.
    ///
    /// These seven took 6-12 because 1-5 was what the client had, which made
    /// *"six and upward is the peer's"* an exact description of the arrangement
    /// — and a false sentence the moment a client kind took 13
    /// ([`crate::frame::Kind::Elsewhere`]). The property never moved. A
    /// contiguous range is a convenient way to say *disjoint* and is never the
    /// thing itself, which is why `no_peer_tag_is_a_client_tag` asserts the
    /// property and no longer asserts the shape.
    #[must_use]
    pub const fn tag(self) -> u8 {
        match self {
            Self::Hello => 6,
            Self::Ballot => 7,
            Self::Vote => 8,
            Self::Collect => 9,
            Self::Collected => 10,
            Self::Uncollectable => 11,
            Self::Unsubscribed => 12,
            // 13 is the client's `Elsewhere`; disjointness is the rule.
            Self::Gather => 14,
            Self::Gathered => 15,
            Self::NotGathered => 16,
            // 17 is the client's `Vault`.
            Self::State => 18,
            Self::StateHead => 19,
            Self::StateChunk => 20,
            Self::StateEnd => 21,
            Self::Stream => 22,
            Self::Streamed => 23,
            Self::Coordinate => 24,
            Self::Coordinated => 25,
            Self::NotCoordinated => 26,
            Self::Attempt => 27,
            Self::Attempted => 28,
            Self::Join => 29,
            Self::Joined => 30,
            Self::Across => 31,
            Self::AcrossDone => 32,
            Self::NotAcross => 33,
        }
    }

    /// Recover a kind from its tag.
    #[must_use]
    pub const fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            6 => Some(Self::Hello),
            7 => Some(Self::Ballot),
            8 => Some(Self::Vote),
            9 => Some(Self::Collect),
            10 => Some(Self::Collected),
            11 => Some(Self::Uncollectable),
            12 => Some(Self::Unsubscribed),
            14 => Some(Self::Gather),
            15 => Some(Self::Gathered),
            16 => Some(Self::NotGathered),
            18 => Some(Self::State),
            19 => Some(Self::StateHead),
            20 => Some(Self::StateChunk),
            21 => Some(Self::StateEnd),
            22 => Some(Self::Stream),
            23 => Some(Self::Streamed),
            24 => Some(Self::Coordinate),
            25 => Some(Self::Coordinated),
            26 => Some(Self::NotCoordinated),
            27 => Some(Self::Attempt),
            28 => Some(Self::Attempted),
            29 => Some(Self::Join),
            30 => Some(Self::Joined),
            31 => Some(Self::Across),
            32 => Some(Self::AcrossDone),
            33 => Some(Self::NotAcross),
            _ => None,
        }
    }
}

/// What a credential was issued for.
///
/// A credential is not only an identity, it is an identity **for something**. A
/// client's credential presented on the peer link is a real event with a real
/// cause — a mis-issue, or a copied file — and it is refused on this field
/// rather than on the id, because the id may be perfectly correct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Purpose {
    /// Issued for one node to reach another.
    Peer,
    /// Issued for a client to reach a node.
    Client,
}

/// What the transport proved about the other end.
///
/// Produced by whichever transport the peer link ends up using; these rules
/// treat it as given and never ask how it was established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Presented {
    /// The node id the credential names.
    pub node: [u8; NODE_ID_LEN],
    /// What that credential was issued for.
    pub purpose: Purpose,
}

/// What a node says about itself when the link opens.
///
/// Every field but the id is a claim about **state**, which is why none of it
/// belongs in a credential: a node's epoch, roles and log tail change while the
/// credential does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hello {
    /// The id this node claims — present so that a disagreement with the
    /// credential is detectable, and for nothing else.
    pub node: [u8; NODE_ID_LEN],
    /// The build it is running.
    pub build: NodeVersion,
    /// The leadership it believes is current.
    pub epoch: Epoch,
    /// What it is for.
    pub roles: Roles,
    /// How far its committed log reaches.
    pub tail: Sequence,
    /// The leadership under which the record at that tail was written.
    ///
    /// Beside the tail and not instead of it, because the two answer one
    /// question together: *which of these two logs is further along*. The
    /// sequence alone cannot, since a node that led an old epoch and diverged
    /// can hold a higher sequence than a node holding the newer history — the
    /// ordering is ADR-0059's, a higher epoch first and the higher sequence at
    /// equal epoch.
    ///
    /// It is deliberately NOT [`Self::epoch`]. That one is the leadership the
    /// greeter believes is current, which is `Epoch::ZERO` on a node that has
    /// never campaigned however much history it holds; this one is the
    /// leadership that actually wrote what it holds. A voter comparing the wrong
    /// one of the two would rank a complete follower below an outdated
    /// ex-leader, which is precisely the ordering an election restriction
    /// exists to prevent.
    pub tail_leadership: Epoch,
    /// How old the greeter says its own copy is.
    ///
    /// The fact a router needs beside the tail and the roles: the tail says how
    /// much this node holds, and this says how long ago that stopped being the
    /// whole story. It is the greeter's own answer from
    /// `Store::current_as_of` — `Some(0)` on a node that may write, because it
    /// is the origin of what it holds, and the time since it was last *level*
    /// on one that may not.
    ///
    /// `None` is a real answer and not an omission: a copy that has collected
    /// and never arrived has no known age, and a node that cannot say how old
    /// its copy is must be treated as outside every bound rather than inside
    /// the ones nobody measured.
    ///
    /// Whole seconds on the wire, rounded **up**. A rounding that can only
    /// report a copy older is the direction a staleness bound already errs in,
    /// so the loss of precision can refuse a read and can never admit one.
    pub current_as_of: Option<Duration>,
    /// Which failover policy the greeter is running under, without the policy.
    ///
    /// The pair and never the five periods. The policy itself is a row in the
    /// system tenancy, so it is already a log record and already reaches every
    /// node through the apply path every other record takes; putting the periods
    /// here too would be a second spelling of one configuration, on a second
    /// route, and this engine has already paid for that twice. What the log
    /// cannot give is the ordering *before* the apply — a node has no way to
    /// know it is behind until the record it is behind on arrives — and that is
    /// exactly what this field is for.
    ///
    /// `None` is a real answer with two causes that need no distinguishing: the
    /// greeter holds no policy row and runs `Failover::DEFAULT`, or the greeter
    /// is an older build whose greeting ends before this field. Both mean *this
    /// node said nothing about a policy*, and any stamp supersedes both.
    pub policy: Option<FailoverStamp>,
    /// The placed range this greeter stands for, and where it stands there
    /// (ADR-0082).
    ///
    /// One and not a list: a node reads one member row — the first bound to its
    /// id, the rule `desired_roles` already applies — and a row places one
    /// range. `None` for a node with no placement, which writes nothing here and
    /// so greets byte-identically to every build before this field.
    pub line: Option<Line>,
}

/// Where a greeter stands on the one placed range it stands for.
///
/// Everything a voter and a candidate need about that range's line and nothing
/// else: whether the greeter leads it now, and how far its own log of the range
/// reaches, as the pair that orders two logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Line {
    /// The placed range.
    pub range: Reach,
    /// The epoch of a LIVE lease on this range's line, and [`Epoch::ZERO`] when
    /// the greeter holds none — a lapsed line is not a leader anybody can hear.
    pub leading: Epoch,
    /// How far the greeter's own log of this range reaches.
    pub tail: Sequence,
    /// The leadership under which the record at that tail was written.
    pub tail_leadership: Epoch,
}

impl Line {
    /// This line's position as the pair a voter orders two logs by.
    #[must_use]
    pub const fn reached(&self) -> crate::grant::Reached {
        crate::grant::Reached {
            leadership: self.tail_leadership,
            tail: self.tail,
        }
    }
}

/// Decide whether the other end may speak on this link.
///
/// The four refusals are four different failures and are kept apart, because
/// one "handshake failed" sends whoever reads it to a packet capture for each:
/// nothing was proven at all means the port let a stranger through; the wrong
/// purpose means a credential was presented where it was never meant to be; a
/// disagreeing id means either a mis-issued credential or a node claiming
/// somebody else's name; and this node's own id means a collision only this end
/// can see.
///
/// Absence is refused rather than defaulted. A handshake that admitted an
/// unproven peer would make every check after it a formality performed on
/// whoever asked.
///
/// # Why `me` is an argument and not read from the greeting this node sends
///
/// The door builds its own `Hello` **after** this function, deliberately: the
/// epoch, the log tail and the copy's age are facts about state, and a door idle
/// for an hour would otherwise state hour-old ones. An identity is not that kind
/// of fact. It is fixed when the store is initialised and cannot go stale, so
/// taking it early costs nothing and moving the greeting early would cost the
/// property that ordering was chosen for.
///
/// # Errors
///
/// Returns [`Error::Unidentified`], [`Error::NotAPeerCredential`],
/// [`Error::IdentityDisagrees`] or [`Error::ClaimsOurOwnIdentity`], one per
/// refusal above.
pub fn admit(presented: Option<&Presented>, said: &Hello, me: &[u8; NODE_ID_LEN]) -> Result<()> {
    let presented = presented.ok_or(Error::Unidentified)?;
    if presented.purpose != Purpose::Peer {
        return Err(Error::NotAPeerCredential);
    }
    if presented.node != said.node {
        return Err(Error::IdentityDisagrees {
            said: RecordId::Uuid(said.node).to_string(),
            presented: RecordId::Uuid(presented.node).to_string(),
        });
    }
    // Last, and after the credential rather than before it. A caller with no
    // credential at all gets the refusal that names the real problem, and only a
    // peer holding a credential this cluster issued for this node's own name —
    // which is to say a mis-issue — reaches this line. That is the case the
    // second layer exists for; putting the check first would make it the answer
    // to every unauthenticated connection that guessed an id.
    if said.node == *me {
        return Err(Error::ClaimsOurOwnIdentity {
            said: RecordId::Uuid(said.node).to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests;
