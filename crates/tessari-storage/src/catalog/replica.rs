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

use tessari_encoding::{NODE_ID_LEN, Roles, decode_payload, encode_payload};
use tessari_types::{Number, RecordId, Value};

use super::authority::{Reach, ReachCodec};
use super::definition::{field_name, number, object};
use super::{Catalog, Level, qualify, system};
use crate::error::{Error, Result};

const FIELD_NAME: &str = "name";
const FIELD_ENDPOINT: &str = "endpoint";
const FIELD_ROLES: &str = "roles";
const FIELD_NODE: &str = "node";
const FIELD_REPLICATES: &str = "replicates";

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
        })
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
        replicates: Option<Reach>,
    ) -> Result<ReplicaDefinition> {
        if self.replica_row(name)?.is_some() {
            return Err(Error::NameTaken {
                qualified: qualify(Level::Replica, &[], name),
            });
        }
        let definition = ReplicaDefinition {
            name: name.to_owned(),
            endpoint: endpoint.to_owned(),
            roles,
            node,
            replicates,
        };
        self.write_replica(&definition);
        Ok(definition)
    }

    /// The stored row a name is written under, read by that key.
    ///
    /// A point read rather than a scan, which is what makes the name an identity
    /// rather than a field somebody searches on: the row either exists at its own
    /// key or it does not, and the answer costs one lookup.
    fn replica_row(&self, name: &str) -> Result<Option<ReplicaDefinition>> {
        let Some(bytes) = self
            .transaction
            .get(&system::address(system::REPLICAS, RecordId::from(name)))?
        else {
            return Ok(None);
        };
        ReplicaDefinition::from_value(&decode_payload(&bytes)?).map(Some)
    }

    /// Write a row at the key its own name gives it.
    ///
    /// Not [`Catalog::write`], which keys by an allocated number. **That number
    /// is what ADR-0077 removed**: it was handed out by the WRITING node while
    /// the table is cluster-wide, so two nodes declaring different peers gave one
    /// number to both and the first replication overwrote one with the other.
    fn write_replica(&mut self, definition: &ReplicaDefinition) {
        self.transaction.put(
            system::address(system::REPLICAS, RecordId::from(definition.name.as_str())),
            encode_payload(&definition.to_value()).into_bytes(),
        );
    }

    /// Bind a declared row to the node whose greeting proved it.
    ///
    /// Writes the `node` field and nothing else. The endpoint, the roles and the
    /// reach stay exactly as the operator declared them, because those are the
    /// operator's decision and the greeting is evidence of an identity only.
    ///
    /// Answers `false` when there is no row under that id — the same shape
    /// [`Self::drop_replica`] uses, and for the same reason: the caller is
    /// reconciling against a list it read a moment ago, and a row that has since
    /// been dropped is an ordinary race rather than a failure.
    ///
    /// It does **not** decide whether the row should be bound. That question has
    /// two refusals in it and they live in [`the_row_a_greeting_binds`], which is
    /// a pure function over the declarations and is therefore testable without a
    /// store.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored definitions cannot be read.
    pub fn bind_replica_node(&mut self, name: &str, node: [u8; NODE_ID_LEN]) -> Result<bool> {
        let Some(mut definition) = self.replica_row(name)? else {
            return Ok(false);
        };
        definition.node = Some(node);
        self.write_replica(&definition);
        Ok(true)
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
    pub fn drop_replica(&mut self, name: &str) -> Result<bool> {
        if self.replica_row(name)?.is_none() {
            return Ok(false);
        }
        self.transaction
            .delete(system::address(system::REPLICAS, RecordId::from(name)));
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

/// Does the catalog name a peer that is not this node?
///
/// The one spelling of *this store is in a cluster*, and it lives here rather
/// than beside either of its callers because it is a question about
/// [`ReplicaDefinition`] and nothing else. `tessari_wire` re-exports it; a
/// second copy over there would be a second definition of membership, and the
/// two would agree until the day they did not.
///
/// # It is not "is the catalog empty"
///
/// [`Catalog::replicas`] returns every membership row, **including the one that
/// describes THIS node** — and the row a cluster writes to admit a newcomer is
/// exactly that row. So a joiner's first collection brings in one row, its own,
/// and an emptiness bound reads that as *the catalog can answer* and stops
/// dialling the seed, while `upstream` and the greeting round both skip the row
/// naming this node. The node collects once, follows nobody afterwards, and
/// nothing is in an error state while it happens (W260).
///
/// A row naming no node at all counts for nothing here, for the same reason it
/// counts for nothing upstream: there is no identity to dial, and none to check
/// a credential against.
///
/// # Why the write gate asks this and not what role the node was given
///
/// [`crate::Store::awaiting_leadership`] used to read `Roles::COORDINATING`, on
/// the reasoning that [`Roles::ALONE`] is documented as *not `COORDINATING`,
/// because there is nothing to coordinate with*, so the bit is already the line
/// between a member of a deciding set and a store on its own.
///
/// That is true about the line it draws and it answers the wrong question. The
/// roles are a **set**, not an enum, so `SERVING | WRITABLE` without
/// `COORDINATING` is a legal and ordinary declaration — a writable node that
/// does not vote. Such a node, sitting in a cluster beside an elected leader,
/// carried no lease and was asked for none: the gate wanted a bit it does not
/// have, so it wrote freely and silently, which is the failure the fence exists
/// to prevent arriving through the one role combination the predicate missed.
///
/// *May this node take part in deciding* and *could there be a leader other
/// than me* are two questions. The first is about the role. The second is about
/// the catalog, and this is it.
#[must_use]
pub fn names_a_peer(declared: &[ReplicaDefinition], me: &[u8; NODE_ID_LEN]) -> bool {
    declared
        .iter()
        .any(|peer| peer.node.is_some_and(|node| node != *me))
}

/// Could a node other than this one accept a write?
///
/// The question the write fence actually asks, and it is **not**
/// [`names_a_peer`]. That one answers *is this store in a cluster*, which its
/// two other callers need — a joiner deciding whether its catalog can yet name
/// somebody to follow counts a read-only peer, and should.
///
/// A peer that carries neither [`Roles::WRITABLE`] nor [`Roles::COORDINATING`]
/// can do neither thing this fence exists to guard against. It cannot be
/// elected, because `driver::voters` will not ballot a row without
/// `COORDINATING` and a majority counted over members that cannot be asked is a
/// majority of a fiction; and it cannot write under somebody else's leadership,
/// because the row declares that it takes no writes.
///
/// # What asking the wider question cost
///
/// A store whose only declared peer was a read-only follower was fenced against
/// a leadership its own election machinery refuses to create, with no
/// configuration that recovers it: a lease is written only by a completed round,
/// no round is possible, and the store never writes again. That is the topology
/// ADR-0067 documents — one node that is already a cluster, and a second told
/// one address — so following the join procedure stopped the leader's writes on
/// its first statement (Q-609).
///
/// # Why it is not narrowed to `COORDINATING` alone
///
/// Because that is the hole ADR-0069 closed, arriving by a different road. Two
/// nodes declared `SERVING | WRITABLE` and neither `COORDINATING` can elect
/// nobody, so neither would be fenced — and both would write. Under the wider
/// reading they fence each other, which is a dead cluster an operator can see
/// rather than a silent divergence. **Unable to write** is the property, and it
/// takes both bits to be absent.
///
/// # A row that understates its peer
///
/// `roles` is what an operator declared, not what the peer has since become. A
/// row that understates a peer defeats this, and it defeats the election and the
/// forwarding lookup in exactly the same breath — they all read the same field,
/// so the deciding set is what the catalog says it is, and one wrong row is one
/// wrong answer rather than two that disagree.
#[must_use]
pub fn another_node_may_write(declared: &[ReplicaDefinition], me: &[u8; NODE_ID_LEN]) -> bool {
    declared.iter().any(|peer| {
        peer.node.is_some_and(|node| node != *me)
            && (peer.roles.has(Roles::WRITABLE) || peer.roles.has(Roles::COORDINATING))
    })
}

/// Which declared row, if any, the greeting of `node` binds itself to.
///
/// A row with no `node` is **declared but undiallable**: `Directory::greet_round`
/// skips it, because opening a session derives the peer's transport name from
/// its identifier and there is none to derive from. So a row nobody bound can
/// never be bound from this side, and the only event that can ever bind it is
/// that peer arriving here and proving who it is. The greeting supplies the
/// **id and nothing else** — the endpoint, the roles and the reach are what the
/// operator wrote, and a row that took its role from the wire would be a role
/// somebody else's first packet got to assign.
///
/// # Two refusals, and the second is the load-bearing one
///
/// Nothing is bound when a row **already names** `node`: that peer is known, and
/// binding a second row to one id would put one node in the catalog twice, where
/// the two rows can disagree about its endpoint and its roles.
///
/// Nothing is bound when **more than one** row is unbound either, and this is
/// the refusal that matters. An inbound connection carries no discriminator that
/// could choose between them — the source port is ephemeral and two peers on one
/// host share an address — so a binding taken among several would be a guess, and
/// the cost of guessing wrong is a node running under somebody else's roles at
/// somebody else's dial-back address. Declining leaves the operator with a row
/// they can bind by hand with `NODE`, which is a visible non-event rather than a
/// silent wrong answer.
///
/// One at a time is also the settled answer in this class of system: a cluster
/// that has been told about a member it has not yet heard from holds further
/// membership changes until it has, for exactly this reason. The comparison and
/// its source are recorded in ADR-0072.
#[must_use]
pub fn the_row_a_greeting_binds<'a>(
    declared: &'a [ReplicaDefinition],
    node: &[u8; NODE_ID_LEN],
) -> Option<&'a str> {
    if declared.iter().any(|row| row.node == Some(*node)) {
        return None;
    }
    let mut unbound = declared.iter().filter(|row| row.node.is_none());
    let candidate = unbound.next()?;
    if unbound.next().is_some() {
        return None;
    }
    Some(candidate.name.as_str())
}

#[cfg(test)]
mod tests {
    use super::{ReplicaDefinition, the_row_a_greeting_binds};
    use tessari_encoding::{NODE_ID_LEN, Roles};

    const GREETER: [u8; NODE_ID_LEN] = [9; NODE_ID_LEN];
    const SOMEBODY_ELSE: [u8; NODE_ID_LEN] = [7; NODE_ID_LEN];

    /// A row an operator declared with `NODE`.
    fn bound(name: &str, node: [u8; NODE_ID_LEN]) -> ReplicaDefinition {
        ReplicaDefinition {
            node: Some(node),
            ..unbound(name)
        }
    }

    /// A row an operator declared without `NODE`.
    ///
    /// The names differ per row throughout, so that a rule taking the wrong
    /// candidate is detectable rather than accidentally right.
    fn unbound(name: &str) -> ReplicaDefinition {
        ReplicaDefinition {
            name: name.to_owned(),
            endpoint: "10.0.0.2:9000".to_owned(),
            roles: Roles::SERVING,
            node: None,
            replicates: None,
        }
    }

    #[test]
    fn the_one_row_nobody_bound_is_the_row_a_greeting_binds() {
        let declared = [bound("leader", SOMEBODY_ELSE), unbound("joiner")];
        assert_eq!(
            the_row_a_greeting_binds(&declared, &GREETER),
            Some("joiner")
        );
    }

    #[test]
    fn an_empty_catalog_binds_nothing_because_there_is_no_row_to_bind() {
        assert_eq!(the_row_a_greeting_binds(&[], &GREETER), None);
    }

    #[test]
    fn a_catalog_whose_every_row_is_bound_binds_nothing() {
        let declared = [bound("leader", SOMEBODY_ELSE)];
        assert_eq!(the_row_a_greeting_binds(&declared, &GREETER), None);
    }

    /// The peer is already known, and knowing it twice is worse than once.
    ///
    /// Two rows for one id can disagree about that node's endpoint and its
    /// roles, and every reader of the catalog then gets whichever it reaches
    /// first. The unbound row beside it is what makes this a real test: without
    /// it there would be nothing to bind either way.
    #[test]
    fn a_greeting_from_a_node_a_row_already_names_binds_nothing() {
        let declared = [bound("leader", GREETER), unbound("joiner")];
        assert_eq!(the_row_a_greeting_binds(&declared, &GREETER), None);
    }

    /// The refusal the whole rule is built around.
    ///
    /// Nothing on an inbound connection can choose between two unbound rows —
    /// the source port is ephemeral and two peers on one host share an address
    /// — so binding either would be a guess, and the cost of guessing wrong is
    /// a node running under somebody else's roles at somebody else's dial-back
    /// address.
    #[test]
    fn two_rows_nobody_bound_bind_nothing_because_neither_can_be_chosen() {
        let declared = [unbound("joiner"), unbound("another")];
        assert_eq!(the_row_a_greeting_binds(&declared, &GREETER), None);
    }
}
