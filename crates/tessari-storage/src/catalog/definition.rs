//! What a catalog entry holds, and how it becomes a value.
//!
//! A definition is an ordinary [`Value::Object`], encoded by the payload codec
//! like any other record. The catalog therefore needs no encoding of its own,
//! and a field added to a definition later is an object field the codec already
//! knows how to carry.
//!
//! Every definition carries its own id even though the id is also its address.
//! That is deliberate redundancy in exactly one direction: a definition read out
//! of a scan knows what it is without its caller having to remember which key it
//! came from.

mod declarations;
mod engine;
mod events;
mod indexes;
mod kinds;
mod reading;
mod rollups;
mod tables;
use std::collections::BTreeMap;

use tessari_types::{
    Acknowledgement, DatabaseId, NamespaceId, Number, Replication, ReplicationClass, Value,
};

use super::ShardMap;
use crate::error::{Error, Result};
pub use declarations::{
    EdgeDeclaration, EdgeOrder, QueueDeclaration, SeriesDeclaration, VaultCustody,
    VaultDeclaration, VectorDeclaration, ViewDeclaration,
};
pub use engine::{EngineField, EngineMember, UNIT_WEIGHT};
pub use events::EventDeclaration;
pub use indexes::{IndexDefinition, IndexShape, SearchCosts, VectorDistance};
pub use kinds::{StoredKind, TableKind};
pub(crate) use reading::{
    ceiling, count_of, field_id, field_name, flag, id_of, identity_kind, object,
};
pub use rollups::{RollupCompute, RollupDeclaration, RollupFold};
pub use tables::{TableDefinition, TableShape};

const FIELD_ID: &str = "id";
const FIELD_NAME: &str = "name";
const FIELD_NAMESPACE: &str = "namespace";
const FIELD_DATABASE: &str = "database";
const FIELD_TABLE: &str = "table";
const FIELD_FIELDS: &str = "fields";
const FIELD_UNIQUE: &str = "unique";
const FIELD_SEARCH: &str = "search";
const FIELD_VECTOR: &str = "vector";
const FIELD_QUANTIZED: &str = "quantized";
const FIELD_MATERIALIZED: &str = "materialized";
const FIELD_SPATIAL: &str = "spatial";
const FIELD_POSITIONS: &str = "positions";
const FIELD_OFFSETS: &str = "offsets";
const FIELD_UNSCORED: &str = "unscored";
const FIELD_ENGINE: &str = "engine";
const FIELD_SCHEMAFULL: &str = "schemafull";
const FIELD_EDGE: &str = "edge";
const FIELD_BUCKET: &str = "bucket";
const FIELD_COLLECTION: &str = "collection";
const FIELD_GEO: &str = "geo";
const FIELD_VAULT: &str = "vault";
const FIELD_KEY_ID: &str = "key_id";
const FIELD_WRAPPED: &str = "wrapped";
const FIELD_ENDPOINTS: &str = "endpoints";
const FIELD_FROM: &str = "from";
const FIELD_TO: &str = "to";
const FIELD_ORDER: &str = "order";
const FIELD_DESCENDING: &str = "descending";
const FIELD_IDENTITY: &str = "identity";
const FIELD_GRAPH: &str = "graph";
const FIELD_CEILING: &str = "ceiling";
const FIELD_QUEUE: &str = "queue";
const FIELD_VIEW: &str = "view";
const FIELD_READ: &str = "read";
const FIELD_TIMEOUT: &str = "timeout";
const FIELD_ATTEMPTS: &str = "attempts";
const FIELD_SERIES: &str = "series";
/// A space's declaration: present (possibly empty) exactly when the table is one.
const FIELD_SPACE: &str = "space";
/// A topic's declaration: present (possibly empty) exactly when the table is one.
const FIELD_TOPIC: &str = "topic";
const FIELD_RETAIN: &str = "retain";
/// The field a series mints its identities from (ADR-0088 §1).
const FIELD_EVENT_TIME: &str = "time";
const FIELD_DIMENSION: &str = "dimension";
const FIELD_DISTANCE: &str = "distance";
const FIELD_REPLICATION: &str = "replication";
const FIELD_REPLICATION_CLASS: &str = "replication_class";
const FIELD_ACKNOWLEDGE: &str = "acknowledge";
const FIELD_CONFLICT: &str = "conflict";
const FIELD_SHARDS: &str = "shards";
const FIELD_PARTITION: &str = "partition";
const FIELD_EVENTS: &str = "events";

/// A namespace: the outermost tenancy level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespaceDefinition {
    /// The namespace's id.
    pub id: NamespaceId,
    /// Its name, which may change without moving anything.
    pub name: String,
    /// How many copies the cluster is asked to keep of it (ADR-0060).
    ///
    /// `None` is **never stated**, and every namespace in every store that
    /// existed before this field did reads that way — which is the correct
    /// reading and not a fallback, exactly as an absent epoch flag is epoch
    /// zero. It is deliberately not [`Replication::None`]: a namespace that
    /// declined replication is honoured, and a namespace nobody ever asked is
    /// refused at the moment a second node would hold it. That difference is
    /// unrecoverable once namespaces exist, which is why the field lands before
    /// the cluster does rather than with it.
    ///
    /// The counter-argument is on the record and it is `replica.rs`'s own: a
    /// field nothing reads is a decision taken with no way to find out it was
    /// wrong. It applies to a copy count held on a **replica**, where the value
    /// can be derived later from the peers that exist. It does not apply here,
    /// because what is bought is not the number — it is the distinction between
    /// silence and a stated answer, and silence cannot be reconstructed.
    pub replication: Option<Replication>,
    /// How many writers it admits — single-leader, or multi-master (G027 S2.1).
    ///
    /// `None` is **never stated** and reads as single-leader wherever it is
    /// asked, which is what every namespace written before this field did and
    /// what the engine has always enforced. It is kept apart from a stated
    /// [`ReplicationClass::SingleLeader`] for the reason its neighbour keeps
    /// its own two absences apart: an operator who considered the question and
    /// answered it has told the cluster something, and `INFO FOR` reports a
    /// decision differently from a silence.
    ///
    /// Separate from `replication` rather than folded into it because the two
    /// are independent — how many copies and how many writers — and a namespace
    /// declared multi-master still has a factor.
    pub class: Option<ReplicationClass>,
    /// How many copies must hold a write here before it is acknowledged, and
    /// whether a request may ask for fewer (ADR-0106 D2).
    ///
    /// `None` is **never stated**, kept apart from a stated level for its
    /// neighbours' reason: the level a write waits for when nobody said is
    /// derived from the replication (`MAJORITY` once more than one node holds
    /// the namespace), and reporting that derivation as a decision would make a
    /// configured namespace and an unconfigured one give the same answer.
    pub acknowledge: Option<Acknowledgement>,
}

/// A database within a namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseDefinition {
    /// The database's id.
    pub id: DatabaseId,
    /// The namespace it belongs to.
    pub namespace: NamespaceId,
    /// Its name, unique within that namespace.
    pub name: String,
}

impl NamespaceDefinition {
    /// The value written to the catalog.
    ///
    /// The replication field is written **only when it was stated**, so a
    /// namespace that said nothing encodes to the exact object it encoded to
    /// before this field existed. Nothing already stored is rewritten, and the
    /// absence carries the meaning instead of a placeholder standing in for it.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut fields = BTreeMap::from([
            (FIELD_ID.to_owned(), number(self.id.get())),
            (FIELD_NAME.to_owned(), Value::from(self.name.as_str())),
        ]);
        if let Some(replication) = self.replication {
            fields.insert(FIELD_REPLICATION.to_owned(), replication.to_value());
        }
        if let Some(class) = self.class {
            fields.insert(FIELD_REPLICATION_CLASS.to_owned(), class.to_value());
        }
        if let Some(acknowledge) = self.acknowledge {
            fields.insert(FIELD_ACKNOWLEDGE.to_owned(), acknowledge.to_value());
        }
        Value::Object(fields)
    }

    /// Read a definition back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when a field is missing or holds the
    /// wrong type — including a replication policy this build does not
    /// recognise, which refuses rather than reading as never-stated. A
    /// namespace written by a later build under a placement policy must not be
    /// served here as though nobody had ever declared one.
    pub fn from_value(value: &Value) -> Result<Self> {
        let fields = object(value, "namespace")?;
        let replication = match fields.get(FIELD_REPLICATION) {
            None => None,
            Some(held) => Some(
                Replication::from_value(held).ok_or(Error::CatalogMalformed {
                    entity: "namespace",
                    field: FIELD_REPLICATION,
                    found: "a replication policy this build does not have",
                })?,
            ),
        };
        let class = match fields.get(FIELD_REPLICATION_CLASS) {
            None => None,
            Some(held) => Some(ReplicationClass::from_value(held).ok_or(
                Error::CatalogMalformed {
                    entity: "namespace",
                    field: FIELD_REPLICATION_CLASS,
                    found: "a replication class this build does not have",
                },
            )?),
        };
        // Refused rather than read as unstated, for `class`'s reason: a level a
        // later build wrote, read as nothing here, would acknowledge writes a
        // failover can lose on a namespace that asked for more.
        let acknowledge = match fields.get(FIELD_ACKNOWLEDGE) {
            None => None,
            Some(held) => Some(Acknowledgement::from_value(held).ok_or(
                Error::CatalogMalformed {
                    entity: "namespace",
                    field: FIELD_ACKNOWLEDGE,
                    found: "an acknowledgement level this build does not have",
                },
            )?),
        };
        Ok(Self {
            id: NamespaceId::new(field_id(fields, FIELD_ID, "namespace")?),
            name: field_name(fields, "namespace")?,
            replication,
            class,
            acknowledge,
        })
    }
}

impl DatabaseDefinition {
    /// The value written to the catalog.
    #[must_use]
    pub fn to_value(&self) -> Value {
        Value::Object(BTreeMap::from([
            (FIELD_ID.to_owned(), number(self.id.get())),
            (FIELD_NAMESPACE.to_owned(), number(self.namespace.get())),
            (FIELD_NAME.to_owned(), Value::from(self.name.as_str())),
        ]))
    }

    /// Read a definition back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when a field is missing or holds the
    /// wrong type.
    pub fn from_value(value: &Value) -> Result<Self> {
        let fields = object(value, "database")?;
        Ok(Self {
            id: DatabaseId::new(field_id(fields, FIELD_ID, "database")?),
            namespace: NamespaceId::new(field_id(fields, FIELD_NAMESPACE, "database")?),
            name: field_name(fields, "database")?,
        })
    }
}

/// What a queue calls the instant a record's current hold lapses.
///
/// Visible and ordinary, so `SELECT` can answer *what is held and until when* —
/// which is the most common thing anybody does with a queue that has gone quiet.
/// It could instead have been hidden behind a byte no identifier can spell, the
/// way a bucket's chunk table is, and that was rejected for exactly that reason:
/// a queue whose state cannot be read is a queue nobody can debug.
///
/// The cost of being visible is that a payload field of this name collides, and
/// the collision is refused at the write naming the field rather than absorbed
/// silently (Q-461).
///
/// A lapsed record is **not** rewritten — nothing sweeps a passed deadline away
/// — so a reader asking for unclaimed records compares rather than testing for
/// absence: `claimed_until IS NONE OR claimed_until < time::now()`.
pub const QUEUE_CLAIMED_UNTIL: &str = "claimed_until";

/// What a queue calls who is holding a record.
///
/// An object of two routes — `consumer`, the name a session declared, and
/// `instance`, the value the engine minted for that session — because the pair
/// is **one fact**: who took this. Two flat fields would be two payload
/// collision surfaces where one will do, and a route into a stored object is
/// already how a condition reaches a nested value.
///
/// **Absent when the session never said who it was.** A claim from an
/// undeclared session writes nothing here, which keeps every existing caller
/// working unchanged and makes the absence mean something true — *nobody said*
/// — rather than a default that is itself a claim.
///
/// Visible and ordinary for [`QUEUE_CLAIMED_UNTIL`]'s reason, and here the
/// reason is sharper: *who has this* is the first question asked of a queue that
/// has gone quiet, and it is the one question the engine could not answer at all
/// until this field existed.
///
/// The instance is **not** derived from a sign-in ticket and never shares its
/// value. A ticket is a credential; this is read by anyone who may `SELECT` the
/// queue, and one value serving both would publish the first to everybody
/// holding the second.
pub const QUEUE_CLAIMED_BY: &str = "claimed_by";

/// A route inside [`QUEUE_CLAIMED_BY`]: the name the session declared.
pub const CLAIMED_BY_CONSUMER: &str = "consumer";

/// A route inside [`QUEUE_CLAIMED_BY`]: the value the engine minted.
pub const CLAIMED_BY_INSTANCE: &str = "instance";

/// What a queue calls the number of times a record has been handed out.
///
/// Counted at the hand-out and not at a failure, because how many times a record
/// was handed out is a fact the store can observe, while how many times the work
/// failed is a fact only the worker holds — and a count the store cannot verify
/// is a count that will eventually be wrong.
pub const QUEUE_ATTEMPTS: &str = "attempts";

/// What a vector store calls the field its vectors are in.
///
/// Fixed rather than named in the declaration, because a store whose vector
/// field could be called anything is a store every reader has to look up before
/// writing to it — and the statement already says the whole of what the field
/// is. It is the same name as the store's index, which does not collide: fields
/// and indexes are separate namespaces.
pub const VECTOR_FIELD: &str = "vector";

/// What a geo store calls the field its geometries are in.
///
/// Fixed for the reason [`VECTOR_FIELD`] is, and named after the **type** rather
/// than after the statement's word. In the vector store those coincide — the
/// word, the field and the type are all `vector` — and here they cannot, since
/// the word is `GEO` and the type is `geometry`. The rule that settles it is the
/// one the vector field states: the name is the whole of what the field is, and
/// what it is is a geometry.
pub const GEO_FIELD: &str = "geometry";

/// An identifier as it is stored: an integer, widened rather than cast.
pub(crate) fn number(id: u32) -> Value {
    Value::Number(Number::Integer(i64::from(id)))
}

/// A byte count as it is stored.
///
/// Saturating rather than fallible, and the saturation is unreachable: the only
/// way a ceiling enters the catalog is a `MAX n` literal, and this language's
/// whole numbers are `i64`, so a value past `i64::MAX` has no spelling. A
/// `Result` here would be a branch no input can take.
fn byte_count(value: u64) -> Value {
    Value::Number(Number::Integer(i64::try_from(value).unwrap_or(i64::MAX)))
}

/// A record counter as it is stored.
///
/// The ceiling belongs to the key grammar rather than to this function: a record
/// identity is a [`tessari_types::RecordId::Int`], an `i64`, so a count past
/// `i64::MAX` could be held here and could never be spent. It is refused where
/// it is produced, which is the only place the refusal can still say something
/// useful.
pub(crate) fn count(value: u64) -> Result<Value> {
    let held = i64::try_from(value).map_err(|_| Error::IdSpaceExhausted {
        level: RECORD_LEVEL,
    })?;
    Ok(Value::Number(Number::Integer(held)))
}

/// The level a record counter reports when it runs out.
///
/// Public because the caller that turns a counter into a
/// [`tessari_types::RecordId`] narrows a `u64` to an `i64` to do it, and must
/// name the same level in the refusal. That narrowing cannot fail — [`count`]
/// refuses to store a number past `i64::MAX`, so a number this store answered
/// is a number it can spend — but "cannot fail" is not something to write an
/// `unwrap` on, and the refusal it would need already exists here.
pub const RECORD_LEVEL: &str = "record";

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use tessari_types::{GraphId, IdentityKind, IndexId, Path, TableId};

    /// The bytes a namespace held before the replication clause existed.
    ///
    /// Pinned as a literal rather than produced by an encoder, so that this is
    /// genuinely a stored value from an older build and not a round trip of
    /// today's. A namespace written then must read as **never stated** — which
    /// is the true reading of a store that had no way to say anything, not a
    /// fallback — and must encode back to exactly the same object, so nothing
    /// already on disk is rewritten by being read.
    #[test]
    fn a_namespace_stored_before_the_clause_reads_as_never_stated() {
        let stored = Value::Object(BTreeMap::from([
            ("id".to_owned(), number(7)),
            ("name".to_owned(), Value::from("prod")),
        ]));
        let read = NamespaceDefinition::from_value(&stored).unwrap();
        assert_eq!(read.replication, None);
        assert_eq!(read.to_value(), stored, "a read must not rewrite it");
    }

    #[test]
    fn a_stated_class_round_trips_and_a_definition_without_one_reads_as_silence() {
        // G027 S2.1. The second half is the one that matters on disk: a
        // namespace written before this field existed must still decode, and
        // must decode as *never stated* rather than as either answer — the same
        // property the replication clause bought, and the same reason nothing
        // stored is rewritten.
        for class in [
            ReplicationClass::SingleLeader,
            ReplicationClass::MultiMaster,
        ] {
            let namespace = NamespaceDefinition {
                id: NamespaceId::new(7),
                name: "prod".to_owned(),
                replication: None,
                class: Some(class),
                acknowledge: None,
            };
            let read = NamespaceDefinition::from_value(&namespace.to_value()).unwrap();
            assert_eq!(read, namespace, "{class}");
        }

        // Pinned as a literal for its neighbour's reason: this is a namespace a
        // build without the class field wrote, not a round trip of today's, and
        // it must read as never stated and encode back unchanged.
        let stored = Value::Object(BTreeMap::from([
            ("id".to_owned(), number(7)),
            ("name".to_owned(), Value::from("prod")),
        ]));
        let read = NamespaceDefinition::from_value(&stored).unwrap();
        assert_eq!(read.class, None);
        assert_eq!(read.to_value(), stored, "a read must not rewrite it");
    }

    #[test]
    fn a_stated_policy_round_trips_and_is_not_silence() {
        for policy in [
            Replication::None,
            Replication::Factor(core::num::NonZeroU32::new(3).unwrap()),
        ] {
            let namespace = NamespaceDefinition {
                id: NamespaceId::new(7),
                name: "prod".to_owned(),
                replication: Some(policy),
                class: None,
                acknowledge: None,
            };
            let read = NamespaceDefinition::from_value(&namespace.to_value()).unwrap();
            assert_eq!(read, namespace, "{policy}");
            assert_ne!(read.replication, None, "{policy}");
        }
    }

    /// A policy a later build understands and this one does not refuses rather
    /// than reading as silence — the same refusal a table's unknown identity
    /// scheme takes, and for the same reason: serving a namespace as *nobody
    /// ever declared one* when somebody did is the wrong answer given
    /// confidently.
    #[test]
    fn a_policy_this_build_does_not_know_refuses() {
        let stored = Value::Object(BTreeMap::from([
            ("id".to_owned(), number(7)),
            ("name".to_owned(), Value::from("prod")),
            ("replication".to_owned(), Value::from("every-rack")),
        ]));
        let error = NamespaceDefinition::from_value(&stored).unwrap_err();
        assert!(
            matches!(
                error,
                Error::CatalogMalformed {
                    field: "replication",
                    ..
                }
            ),
            "{error}"
        );
    }

    #[test]
    fn every_definition_round_trips_through_its_value() {
        let namespace = NamespaceDefinition {
            id: NamespaceId::new(7),
            name: "prod".to_owned(),
            replication: None,
            class: None,
            acknowledge: None,
        };
        assert_eq!(
            NamespaceDefinition::from_value(&namespace.to_value()).unwrap(),
            namespace
        );

        let database = DatabaseDefinition {
            id: DatabaseId::new(3),
            namespace: NamespaceId::new(7),
            name: "orders".to_owned(),
        };
        assert_eq!(
            DatabaseDefinition::from_value(&database.to_value()).unwrap(),
            database
        );

        let table = TableDefinition {
            id: TableId::new(11),
            namespace: NamespaceId::new(7),
            database: DatabaseId::new(3),
            name: "line_items".to_owned(),
            schemafull: true,
            graph: Some(GraphId::new(9)),
            // Deliberately not the default here either, for the same reason the
            // identity below is not: a kind that round trips through three flags
            // is only proven by a kind that is not the one an absent flag gives.
            // The ceiling is present for that same reason once more — an absent
            // one round trips through a field that was never written, which
            // proves the default rather than the encoding.
            kind: TableKind::Bucket(Some(5 * 1024 * 1024)),
            // Deliberately not the default: a field that never travels round
            // trips perfectly as long as both ends agree on what it is when
            // absent, which is exactly the bug this assertion is for.
            identity: IdentityKind::Uuid,
            conflict: None,
            shards: None,
            partition: None,
            // Not the default either, for the identity's reason: an empty list
            // round trips through a field that was never written.
            events: vec![super::EventDeclaration {
                name: "audit".to_owned(),
                on: vec![tessari_types::WriteKind::Update],
                when: Some("$after.v > 1".to_owned()),
                body: "CREATE log = { v: $after.v }".to_owned(),
            }],
        };
        assert_eq!(
            TableDefinition::from_value(&table.to_value()).unwrap(),
            table
        );
    }

    #[test]
    fn a_table_entry_written_before_identities_were_declared_names_records_with_a_counter() {
        // Every table already in a store predates the field, and each one is
        // already naming records with a counter. Reading them as anything else
        // would rename what the *next* record in them is called.
        let fields = BTreeMap::from([
            (FIELD_ID.to_owned(), number(11)),
            (FIELD_NAMESPACE.to_owned(), number(7)),
            (FIELD_DATABASE.to_owned(), number(3)),
            (FIELD_NAME.to_owned(), Value::from("line_items")),
        ]);
        let read = TableDefinition::from_value(&Value::Object(fields)).unwrap();
        assert_eq!(read.identity, IdentityKind::Int);
    }

    #[test]
    fn a_naming_scheme_this_build_does_not_know_is_refused_rather_than_read_as_the_default() {
        // The asymmetry with the test above is the whole point. *Absent* means a
        // store written before the field existed, and its answer is knowable.
        // *Present and unrecognised* means a store written by a later build, and
        // the one thing that must not happen is this build deciding the table
        // uses the scheme it happens to prefer and minting ids under it.
        let fields = BTreeMap::from([
            (FIELD_ID.to_owned(), number(11)),
            (FIELD_NAMESPACE.to_owned(), number(7)),
            (FIELD_DATABASE.to_owned(), number(3)),
            (FIELD_NAME.to_owned(), Value::from("line_items")),
            (FIELD_IDENTITY.to_owned(), Value::from("ulid")),
        ]);
        let error = TableDefinition::from_value(&Value::Object(fields)).unwrap_err();
        assert_eq!(error.code(), "corruption");

        // And a scheme that is not even a word is refused for the same reason
        // rather than falling through a `match` on the string.
        let fields = BTreeMap::from([
            (FIELD_ID.to_owned(), number(11)),
            (FIELD_NAMESPACE.to_owned(), number(7)),
            (FIELD_DATABASE.to_owned(), number(3)),
            (FIELD_NAME.to_owned(), Value::from("line_items")),
            (FIELD_IDENTITY.to_owned(), Value::Bool(true)),
        ]);
        let error = TableDefinition::from_value(&Value::Object(fields)).unwrap_err();
        assert_eq!(error.code(), "corruption");
    }

    #[test]
    fn a_table_entry_written_before_schemas_existed_reads_as_schemaless() {
        let fields = BTreeMap::from([
            (FIELD_ID.to_owned(), number(11)),
            (FIELD_NAMESPACE.to_owned(), number(7)),
            (FIELD_DATABASE.to_owned(), number(3)),
            (FIELD_NAME.to_owned(), Value::from("line_items")),
        ]);
        let read = TableDefinition::from_value(&Value::Object(fields)).unwrap();
        assert!(!read.schemafull);
    }

    #[test]
    fn a_schemafull_flag_that_is_not_a_boolean_is_refused_rather_than_read_as_false() {
        let fields = BTreeMap::from([
            (FIELD_ID.to_owned(), number(11)),
            (FIELD_NAMESPACE.to_owned(), number(7)),
            (FIELD_DATABASE.to_owned(), number(3)),
            (FIELD_NAME.to_owned(), Value::from("line_items")),
            (FIELD_SCHEMAFULL.to_owned(), Value::from("yes")),
        ]);
        let error = TableDefinition::from_value(&Value::Object(fields)).unwrap_err();
        assert_eq!(error.code(), "corruption");
    }

    #[test]
    fn a_stored_table_claiming_two_kinds_is_refused_rather_than_read_as_one_of_them() {
        // In memory a table has one kind and a second one cannot be spelled. On
        // disk it is still three separate booleans, so the pair *is* writable —
        // by a build that predates the kind, or by corruption — and the decoder
        // is the only place left that can refuse it. Picking a winner here would
        // be the worse failure: the table would read as an edge on one replica
        // and a bucket on another, from bytes both agree on.
        let fields = BTreeMap::from([
            (FIELD_ID.to_owned(), number(11)),
            (FIELD_NAMESPACE.to_owned(), number(7)),
            (FIELD_DATABASE.to_owned(), number(3)),
            (FIELD_NAME.to_owned(), Value::from("line_items")),
            (FIELD_EDGE.to_owned(), Value::Bool(true)),
            (FIELD_BUCKET.to_owned(), Value::Bool(true)),
        ]);
        let error = TableDefinition::from_value(&Value::Object(fields)).unwrap_err();
        assert_eq!(error.code(), "corruption");
        let text = error.to_string();
        assert!(text.contains("kind"), "{text}");
    }

    #[test]
    fn an_edge_table_round_trips_its_endpoints_and_the_order_its_edges_are_held_in() {
        let table = TableDefinition {
            id: TableId::new(11),
            namespace: NamespaceId::new(7),
            database: DatabaseId::new(3),
            name: "follows".to_owned(),
            schemafull: false,
            graph: None,
            shards: None,
            partition: None,
            events: Vec::new(),
            kind: TableKind::Edge(Some(EdgeDeclaration {
                from: TableId::new(4),
                to: TableId::new(5),
                // Deliberately descending, and deliberately not the same table at
                // both ends: an order that round trips as `false` and endpoints
                // that round trip transposed both survive an assertion made with
                // the defaults.
                order: Some(EdgeOrder {
                    field: "at".to_owned(),
                    descending: true,
                }),
            })),
            identity: IdentityKind::Int,
            conflict: None,
        };
        assert_eq!(
            TableDefinition::from_value(&table.to_value()).unwrap(),
            table
        );

        // And a declared pair with no declared order is a different value, not a
        // missing one — it reads back unordered rather than as the default order.
        let unordered = TableDefinition {
            kind: TableKind::Edge(Some(EdgeDeclaration {
                from: TableId::new(4),
                to: TableId::new(5),
                order: None,
            })),
            ..table.clone()
        };
        assert_eq!(
            TableDefinition::from_value(&unordered.to_value()).unwrap(),
            unordered
        );

        // And the bare `EDGE`, which is what every edge table declared before the
        // clause existed is: no endpoints field is written, and the entry reads
        // back permissive rather than as a pair nobody declared.
        let permissive = TableDefinition {
            kind: TableKind::Edge(None),
            ..table
        };
        assert_eq!(
            TableDefinition::from_value(&permissive.to_value()).unwrap(),
            permissive
        );
    }

    #[test]
    fn a_vector_store_round_trips_its_width_and_its_distance() {
        let table = TableDefinition {
            id: TableId::new(11),
            namespace: NamespaceId::new(7),
            database: DatabaseId::new(3),
            name: "embeddings".to_owned(),
            schemafull: false,
            graph: None,
            shards: None,
            partition: None,
            events: Vec::new(),
            // Deliberately the second distance rather than the first: a store
            // that round tripped as `cosine` whatever it was declared with
            // survives an assertion made with the default.
            kind: TableKind::Vector(VectorDeclaration {
                dimension: 768,
                distance: VectorDistance::Euclidean,
            }),
            identity: IdentityKind::Int,
            conflict: None,
        };
        assert_eq!(
            TableDefinition::from_value(&table.to_value()).unwrap(),
            table
        );
    }

    #[test]
    fn a_stored_vector_store_naming_a_distance_this_build_has_not_is_refused() {
        // Refused rather than read as `cosine`, on the reasoning `identity_kind`
        // records for an unknown word: the entry describes a store already
        // searched some other way, and answering its nearest-neighbour question
        // from a graph built for a different geometry returns plausible
        // neighbours that are not the nearest — with nothing in an error state.
        let fields = BTreeMap::from([
            (FIELD_ID.to_owned(), number(11)),
            (FIELD_NAMESPACE.to_owned(), number(7)),
            (FIELD_DATABASE.to_owned(), number(3)),
            (FIELD_NAME.to_owned(), Value::from("embeddings")),
            (
                FIELD_VECTOR.to_owned(),
                Value::Object(BTreeMap::from([
                    (FIELD_DIMENSION.to_owned(), number(768)),
                    (FIELD_DISTANCE.to_owned(), Value::from("manhattan")),
                ])),
            ),
        ]);
        let error = TableDefinition::from_value(&Value::Object(fields)).unwrap_err();
        assert_eq!(error.code(), "corruption");
    }

    #[test]
    fn a_stored_vector_store_that_also_claims_a_flag_is_refused() {
        // The same refusal the three flags already get, extended to the kind
        // that is carried by a declaration instead of by a flag: on disk both
        // are writable side by side, and picking a winner would make one replica
        // read a bucket where another reads a vector store, from bytes they
        // agree on.
        let fields = BTreeMap::from([
            (FIELD_ID.to_owned(), number(11)),
            (FIELD_NAMESPACE.to_owned(), number(7)),
            (FIELD_DATABASE.to_owned(), number(3)),
            (FIELD_NAME.to_owned(), Value::from("embeddings")),
            (FIELD_BUCKET.to_owned(), Value::Bool(true)),
            (
                FIELD_VECTOR.to_owned(),
                Value::Object(BTreeMap::from([
                    (FIELD_DIMENSION.to_owned(), number(768)),
                    (FIELD_DISTANCE.to_owned(), Value::from("cosine")),
                ])),
            ),
        ]);
        let error = TableDefinition::from_value(&Value::Object(fields)).unwrap_err();
        assert_eq!(error.code(), "corruption");
        let text = error.to_string();
        assert!(text.contains("kind"), "{text}");
    }

    #[test]
    fn stored_endpoints_with_a_direction_but_no_field_are_refused_rather_than_read_as_unordered() {
        // The order is the endpoint index's key suffix, so reading a half-written
        // order as "no order" would answer a bounded neighbour read from an index
        // that does not hold the order it is being asked for — in the right
        // sequence often enough, by accident, to look correct.
        let endpoints = BTreeMap::from([
            (FIELD_FROM.to_owned(), number(4)),
            (FIELD_TO.to_owned(), number(5)),
            (FIELD_DESCENDING.to_owned(), Value::Bool(true)),
        ]);
        let fields = BTreeMap::from([
            (FIELD_ID.to_owned(), number(11)),
            (FIELD_NAMESPACE.to_owned(), number(7)),
            (FIELD_DATABASE.to_owned(), number(3)),
            (FIELD_NAME.to_owned(), Value::from("follows")),
            (FIELD_EDGE.to_owned(), Value::Bool(true)),
            (FIELD_ENDPOINTS.to_owned(), Value::Object(endpoints)),
        ]);
        let error = TableDefinition::from_value(&Value::Object(fields)).unwrap_err();
        assert_eq!(error.code(), "corruption");
        let text = error.to_string();
        assert!(text.contains("order"), "{text}");
    }

    #[test]
    fn a_stored_pair_on_a_table_that_is_not_an_edge_table_is_refused() {
        // The pair only means anything on an edge table, and this is the state
        // the kind exists to keep unrepresentable in memory — but disk is still
        // writable by an older build or by corruption. Reading it as a plain
        // table would silently discard a declared refusal, and reading it as an
        // edge table would give one replica a declared pair where another has
        // a table, from bytes both agree on.
        let endpoints = BTreeMap::from([
            (FIELD_FROM.to_owned(), number(4)),
            (FIELD_TO.to_owned(), number(5)),
        ]);
        let fields = BTreeMap::from([
            (FIELD_ID.to_owned(), number(11)),
            (FIELD_NAMESPACE.to_owned(), number(7)),
            (FIELD_DATABASE.to_owned(), number(3)),
            (FIELD_NAME.to_owned(), Value::from("follows")),
            (FIELD_EDGE.to_owned(), Value::Bool(false)),
            (FIELD_ENDPOINTS.to_owned(), Value::Object(endpoints)),
        ]);
        let error = TableDefinition::from_value(&Value::Object(fields)).unwrap_err();
        assert_eq!(error.code(), "corruption");
    }

    #[test]
    fn a_definition_missing_a_field_is_refused_and_names_it() {
        let fields = BTreeMap::from([(FIELD_ID.to_owned(), number(1))]);
        let error = TableDefinition::from_value(&Value::Object(fields)).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("namespace"), "{text}");
    }

    #[test]
    fn an_index_definition_round_trips_a_route_through_the_text_it_is_stored_as() {
        // The catalog stores a path as its spelling, so a path that read back as
        // a different route would index one value and filter another — and both
        // sides would look right in isolation.
        let index = IndexDefinition {
            id: IndexId::new(2),
            namespace: NamespaceId::new(7),
            database: DatabaseId::new(3),
            table: TableId::new(11),
            name: "by_home_city".to_owned(),
            fields: vec![
                Path::parse("address.city").expect("a path"),
                Path::parse("tags[0].name").expect("a path"),
                Path::field("email"),
            ],
            unique: false,
            search: false,
            spatial: false,
            quantized: false,
            vector: None,
            costs: crate::catalog::SearchCosts::default(),
            engine: None,
        };
        assert_eq!(
            IndexDefinition::from_value(&index.to_value()).unwrap(),
            index
        );
    }

    #[test]
    fn an_index_entry_written_before_paths_existed_reads_as_a_single_field() {
        // Every definition on disk today spells one plain field name, and a plain
        // field name is a path of one step. Nothing has to be migrated.
        let fields = BTreeMap::from([
            (FIELD_ID.to_owned(), number(2)),
            (FIELD_NAMESPACE.to_owned(), number(7)),
            (FIELD_DATABASE.to_owned(), number(3)),
            (FIELD_TABLE.to_owned(), number(11)),
            (FIELD_NAME.to_owned(), Value::from("by_email")),
            (
                FIELD_FIELDS.to_owned(),
                Value::Array(vec![Value::from("email")]),
            ),
            (FIELD_UNIQUE.to_owned(), Value::Bool(true)),
        ]);
        let read = IndexDefinition::from_value(&Value::Object(fields)).unwrap();
        assert_eq!(read.fields, vec![Path::field("email")]);
    }

    #[test]
    fn a_stored_route_that_is_not_a_route_is_corruption_rather_than_a_bad_request() {
        // Nothing that reached the catalog could have been an unreadable path:
        // the parser produced it, and the parser cannot spell one. So finding one
        // means the bytes changed underneath, which is a different failure from a
        // caller asking for something impossible.
        let fields = BTreeMap::from([
            (FIELD_ID.to_owned(), number(2)),
            (FIELD_NAMESPACE.to_owned(), number(7)),
            (FIELD_DATABASE.to_owned(), number(3)),
            (FIELD_TABLE.to_owned(), number(11)),
            (FIELD_NAME.to_owned(), Value::from("by_broken")),
            (
                FIELD_FIELDS.to_owned(),
                Value::Array(vec![Value::from("address..city")]),
            ),
            (FIELD_UNIQUE.to_owned(), Value::Bool(false)),
        ]);
        let error = IndexDefinition::from_value(&Value::Object(fields)).unwrap_err();
        assert_eq!(error.code(), "corruption");
    }

    #[test]
    fn a_definition_that_is_not_an_object_is_refused() {
        let error = NamespaceDefinition::from_value(&Value::from("prod")).unwrap_err();
        assert_eq!(error.code(), "corruption");
    }

    #[test]
    fn a_record_count_round_trips_through_the_value_it_is_stored_as() {
        let stored = count(9_001).unwrap();
        assert_eq!(count_of(&stored, "record sequence", "next").unwrap(), 9_001);
    }

    #[test]
    fn a_record_count_the_key_grammar_could_never_spend_is_refused_where_it_is_produced() {
        // A record identity is an `i64`. A count past that could be held here
        // and could never become an identity, so the refusal belongs at the
        // write rather than at the read that would have to explain it.
        let error = count(u64::MAX).unwrap_err();
        assert!(matches!(error, Error::IdSpaceExhausted { .. }), "{error}");
    }

    #[test]
    fn a_stored_record_count_that_is_negative_is_refused_rather_than_wrapped() {
        // Nothing writes one, so one being present means the record is not what
        // this build takes it for — and wrapping would hand the table an
        // identity space it has already spent.
        let error = count_of(
            &Value::Number(Number::Integer(-1)),
            "record sequence",
            "next",
        )
        .unwrap_err();
        assert_eq!(error.code(), "corruption");
    }

    #[test]
    fn an_id_too_large_for_the_identifier_width_is_refused_rather_than_truncated() {
        let fields = BTreeMap::from([
            (
                FIELD_ID.to_owned(),
                Value::Number(Number::Integer(i64::from(u32::MAX) + 1)),
            ),
            (FIELD_NAME.to_owned(), Value::from("x")),
        ]);
        assert!(NamespaceDefinition::from_value(&Value::Object(fields)).is_err());
    }
}
