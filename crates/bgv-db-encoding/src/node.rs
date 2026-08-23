//! What a node knows about itself.
//!
//! This value answers one question — *who am I* — and it is deliberately the
//! only thing in the store that a backup does not carry. ADR-0018 §1 puts it in
//! the `META` keyspace rather than in the log because a replica reaches its
//! state by replaying that log: an identity travelling in it would be inherited
//! by whoever restored last night's backup onto a fresh machine, and two
//! processes would then claim the same id, heartbeat under it, and route from a
//! membership table that is wrong in a way nothing reports.
//!
//! Who *else* is here is the opposite decision and is not here. Membership is a
//! record, in the log, replicated, and it arrives with the second node.
//!
//! # Why the payload carries its own revision
//!
//! The value header already names the codec version, which is a property of the
//! whole store. This byte is a property of *this* value, and it is written from
//! the first commit rather than at the first migration: a value that gains
//! versioning later has one version that was never tagged, and that one stays
//! awkward forever (ADR-0018 §4).

use bgv_db_kv::Value;
use bgv_db_types::RecordId;

use crate::error::{Error, Result};
use crate::kind::KeyKind;
use crate::order::{KeyReader, KeyWriter};
use crate::value::{StoreValue, split_header, with_header};

/// The revision this build writes.
const REVISION: u8 = 1;

/// Bytes of identifier. Sixteen, so it is a record id without a conversion.
pub const NODE_ID_LEN: usize = 16;

/// What this node is for, as a set rather than as one word.
///
/// A set because the cases are combinations and not alternatives: a node may
/// serve reads and refuse writes, or serve nothing and only coordinate. An enum
/// would need widening on the first deployment that mixed two of them.
///
/// **Read-only is the absence of [`Roles::WRITABLE`]** and not a flag of its
/// own, which is what makes "a read-only node forwards writes to the leader" a
/// rule about roles rather than a mode carrying its own state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Roles(u8);

impl Roles {
    /// Answers client requests.
    pub const SERVING: Self = Self(0b0000_0001);
    /// Accepts writes rather than forwarding them.
    pub const WRITABLE: Self = Self(0b0000_0010);
    /// Takes part in deciding, rather than only in storing.
    pub const COORDINATING: Self = Self(0b0000_0100);

    /// Every bit this build assigns. A set bit outside it is from a newer build.
    const KNOWN: u8 = 0b0000_0111;

    /// A node standing on its own: it serves, and it accepts writes.
    ///
    /// Not `COORDINATING`, because there is nothing to coordinate with.
    pub const ALONE: Self = Self(Self::SERVING.0 | Self::WRITABLE.0);

    /// No roles at all.
    ///
    /// A real state rather than only a fold's starting point: a node that
    /// neither answers clients nor accepts writes still holds data, and saying
    /// so is how an operator drains one without stopping it.
    pub const NONE: Self = Self(0);

    /// Both of these roles, together.
    #[must_use]
    pub const fn and(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Whether this set includes `other`.
    #[must_use]
    pub const fn has(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// The raw bits, for the codec.
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// The roles present, in a fixed order, named.
    ///
    /// Ordered by bit rather than by insertion so that two nodes holding the
    /// same roles answer identically — an answer whose field order depends on
    /// how the set was built is an answer that cannot be compared.
    #[must_use]
    pub fn names(self) -> Vec<&'static str> {
        let mut found = Vec::new();
        for (role, name) in Self::NAMED {
            if self.has(role) {
                found.push(name);
            }
        }
        found
    }

    /// One role, from the word a statement wrote.
    ///
    /// Nothing here, and no set: `DEFINE NODE ROLES a, b` is a list, and a
    /// spelling that parsed into a *set* would make `ROLES serving` and
    /// `ROLES serving, serving` two different statements to reason about.
    /// The caller folds with [`Roles::and`].
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::NAMED
            .into_iter()
            .find(|(_, known)| *known == name)
            .map(|(role, _)| role)
    }

    /// Every role this build knows, with its name, in bit order.
    ///
    /// One table read in both directions, so a role that can be written can be
    /// read back by construction rather than because two lists were kept in
    /// step. A role named in only one of them is the drift this shape removes.
    const NAMED: [(Self, &'static str); 3] = [
        (Self::SERVING, "serving"),
        (Self::WRITABLE, "writable"),
        (Self::COORDINATING, "coordinating"),
    ];
}

/// Which build of the software this node last ran.
///
/// # Why it is here and why it is written rather than derived
///
/// It is the **updatable** half of the identity. The id never changes; this does,
/// every time the binary is replaced, and recording it is what makes an upgrade
/// something a node can notice rather than something an operator has to
/// remember. A node that starts and finds a version older than its own is a node
/// that has just been upgraded, and that instant is the only place a data
/// migration can correctly run — before the store serves anything.
///
/// Three ordered numbers rather than a string, because the question a migration
/// asks is "is the stored version below the one that needs the migration", and a
/// string answers that wrongly the first time a component reaches ten.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct NodeVersion {
    /// Breaking changes.
    pub major: u32,
    /// Additions.
    pub minor: u32,
    /// Fixes.
    pub patch: u32,
}

impl NodeVersion {
    /// The version this build is.
    ///
    /// Taken from the package version, which every crate in the workspace
    /// inherits from one place — so this is the product's version and not one
    /// crate's opinion of it.
    #[must_use]
    pub fn current() -> Self {
        Self {
            major: number(env!("CARGO_PKG_VERSION_MAJOR")),
            minor: number(env!("CARGO_PKG_VERSION_MINOR")),
            patch: number(env!("CARGO_PKG_VERSION_PATCH")),
        }
    }
}

impl core::fmt::Display for NodeVersion {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// One component of the package version.
///
/// A component that does not parse answers zero rather than raising: cargo
/// guarantees these are numbers, and a store that refused to open over its own
/// build metadata would be trading a real outage for an impossible one.
fn number(text: &str) -> u32 {
    text.parse().unwrap_or(0)
}

/// Whether this node stands on its own.
///
/// One variant at this milestone, and a tag byte so that the second one is an
/// assignment rather than a format change. `Alone` is a variant in its own right
/// and not a cluster of one, because "there are no peers" has to be the cheap
/// path — a check, not a walk over an empty list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Membership {
    /// No peers. The single-node store, which must pay nothing for the fact.
    #[default]
    Alone,
}

impl Membership {
    /// The tag written into the value.
    #[must_use]
    const fn tag(self) -> u8 {
        match self {
            Self::Alone => 0,
        }
    }

    /// Recover a membership from its tag.
    fn from_tag(tag: u8) -> Result<Self> {
        match tag {
            0 => Ok(Self::Alone),
            found => Err(Error::UnknownNodeIdentity {
                field: "membership",
                found,
            }),
        }
    }

    /// A stable name, as the query surface answers it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Alone => "alone",
        }
    }
}

/// This node's own identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeIdentity {
    /// Generated once, on first open, and stable across restarts.
    ///
    /// An id that changed on restart would be a session token wearing an
    /// identity's name, which is why the restart — not the read — is what F5
    /// asks about.
    pub id: [u8; NODE_ID_LEN],
    /// What this node is for.
    pub roles: Roles,
    /// Whether it has peers.
    pub membership: Membership,
    /// The build this node last ran.
    ///
    /// The one field here that is expected to change. It is rewritten whenever a
    /// node starts under a different binary, which is what turns an upgrade into
    /// an event the store can act on (see [`NodeVersion`]).
    pub version: NodeVersion,
    /// Where peers reach it, per surface.
    ///
    /// Empty at this milestone and carried anyway, for the reason `shards` is
    /// carried in the membership record: it is a fact about *this machine's*
    /// network position, so it is local, and a node that restores a backup must
    /// not inherit the original's address.
    pub endpoints: Vec<String>,
}

impl NodeIdentity {
    /// A fresh identity for a node standing on its own.
    #[must_use]
    pub fn alone(id: [u8; NODE_ID_LEN]) -> Self {
        Self {
            id,
            roles: Roles::ALONE,
            membership: Membership::Alone,
            version: NodeVersion::current(),
            endpoints: Vec::new(),
        }
    }

    /// The id as the record id it already is.
    ///
    /// Sixteen bytes render as lowercase hex, which is what `$node` answers and
    /// what an operator pastes into a search.
    #[must_use]
    pub const fn record_id(&self) -> RecordId {
        RecordId::Uuid(self.id)
    }
}

impl StoreValue for NodeIdentity {
    fn encode(&self) -> Value {
        let mut writer = KeyWriter::with_capacity(32);
        writer
            .put_u8(REVISION)
            .put_fixed(&self.id)
            .put_u8(self.roles.bits())
            .put_u8(self.membership.tag())
            .put_u32(self.version.major)
            .put_u32(self.version.minor)
            .put_u32(self.version.patch);
        for endpoint in &self.endpoints {
            // Terminated rather than counted, for the reason `LogRecord` gives:
            // a count is a second statement of a fact the bytes already carry,
            // and two statements of one fact can disagree.
            writer.put_variable(endpoint.as_bytes());
        }
        let payload = writer.finish();
        let mut buffer = with_header(0, payload.len());
        buffer.extend_from_slice(&payload);
        Value::from(buffer)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let (_, payload) = split_header(bytes, 0)?;
        let mut reader = KeyReader::new(KeyKind::NodeIdentity, payload);
        let revision = reader.take_u8()?;
        if revision != REVISION {
            return Err(Error::UnknownNodeIdentity {
                field: "revision",
                found: revision,
            });
        }
        let id = reader.take_fixed::<NODE_ID_LEN>()?;
        let bits = reader.take_u8()?;
        if bits & !Roles::KNOWN != 0 {
            // A role this build does not know is not a role to ignore: the
            // writer knew something about this node that this process does not,
            // and carrying on would mean acting as a node we cannot describe.
            return Err(Error::UnknownNodeIdentity {
                field: "role",
                found: bits,
            });
        }
        let membership = Membership::from_tag(reader.take_u8()?)?;
        // Before the endpoints, because those are terminated and run to the end
        // of the payload: a field added after them would have no place to sit.
        let version = NodeVersion {
            major: reader.take_u32()?,
            minor: reader.take_u32()?,
            patch: reader.take_u32()?,
        };
        let mut endpoints = Vec::new();
        while reader.remaining() > 0 {
            let raw = reader.take_variable()?;
            endpoints.push(String::from_utf8(raw).map_err(|_| Error::InvalidNodeEndpoint)?);
        }
        Ok(Self {
            id,
            roles: Roles(bits),
            membership,
            version,
            endpoints,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn every_role_that_can_be_written_can_be_read_back() {
        // The property the shared table buys, asserted rather than assumed: a
        // role the answer names is a role a statement can set. Two hand-kept
        // lists would drift here silently, and the symptom would be an operator
        // told a role exists and refused when they write it.
        for name in Roles::SERVING
            .and(Roles::WRITABLE)
            .and(Roles::COORDINATING)
            .names()
        {
            let parsed = Roles::parse(name).expect("a reported role must parse");
            assert_eq!(parsed.names(), vec![name]);
        }
        assert!(Roles::parse("leader").is_none());
        assert!(Roles::parse("Serving").is_none(), "matching is exact");
    }

    #[test]
    fn no_roles_is_a_state_and_not_a_missing_answer() {
        assert!(Roles::NONE.names().is_empty());
        assert!(!Roles::NONE.has(Roles::SERVING));
        assert_eq!(Roles::NONE.and(Roles::SERVING), Roles::SERVING);
    }

    #[test]
    fn an_identity_round_trips_through_its_bytes() {
        let original = NodeIdentity {
            id: [0x5a; NODE_ID_LEN],
            roles: Roles::ALONE.and(Roles::COORDINATING),
            membership: Membership::Alone,
            version: NodeVersion {
                major: 3,
                minor: 14,
                patch: 159,
            },
            endpoints: vec!["127.0.0.1:8000".to_owned(), "[::1]:8001".to_owned()],
        };
        let encoded = original.encode();
        assert_eq!(NodeIdentity::decode(encoded.as_slice()).unwrap(), original);
    }

    #[test]
    fn a_node_standing_alone_serves_and_writes_and_does_not_coordinate() {
        let identity = NodeIdentity::alone([1; NODE_ID_LEN]);
        assert!(identity.roles.has(Roles::SERVING));
        assert!(identity.roles.has(Roles::WRITABLE));
        assert!(!identity.roles.has(Roles::COORDINATING));
        assert_eq!(identity.roles.names(), vec!["serving", "writable"]);
    }

    #[test]
    fn an_empty_endpoint_list_round_trips_as_an_empty_list() {
        // The arm that would otherwise be tested only by the arm that has data:
        // a terminated encoding with nothing in it must decode to nothing, not
        // to one empty string.
        let original = NodeIdentity::alone([7; NODE_ID_LEN]);
        let encoded = original.encode();
        let found = NodeIdentity::decode(encoded.as_slice()).unwrap();
        assert!(found.endpoints.is_empty());
        assert_eq!(found, original);
    }

    #[test]
    fn a_role_bit_this_build_does_not_know_is_refused_rather_than_ignored() {
        let mut bytes = NodeIdentity::alone([2; NODE_ID_LEN]).encode().into_bytes();
        // Header (2) + revision (1) + id (16) lands on the roles byte.
        bytes[19] |= 0b1000_0000;
        assert!(NodeIdentity::decode(&bytes).is_err());
    }

    #[test]
    fn the_id_renders_as_the_hex_an_operator_reads() {
        let identity = NodeIdentity::alone([0xab; NODE_ID_LEN]);
        assert_eq!(identity.record_id().to_string(), "ab".repeat(NODE_ID_LEN));
    }
}
