//! The peers this store knows about.
//!
//! A replica is a catalog entry like any other — an ordinary record in the
//! system tenancy (ADR-0009) — so declaring one takes part in the transaction
//! that issued it and replicates through the same apply path. That is the whole
//! reason it is here rather than beside the node's own identity in `META`:
//! **every node must learn that a peer exists**, which is what replicating it is
//! for, while no node may inherit *another's* identity, which is what keeping
//! that out of the log is for (ADR-0018, ADR-0020 §3).
//!
//! The test deciding which side a setting falls on is ADR-0018's own, and it is
//! sharp: *what happens when this setting is replayed on another machine — does
//! that machine become confused about which one it is?* A peer list replayed
//! elsewhere teaches that machine the same peers, which is correct. An endpoint
//! replayed elsewhere tells peers to reach the wrong host, which is not.
//!
//! # A row may also be about this node
//!
//! Since the desired role arrived (`04_concept.md` §6.1), a row may carry the
//! id of the node it is about — including this one's. That does not make the
//! table local: the row still replicates, still describes topology, and still
//! means the same thing on every node that holds it. What changes is that
//! exactly one node finds its own id in it, and that node reads the row's roles
//! as what it is *supposed* to be. See [`Catalog::desired_roles`].
//!
//! # What a replica does not carry yet
//!
//! Which ranges it holds, and how many copies of the data there should be. Both
//! are replicated facts and both belong here eventually; neither is written
//! today, because there is no second node to enforce anything against and a
//! field nothing reads is a decision taken with no way to find out it was wrong.

mod declaring;
mod peers;

use std::collections::BTreeMap;

use tessari_encoding::{NODE_ID_LEN, Roles, decode_payload, encode_payload};
use tessari_types::{Number, RecordId, Value};

use super::authority::{Reach, ReachCodec};
use super::definition::{field_name, number, object};
use super::{Catalog, Level, qualify, system};
use crate::error::{Error, Result};
pub use peers::{another_node_may_write, names_a_peer, the_row_a_greeting_binds};

const FIELD_NAME: &str = "name";
const FIELD_ENDPOINT: &str = "endpoint";
const FIELD_ROLES: &str = "roles";
const FIELD_NODE: &str = "node";
const FIELD_REPLICATES: &str = "replicates";
const FIELD_LEADS: &str = "leads";
const FIELD_CLIENTS: &str = "clients";
const FIELD_HTTP: &str = "http";
const FIELD_FINGERPRINT: &str = "fingerprint";
const FIELD_JOIN: &str = "join";
const FIELD_RELEASING: &str = "releasing";
const FIELD_PREFERRED: &str = "preferred";
const FIELD_REGION: &str = "region";
const FIELD_DIGEST: &str = "digest";
const FIELD_EXPIRES: &str = "expires";

const ENTITY: &str = "replica";

/// A peer, and where it answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicaDefinition {
    /// The name it is known by, unique across the store, and **the identity of
    /// the stored record** (ADR-0077).
    ///
    /// Not the node's generated identifier: that one is sixteen unpredictable
    /// bytes a node gives *itself*, and nothing else can know it before the two
    /// have spoken. This is the name an operator writes down.
    pub name: String,
    /// Where it is reached.
    ///
    /// Stored as written. Which surface a host and port belong to, and whether
    /// it resolves, are questions for whoever dials it — refusing an
    /// unreachable address at declaration time would make the statement's
    /// success depend on the network being up at the moment it ran.
    pub endpoint: String,
    /// What that peer is for.
    ///
    /// The field a forward is looked up by: a node that may not write finds the
    /// peer whose roles carry [`Roles::WRITABLE`] and sends the statement there.
    /// ADR-0019 §1 puts the leader in the membership table and ADR-0018 §2 puts
    /// `roles` on its rows, so this is that row's field arriving rather than a
    /// new idea — and it is declared by an operator rather than written by the
    /// peer itself, because a self-maintained row needs a heartbeat, and a
    /// heartbeat is failure detection, which this milestone deliberately has
    /// none of.
    ///
    /// [`Roles::NONE`] when the declaration did not say, which reads as *this
    /// peer takes no writes*. That is the safe direction: the cost of being
    /// wrong is a refusal an operator can see and fix, where the other
    /// direction commits a write on a follower.
    pub roles: Roles,
    /// Which node this row is about, when anybody knows.
    ///
    /// `None` is the row as it has always been: a peer an operator declared by
    /// name and endpoint, before anything had spoken to it. Nothing can know
    /// another node's generated id until first contact, so a row that names one
    /// is a row somebody bound deliberately.
    ///
    /// # What the binding is for
    ///
    /// It is what makes [`roles`] a **desired role** rather than a note about
    /// somebody else. A node compares this against its own id, and the row that
    /// matches is the one the operator wrote about *it* — see
    /// [`Catalog::desired_roles`].
    ///
    /// # Why the id and not the name
    ///
    /// The name is an operator's word and every node can read it, so a role
    /// written against a name would arrive at whichever node happened to answer
    /// to it. The id is sixteen bytes a node gave itself and never shares with
    /// the log, so three things hold without a rule for any of them: the row
    /// replicates to every follower and matches exactly one of them; a node that
    /// restored a backup has a *fresh* id and therefore inherits no role, which
    /// is ADR-0018 §1's property surviving this change untouched; and the value
    /// an operator has to type is the one `INFO FOR NODE` already prints.
    ///
    /// [`roles`]: Self::roles
    pub node: Option<[u8; NODE_ID_LEN]>,
    /// How far this peer may collect this store's log, when it may at all.
    ///
    /// `None` is *never asked* and it is the refusal. It is deliberately not
    /// spelled as an empty reach: a peer declared before this field existed was
    /// never granted anything, and collapsing that into *granted nothing* would
    /// make the two indistinguishable the moment somebody wants to tell them
    /// apart. The field is written only when the declaration said so, so such a
    /// row encodes back byte-identical.
    ///
    /// # It is one value on purpose
    ///
    /// This is both halves of the question the peer door asks — whether that
    /// node may take the log at all, and how much of it it then receives. A
    /// design in which the permission and the filter were two values has a
    /// state in which a peer authorized for one namespace is served another,
    /// and neither of the two calls is wrong about its own argument.
    ///
    /// # What granting it discloses
    ///
    /// The log is a stream of mutations and the identity class is in it, so a
    /// subscription hands over the users, credential hashes and grants **inside
    /// its reach**. [`Reach::Store`] therefore hands over every tenancy's, which
    /// is why it is an operator's explicit word and never a default.
    pub replicates: Option<Reach>,
    /// The range this row's node stands to lead, when the operator placed one
    /// (ADR-0082).
    ///
    /// `None` is the row as it has always been: the node stands for the store
    /// and nothing else. A placed range is carved out of the store line for
    /// EVERY node, not only this one — the store leader stops writing it the
    /// moment this row commits — so the field is read by the write gate on
    /// every node and by the campaign only on the node the row names.
    pub leads: Option<Reach>,
    /// Where a **client** reaches that peer over the wire, when somebody said
    /// (`CLIENTS AT`, ADR-0101).
    ///
    /// [`Self::endpoint`] is the peer door, which speaks only to other nodes, so
    /// a redirect that named it would send a client somewhere it cannot talk.
    /// `None` keeps every redirect exactly as it was before this field existed.
    pub clients: Option<String>,
    /// The HTTP base a client reaches that peer at (`HTTP AT`), for the
    /// `Location` of a `307`. `None` for the same reason as [`Self::clients`].
    pub http: Option<String>,
    /// The SHA-256 of the one certificate allowed to bind this row
    /// (`FINGERPRINT`, ADR-0108 D9), lowercase hex.
    ///
    /// One of the three ways a row learns its node — `NODE`, this, or a join
    /// token — because a row that bound whichever peer arrived first handed its
    /// whole reach to it (R-15).
    pub fingerprint: Option<String>,
    /// A join token waiting to bind this row (`CREATE JOIN TOKEN`), as its
    /// digest and its expiry; cleared by the binding that spends it.
    pub join: Option<JoinTicket>,
    /// The placement is being given back to the store line (ADR-0098 D3).
    ///
    /// Set by `ALTER REPLICA … LEADS NONE` on the last row placing a range.
    /// [`Self::leads`] is kept, so the range stays carved out of the store
    /// line on every node and nobody else writes it; this row's node no longer
    /// stands for it, and the store line's leader stands instead. Once it
    /// leads the range too it folds the placement away
    /// ([`Catalog::finish_release`]). Written only when true.
    pub releasing: bool,
    /// `LEADS … PREFERRED`: the candidate a non-preferred leader of the range
    /// hands it to once this one is caught up (G053 SG5b). Written only when
    /// true.
    pub preferred: bool,
    /// The region the peer stands in (`REGION 'eu'`), when said: what a
    /// `LOCAL MAJORITY` counts its voters by (G057 C3). Written only when
    /// stated, so a row declared before it keeps its bytes.
    pub region: Option<String>,
}

/// A one-time join token as the catalog keeps it: never the token itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinTicket {
    /// The token's SHA-256, lowercase hex.
    pub digest: String,
    /// When it stops binding, in milliseconds since the Unix epoch.
    pub expires_ms: i64,
}

impl ReplicaDefinition {
    /// The value written to the catalog.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut fields = BTreeMap::from([
            (FIELD_NAME.to_owned(), Value::from(self.name.as_str())),
            (
                FIELD_ENDPOINT.to_owned(),
                Value::from(self.endpoint.as_str()),
            ),
            (FIELD_ROLES.to_owned(), number(u32::from(self.roles.bits()))),
        ]);
        // Written only when there is one, so an unbound row is byte-identical to
        // the row this build's predecessor wrote. A stored `null` would be a
        // second spelling of absent, and the reader would then have two.
        if let Some(node) = self.node {
            fields.insert(FIELD_NODE.to_owned(), Value::Uuid(node));
        }
        // Written only when it was stated, for the same reason and with more at
        // stake: an absent subscription *is* the refusal.
        if let Some(reach) = self.replicates {
            fields.insert(FIELD_REPLICATES.to_owned(), reach.to_value());
        }
        // Written only when it was stated, so a row with no placement keeps the
        // bytes it always had.
        if let Some(reach) = self.leads {
            fields.insert(FIELD_LEADS.to_owned(), reach.to_value());
        }
        // Written only when stated, so a row declared before ADR-0101 keeps its
        // bytes and a reader of either age sees one spelling of "not said".
        if let Some(clients) = &self.clients {
            fields.insert(FIELD_CLIENTS.to_owned(), Value::from(clients.as_str()));
        }
        if let Some(http) = &self.http {
            fields.insert(FIELD_HTTP.to_owned(), Value::from(http.as_str()));
        }
        // Both written only when there is one, so a row declared before
        // ADR-0108 D9 keeps its bytes.
        if let Some(fingerprint) = &self.fingerprint {
            fields.insert(
                FIELD_FINGERPRINT.to_owned(),
                Value::from(fingerprint.as_str()),
            );
        }
        if self.releasing {
            fields.insert(FIELD_RELEASING.to_owned(), Value::Bool(true));
        }
        if self.preferred {
            fields.insert(FIELD_PREFERRED.to_owned(), Value::Bool(true));
        }
        if let Some(region) = &self.region {
            fields.insert(FIELD_REGION.to_owned(), Value::from(region.as_str()));
        }
        if let Some(join) = &self.join {
            fields.insert(
                FIELD_JOIN.to_owned(),
                Value::Object(BTreeMap::from([
                    (FIELD_DIGEST.to_owned(), Value::from(join.digest.as_str())),
                    (
                        FIELD_EXPIRES.to_owned(),
                        Value::Number(Number::Integer(join.expires_ms)),
                    ),
                ])),
            );
        }
        Value::Object(fields)
    }

    /// Read a definition back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when a field is missing or holds the
    /// wrong type.
    pub fn from_value(value: &Value) -> Result<Self> {
        let fields = object(value, ENTITY)?;
        let Some(Value::String(endpoint)) = fields.get(FIELD_ENDPOINT) else {
            return Err(Error::CatalogMalformed {
                entity: ENTITY,
                field: FIELD_ENDPOINT,
                found: fields.get(FIELD_ENDPOINT).map_or("none", Value::type_name),
            });
        };
        Ok(Self {
            name: field_name(fields, ENTITY)?,
            endpoint: endpoint.clone(),
            roles: roles_in(fields)?,
            node: node_in(fields)?,
            replicates: replicates_in(fields)?,
            leads: fields
                .get(FIELD_LEADS)
                .map(|found| Reach::from_value(found, ENTITY, FIELD_LEADS))
                .transpose()?,
            clients: text_in(fields, FIELD_CLIENTS)?,
            http: text_in(fields, FIELD_HTTP)?,
            fingerprint: text_in(fields, FIELD_FINGERPRINT)?,
            join: join_in(fields)?,
            releasing: flag_in(fields, FIELD_RELEASING)?,
            preferred: flag_in(fields, FIELD_PREFERRED)?,
            region: text_in(fields, FIELD_REGION)?,
        })
    }
}

/// A flag written only when true: absent is false, and anything present that
/// is not true or false is refused rather than read as either.
fn flag_in(fields: &BTreeMap<String, Value>, field: &'static str) -> Result<bool> {
    match fields.get(field) {
        None => Ok(false),
        Some(Value::Bool(set)) => Ok(*set),
        Some(other) => Err(Error::CatalogMalformed {
            entity: ENTITY,
            field,
            found: other.type_name(),
        }),
    }
}

/// The join token a stored row waits on: absent is none, anything else that is
/// not a digest and an expiry is refused rather than read as none.
fn join_in(fields: &BTreeMap<String, Value>) -> Result<Option<JoinTicket>> {
    let Some(found) = fields.get(FIELD_JOIN) else {
        return Ok(None);
    };
    let malformed = |field: &'static str, found: &Value| Error::CatalogMalformed {
        entity: ENTITY,
        field,
        found: found.type_name(),
    };
    let Value::Object(join) = found else {
        return Err(malformed(FIELD_JOIN, found));
    };
    match (join.get(FIELD_DIGEST), join.get(FIELD_EXPIRES)) {
        (Some(Value::String(digest)), Some(Value::Number(Number::Integer(expires_ms)))) => {
            Ok(Some(JoinTicket {
                digest: digest.clone(),
                expires_ms: *expires_ms,
            }))
        }
        _ => Err(malformed(FIELD_JOIN, found)),
    }
}

/// An optional text field of a stored row: absent is `None`, and anything
/// present that is not text is refused rather than read as absent.
fn text_in(fields: &BTreeMap<String, Value>, field: &'static str) -> Result<Option<String>> {
    match fields.get(field) {
        None => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(other) => Err(Error::CatalogMalformed {
            entity: ENTITY,
            field,
            found: other.type_name(),
        }),
    }
}

/// The subscription a stored definition carries.
///
/// Absent reads as `None` — no subscription — which is the rule every property
/// added after the fact follows here and, uniquely among them, the rule that is
/// also the safe direction. Anything present and not readable as a reach is
/// **refused**: something well-formed that is not a subscription would otherwise
/// be read as *this peer was granted nothing*, and a grant that silently
/// evaporates is a follower that silently stops receiving.
fn replicates_in(fields: &BTreeMap<String, Value>) -> Result<Option<Reach>> {
    fields
        .get(FIELD_REPLICATES)
        .map(|found| Reach::from_value(found, ENTITY, FIELD_REPLICATES))
        .transpose()
}

/// The roles a stored definition carries.
///
/// Absent reads as [`Roles::NONE`], so a peer declared before this field existed
/// is readable rather than refused — the rule `flag` already sets for every
/// other property added after the fact. A value of the wrong type or one holding
/// bits this build has no name for is **refused**, for the same reason `flag`
/// refuses a non-flag: something wrote a well-formed value that is not roles,
/// and defaulting it would turn an integrity problem into a routing decision.
fn roles_in(fields: &BTreeMap<String, Value>) -> Result<Roles> {
    let Some(found) = fields.get(FIELD_ROLES) else {
        return Ok(Roles::NONE);
    };
    let Value::Number(Number::Integer(bits)) = found else {
        return Err(Error::CatalogMalformed {
            entity: ENTITY,
            field: FIELD_ROLES,
            found: found.type_name(),
        });
    };
    u8::try_from(*bits)
        .ok()
        .and_then(Roles::from_bits)
        .ok_or(Error::CatalogMalformed {
            entity: ENTITY,
            field: FIELD_ROLES,
            found: "roles",
        })
}

/// The node a stored definition names.
///
/// Absent reads as `None`, the rule every property added after the fact follows
/// here. A value of the wrong type is **refused** rather than ignored, on
/// `roles_in`'s reasoning and with more at stake: something well-formed that is
/// not a node id would otherwise be read as *this row names nobody*, and a row
/// that silently stops naming a node is a node that silently stops converging.
fn node_in(fields: &BTreeMap<String, Value>) -> Result<Option<[u8; NODE_ID_LEN]>> {
    match fields.get(FIELD_NODE) {
        None => Ok(None),
        Some(Value::Uuid(bytes)) => Ok(Some(*bytes)),
        Some(found) => Err(Error::CatalogMalformed {
            entity: ENTITY,
            field: FIELD_NODE,
            found: found.type_name(),
        }),
    }
}

/// Who arrived at the door, as far as binding a row is concerned.
#[derive(Debug, Clone, Copy)]
pub struct Greeter<'a> {
    /// The node the greeting and its certificate both name.
    pub node: [u8; NODE_ID_LEN],
    /// The SHA-256 of the certificate it presented, lowercase hex.
    pub fingerprint: &'a str,
    /// The SHA-256 of the join token it carried, lowercase hex, if it carried one.
    pub token: Option<&'a str>,
}

#[cfg(test)]
mod tests;
