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

use core::time::Duration;

use tessari_encoding::{NODE_ID_LEN, NodeIdentity, NodeVersion, Roles};
use tessari_types::{Epoch, RecordId, Sequence};

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
}

impl Hello {
    /// This node's own greeting, built from its own stored identity.
    ///
    /// Taken from [`NodeIdentity`] rather than assembled field by field so that
    /// a node cannot greet under an id, a role set or a build that disagree with
    /// what it actually holds. The two arguments are the two facts an identity
    /// deliberately does not carry, because both change with every commit.
    #[must_use]
    pub fn about(
        identity: &NodeIdentity,
        epoch: Epoch,
        tail: Sequence,
        tail_leadership: Epoch,
        current_as_of: Option<Duration>,
    ) -> Self {
        Self {
            node: identity.id,
            build: identity.version,
            epoch,
            roles: identity.roles,
            tail,
            tail_leadership,
            current_as_of,
        }
    }

    /// Where this greeter's log has got to, as the pair that orders two logs.
    ///
    /// The two fields travel together in every comparison, so they are paired
    /// here rather than at each call site — a caller that assembled them itself
    /// could pair [`Self::tail`] with [`Self::epoch`], which is the one mistake
    /// that inverts the answer.
    #[must_use]
    pub fn reached(&self) -> crate::grant::Reached {
        crate::grant::Reached {
            leadership: self.tail_leadership,
            tail: self.tail,
        }
    }

    /// The body of a [`PeerFrame::Hello`] frame.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut body = Vec::with_capacity(53);
        body.extend_from_slice(&self.node);
        frame::put_u32(&mut body, self.build.major);
        frame::put_u32(&mut body, self.build.minor);
        frame::put_u32(&mut body, self.build.patch);
        frame::put_u64(&mut body, self.epoch.get());
        body.push(self.roles.bits());
        frame::put_u64(&mut body, self.tail.get());
        frame::put_u64(&mut body, self.tail_leadership.get());
        // A presence byte and then the seconds, rather than a sentinel value:
        // every `u64` is a legitimate age, so there is no number left over to
        // mean *I cannot say*.
        match self.current_as_of {
            Some(age) => {
                body.push(1);
                frame::put_u64(&mut body, whole_seconds(age));
            }
            None => {
                body.push(0);
                frame::put_u64(&mut body, 0);
            }
        }
        body
    }

    /// Read one back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Malformed`] when the body is not the shape a greeting
    /// takes, and [`Error::UnknownRoles`] when it is the right shape but names a
    /// role this build does not have — which is a newer peer rather than a
    /// broken one, and is worth saying so.
    pub fn decode(body: &[u8]) -> Result<Self> {
        let mut node = [0_u8; NODE_ID_LEN];
        let head = body.get(..NODE_ID_LEN).ok_or(Error::Malformed)?;
        node.copy_from_slice(head);

        let (major, at) = frame::take_u32(body, NODE_ID_LEN)?;
        let (minor, at) = frame::take_u32(body, at)?;
        let (patch, at) = frame::take_u32(body, at)?;
        let (epoch, at) = frame::take_u64(body, at)?;
        let bits = *body.get(at).ok_or(Error::Malformed)?;
        let roles = Roles::from_bits(bits).ok_or(Error::UnknownRoles { bits })?;
        let (tail, at) = frame::take_u64(body, at.checked_add(1).ok_or(Error::Malformed)?)?;
        let (tail_leadership, at) = frame::take_u64(body, at)?;
        let present = *body.get(at).ok_or(Error::Malformed)?;
        let (seconds, _) = frame::take_u64(body, at.checked_add(1).ok_or(Error::Malformed)?)?;

        Ok(Self {
            node,
            build: NodeVersion {
                major,
                minor,
                patch,
            },
            epoch: Epoch::new(epoch),
            roles,
            tail: Sequence::new(tail),
            tail_leadership: Epoch::new(tail_leadership),
            current_as_of: (present != 0).then(|| Duration::from_secs(seconds)),
        })
    }
}

/// An age in whole seconds, rounded up.
///
/// Up rather than down, and the direction is the point: a bound admits a copy
/// no older than it says, so reporting a fraction of a second as a whole one can
/// only put a copy *outside* a bound it was marginally inside. Refusing a read
/// that was borderline is recoverable; admitting one that was not is the thing
/// the bound exists to stop.
fn whole_seconds(age: Duration) -> u64 {
    age.as_secs()
        .saturating_add(u64::from(age.subsec_nanos() > 0))
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
mod tests {
    use super::{Hello, PeerFrame, Presented, Purpose, admit};
    use crate::error::Error;
    use crate::frame;
    use core::time::Duration;
    use tessari_encoding::{NODE_ID_LEN, NodeIdentity, NodeVersion, Roles};
    use tessari_types::{Epoch, Sequence};

    const ONE: [u8; NODE_ID_LEN] = [1; NODE_ID_LEN];
    const ANOTHER: [u8; NODE_ID_LEN] = [2; NODE_ID_LEN];
    /// The node doing the admitting, distinct from both peers above so that the
    /// fourth refusal cannot fire by accident in a test about the other three.
    const ME: [u8; NODE_ID_LEN] = [3; NODE_ID_LEN];

    fn greeting(node: [u8; NODE_ID_LEN]) -> Hello {
        Hello {
            node,
            build: NodeVersion {
                major: 0,
                minor: 1,
                patch: 1,
            },
            epoch: Epoch::new(7),
            roles: Roles::ALONE,
            tail: Sequence::new(4096),
            tail_leadership: Epoch::new(7),
            current_as_of: Some(Duration::from_secs(3)),
        }
    }

    #[test]
    fn no_peer_tag_is_a_client_tag() {
        // The whole reason the peer protocol gets its own enum: the two spaces
        // must not overlap, and a comment saying so is not a check.
        for tag in 0..=u8::MAX {
            let peer = PeerFrame::from_tag(tag).is_some();
            let client = frame::Kind::from_tag(tag).is_some();
            assert!(
                !(peer && client),
                "tag {tag} is claimed by both the peer and the client protocol"
            );
        }
        // The safety property stated directly rather than inferred from the
        // sweep: every peer tag must be one the client's reader refuses, because
        // that refusal is what closes a misdirected connection instead of
        // decoding it as something else.
        for kind in [
            PeerFrame::Hello,
            PeerFrame::Ballot,
            PeerFrame::Vote,
            PeerFrame::Collect,
            PeerFrame::Collected,
            PeerFrame::Uncollectable,
            PeerFrame::Unsubscribed,
        ] {
            assert!(
                frame::Kind::from_tag(kind.tag()).is_none(),
                "the client reader accepts {kind:?}, which it must close on"
            );
        }
    }

    #[test]
    fn a_greeting_survives_the_wire_unchanged() {
        let said = greeting(ONE);
        let heard = Hello::decode(&said.encode()).expect("a greeting this build wrote");
        assert_eq!(heard, said);
    }

    #[test]
    fn a_greeting_carries_how_old_its_own_copy_is() {
        // The fact a router needs, and the one a greeting did not carry until
        // this wave. Three readings, because they are three different claims: a
        // copy of a known age, a copy whose age nobody can state, and a fraction
        // of a second -- which must come back as the whole second ABOVE it, so
        // the rounding can only refuse a borderline read and never admit one.
        let mut said = greeting(ONE);

        said.current_as_of = Some(Duration::from_secs(41));
        let heard = Hello::decode(&said.encode()).expect("a greeting this build wrote");
        assert_eq!(heard.current_as_of, Some(Duration::from_secs(41)));

        said.current_as_of = None;
        let heard = Hello::decode(&said.encode()).expect("a greeting this build wrote");
        assert_eq!(
            heard.current_as_of, None,
            "a node that cannot say how old its copy is came back claiming an age"
        );

        said.current_as_of = Some(Duration::from_millis(1_500));
        let heard = Hello::decode(&said.encode()).expect("a greeting this build wrote");
        assert_eq!(
            heard.current_as_of,
            Some(Duration::from_secs(2)),
            "a fraction of a second rounded the copy younger, which is the one \
             direction a staleness reading may never move"
        );
    }

    #[test]
    fn a_greeting_that_stops_early_is_malformed_rather_than_a_panic() {
        let whole = greeting(ONE).encode();
        for stop in 0..whole.len() {
            let cut = whole.get(..stop).expect("a prefix of a vector");
            assert!(
                matches!(Hello::decode(cut), Err(Error::Malformed)),
                "{stop} bytes of a greeting decoded as something other than malformed"
            );
        }
    }

    #[test]
    fn a_role_this_build_does_not_know_is_a_newer_peer_and_says_so() {
        // Not `Malformed`: the body is exactly the shape a greeting takes. What
        // it carries is a fact from a build that knows more than this one, and
        // reporting that as a broken frame would send somebody to the wrong
        // question.
        let mut body = greeting(ONE).encode();
        let at = NODE_ID_LEN.saturating_add(20);
        *body
            .get_mut(at)
            .expect("the role byte a greeting always carries") = 0b1000_0000;
        assert!(matches!(
            Hello::decode(&body),
            Err(Error::UnknownRoles { bits: 0b1000_0000 })
        ));
    }

    #[test]
    fn a_node_whose_frame_agrees_with_its_credential_is_admitted() {
        let presented = Presented {
            node: ONE,
            purpose: Purpose::Peer,
        };
        assert!(admit(Some(&presented), &greeting(ONE), &ME).is_ok());
    }

    #[test]
    fn a_connection_that_proved_nothing_is_refused_before_anything_else() {
        // Absence is the default-deny case, and it is checked first: a greeting
        // from nobody is not improved by being well formed.
        assert!(matches!(
            admit(None, &greeting(ONE), &ME),
            Err(Error::Unidentified)
        ));
    }

    #[test]
    fn a_clients_credential_on_the_peer_link_is_refused_on_its_purpose() {
        // The id here is perfectly correct, which is the point: this refusal is
        // about what the credential was issued for and nothing else.
        let presented = Presented {
            node: ONE,
            purpose: Purpose::Client,
        };
        assert!(matches!(
            admit(Some(&presented), &greeting(ONE), &ME),
            Err(Error::NotAPeerCredential)
        ));
    }

    #[test]
    fn a_peer_arriving_under_this_nodes_own_id_is_refused() {
        // The credential agrees with the frame, which is what makes this worth
        // checking: every other refusal has already passed, so the only thing
        // left that can catch it is this end knowing its own name. A cluster
        // that issued this credential mis-issued it, and the door survives that
        // rather than trusting it did not happen.
        let presented = Presented {
            node: ME,
            purpose: Purpose::Peer,
        };
        let refused = admit(Some(&presented), &greeting(ME), &ME)
            .expect_err("a peer claiming this node's own id was admitted");
        assert!(matches!(refused, Error::ClaimsOurOwnIdentity { .. }));
        let said = refused.to_string();
        assert!(
            said.contains(&"03".repeat(NODE_ID_LEN)),
            "the claim: {said}"
        );
    }

    #[test]
    fn an_unproven_connection_claiming_this_nodes_id_is_refused_for_proving_nothing() {
        // The order of the two checks stated as a property rather than left to
        // the reading. A caller with no credential gets the refusal that names
        // the real problem; being told it collided with an identity would send
        // whoever reads the log looking for a second node that does not exist.
        assert!(matches!(
            admit(None, &greeting(ME), &ME),
            Err(Error::Unidentified)
        ));
    }

    #[test]
    fn a_frame_claiming_an_id_the_credential_does_not_name_is_refused() {
        let presented = Presented {
            node: ONE,
            purpose: Purpose::Peer,
        };
        let refused = admit(Some(&presented), &greeting(ANOTHER), &ME)
            .expect_err("a frame naming another node was admitted");
        assert!(matches!(refused, Error::IdentityDisagrees { .. }));
        // Both ids reach the operator, because whoever reads this needs to know
        // which of the two is the node they configured. Asserted on the rendered
        // message rather than on the fields: the rendering is what they see.
        let said = refused.to_string();
        assert!(
            said.contains(&"02".repeat(NODE_ID_LEN)),
            "the claim: {said}"
        );
        assert!(
            said.contains(&"01".repeat(NODE_ID_LEN)),
            "the credential: {said}"
        );
    }

    #[test]
    fn a_node_greets_under_the_identity_it_actually_holds() {
        // The reason `about` exists: a greeting assembled field by field could
        // disagree with the node's own stored identity, and nothing downstream
        // would ever see the difference.
        let identity = NodeIdentity::alone(ONE);
        let said = Hello::about(
            &identity,
            Epoch::new(3),
            Sequence::new(90),
            Epoch::new(2),
            Some(Duration::from_secs(11)),
        );
        assert_eq!(said.node, identity.id);
        assert_eq!(said.roles, identity.roles);
        assert_eq!(said.build, identity.version);
    }
}
