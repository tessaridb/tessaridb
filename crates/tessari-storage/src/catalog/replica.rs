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

use std::collections::BTreeMap;

use tessari_encoding::{NODE_ID_LEN, Roles, decode_payload};
use tessari_types::{Number, RecordId, Value};

use super::definition::{field_id, field_name, number, object};
use super::{Catalog, Level, id_key, qualify, system};
use crate::error::{Error, Result};

const FIELD_ID: &str = "id";
const FIELD_NAME: &str = "name";
const FIELD_ENDPOINT: &str = "endpoint";
const FIELD_ROLES: &str = "roles";
const FIELD_NODE: &str = "node";

const ENTITY: &str = "replica";

/// A peer, and where it answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicaDefinition {
    /// Its id.
    pub id: u32,
    /// The name it is known by, unique across the store.
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
}

impl ReplicaDefinition {
    /// The value written to the catalog.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut fields = BTreeMap::from([
            (FIELD_ID.to_owned(), number(self.id)),
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
            id: field_id(fields, FIELD_ID, ENTITY)?,
            name: field_name(fields, ENTITY)?,
            endpoint: endpoint.clone(),
            roles: roles_in(fields)?,
            node: node_in(fields)?,
        })
    }
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

impl Catalog<'_, '_> {
    /// Declare a peer.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NameTaken`] when the name is already declared.
    pub fn create_replica(
        &mut self,
        name: &str,
        endpoint: &str,
        roles: Roles,
        node: Option<[u8; NODE_ID_LEN]>,
    ) -> Result<ReplicaDefinition> {
        let qualified = qualify(Level::Replica, &[], name);
        self.reserve_name(&qualified)?;
        let id = self.allocate(Level::Replica)?;
        let definition = ReplicaDefinition {
            id,
            name: name.to_owned(),
            endpoint: endpoint.to_owned(),
            roles,
            node,
        };
        self.write(system::REPLICAS, id, &definition.to_value());
        self.claim_name(&qualified, id);
        Ok(definition)
    }

    /// Remove a peer's declaration and release its name.
    ///
    /// Answers `false` when there was nothing under that id.
    ///
    /// Removing the declaration is all this does. The peer is not told, and
    /// nothing chases the data it already holds — which is the honest shape for
    /// a store whose replication is declarative: this statement says *we no
    /// longer count that endpoint as a peer*, and a peer that disagrees is a
    /// question for the operator rather than one a catalog write can settle.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored definition cannot be read.
    pub fn drop_replica(&mut self, id: u32) -> Result<bool> {
        let Some(definition) = self.replicas()?.into_iter().find(|found| found.id == id) else {
            return Ok(false);
        };
        let qualified = qualify(Level::Replica, &[], &definition.name);
        self.transaction
            .delete(system::address(system::REPLICAS, RecordId::Int(id_key(id))));
        self.transaction
            .delete(system::address(system::NAMES, RecordId::from(qualified)));
        Ok(true)
    }

    /// Every declared peer, in name order.
    ///
    /// Sorted here rather than by the caller, for the reason `INFO FOR`'s name
    /// lists are sorted: the catalog hands these back in the order somebody
    /// happened to declare them, and an answer whose shape depends on that is
    /// two answers to one question — which is worse here than elsewhere,
    /// because two nodes comparing peer lists is the point of having one.
    ///
    /// # Errors
    ///
    /// Returns an error when a stored definition cannot be read.
    pub fn replicas(&self) -> Result<Vec<ReplicaDefinition>> {
        let mut found = Vec::new();
        for (_, payload) in self.transaction.scan_table(
            system::SYSTEM_NAMESPACE,
            system::SYSTEM_DATABASE,
            system::REPLICAS,
        )? {
            found.push(ReplicaDefinition::from_value(&decode_payload(&payload)?)?);
        }
        found.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(found)
    }

    /// What the cluster says a node should be, if anything says so.
    ///
    /// The **desired** role of `04_concept.md` §6.1: a replicated catalog record
    /// an operator writes, against which a node reconciles what it actually
    /// holds. `None` when no membership row names this node — which is every
    /// store until somebody binds one, and is why this changes nothing for a
    /// node standing on its own.
    ///
    /// # One place, so the two readers cannot disagree
    ///
    /// Two callers ask this question — the node reconciling itself at open, and
    /// `INFO FOR NODE` reporting what it will reconcile to — and they must never
    /// answer it differently, because the whole value of reporting a desired
    /// role is that it predicts the one that will be adopted. So the rule for
    /// *which row is mine* lives here and is called twice, rather than being
    /// written twice and kept in step by hand.
    ///
    /// # The first match, and why there can only be one
    ///
    /// Nothing stops an operator binding two rows to one node, and nothing here
    /// tries to arbitrate: `replicas` hands them back in **name order**, so the
    /// answer is stable rather than dependent on declaration order, which is the
    /// property that matters when two nodes compare what they think the cluster
    /// says. A second binding is an operator error and is visible in
    /// `INFO FOR NODE`'s peer list, where both rows are shown carrying the same
    /// id.
    ///
    /// # Errors
    ///
    /// Returns an error when a stored definition cannot be read.
    pub fn desired_roles(&self, node: &[u8; NODE_ID_LEN]) -> Result<Option<Roles>> {
        Ok(self
            .replicas()?
            .into_iter()
            .find(|found| found.node.as_ref() == Some(node))
            .map(|found| found.roles))
    }
}
