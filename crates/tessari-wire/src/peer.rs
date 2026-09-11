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
}

impl PeerFrame {
    /// The tag written on the wire.
    ///
    /// Six and upward, which is exactly the range the client's reader refuses as
    /// unknown — so a peer frame arriving on the client port is closed rather
    /// than misread, and the two spaces cannot silently overlap.
    #[must_use]
    pub const fn tag(self) -> u8 {
        match self {
            Self::Hello => 6,
            Self::Ballot => 7,
            Self::Vote => 8,
        }
    }

    /// Recover a kind from its tag.
    #[must_use]
    pub const fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            6 => Some(Self::Hello),
            7 => Some(Self::Ballot),
            8 => Some(Self::Vote),
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
}

impl Hello {
    /// This node's own greeting, built from its own stored identity.
    ///
    /// Taken from [`NodeIdentity`] rather than assembled field by field so that
    /// a node cannot greet under an id, a role set or a build that disagree with
    /// what it actually holds. The two arguments are the two facts an identity
    /// deliberately does not carry, because both change with every commit.
    #[must_use]
    pub fn about(identity: &NodeIdentity, epoch: Epoch, tail: Sequence) -> Self {
        Self {
            node: identity.id,
            build: identity.version,
            epoch,
            roles: identity.roles,
            tail,
        }
    }

    /// The body of a [`PeerFrame::Hello`] frame.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut body = Vec::with_capacity(45);
        body.extend_from_slice(&self.node);
        frame::put_u32(&mut body, self.build.major);
        frame::put_u32(&mut body, self.build.minor);
        frame::put_u32(&mut body, self.build.patch);
        frame::put_u64(&mut body, self.epoch.get());
        body.push(self.roles.bits());
        frame::put_u64(&mut body, self.tail.get());
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
        let (tail, _) = frame::take_u64(body, at.checked_add(1).ok_or(Error::Malformed)?)?;

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
        })
    }
}

/// Decide whether the other end may speak on this link.
///
/// The three refusals are three different failures and are kept apart, because
/// one "handshake failed" sends whoever reads it to a packet capture for each:
/// nothing was proven at all means the port let a stranger through; the wrong
/// purpose means a credential was presented where it was never meant to be; and
/// a disagreeing id means either a mis-issued credential or a node claiming
/// somebody else's name.
///
/// Absence is refused rather than defaulted. A handshake that admitted an
/// unproven peer would make every check after it a formality performed on
/// whoever asked.
///
/// # Errors
///
/// Returns [`Error::Unidentified`], [`Error::NotAPeerCredential`] or
/// [`Error::IdentityDisagrees`], one per refusal above.
pub fn admit(presented: Option<&Presented>, said: &Hello) -> Result<()> {
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Hello, PeerFrame, Presented, Purpose, admit};
    use crate::error::Error;
    use crate::frame;
    use tessari_encoding::{NODE_ID_LEN, NodeIdentity, NodeVersion, Roles};
    use tessari_types::{Epoch, Sequence};

    const ONE: [u8; NODE_ID_LEN] = [1; NODE_ID_LEN];
    const ANOTHER: [u8; NODE_ID_LEN] = [2; NODE_ID_LEN];

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
        assert_eq!(
            PeerFrame::Hello.tag(),
            6,
            "the peer space starts where 1-5 ends"
        );
    }

    #[test]
    fn a_greeting_survives_the_wire_unchanged() {
        let said = greeting(ONE);
        let heard = Hello::decode(&said.encode()).expect("a greeting this build wrote");
        assert_eq!(heard, said);
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
        assert!(admit(Some(&presented), &greeting(ONE)).is_ok());
    }

    #[test]
    fn a_connection_that_proved_nothing_is_refused_before_anything_else() {
        // Absence is the default-deny case, and it is checked first: a greeting
        // from nobody is not improved by being well formed.
        assert!(matches!(
            admit(None, &greeting(ONE)),
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
            admit(Some(&presented), &greeting(ONE)),
            Err(Error::NotAPeerCredential)
        ));
    }

    #[test]
    fn a_frame_claiming_an_id_the_credential_does_not_name_is_refused() {
        let presented = Presented {
            node: ONE,
            purpose: Purpose::Peer,
        };
        let refused = admit(Some(&presented), &greeting(ANOTHER))
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
        let said = Hello::about(&identity, Epoch::new(3), Sequence::new(90));
        assert_eq!(said.node, identity.id);
        assert_eq!(said.roles, identity.roles);
        assert_eq!(said.build, identity.version);
    }
}
