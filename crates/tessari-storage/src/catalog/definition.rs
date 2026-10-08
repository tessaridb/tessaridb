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

mod auto_split;
mod declarations;
mod engine;
mod events;
mod expiry;
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
pub use auto_split::AutoSplit;
pub use declarations::{
    EdgeDeclaration, EdgeOrder, QueueDeclaration, SeriesDeclaration, VaultCustody,
    VaultDeclaration, VectorDeclaration, ViewDeclaration,
};
pub use engine::{EngineField, EngineMember, UNIT_WEIGHT};
pub use events::EventDeclaration;
pub use expiry::TableExpiry;
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
/// A database's `DEFINE PARAM` values (ADR-0124 D2).
const FIELD_PARAMS: &str = "params";
const FIELD_DATABASE: &str = "database";
const FIELD_TABLE: &str = "table";
const FIELD_FIELDS: &str = "fields";
const FIELD_UNIQUE: &str = "unique";
const FIELD_SEARCH: &str = "search";
const FIELD_VECTOR: &str = "vector";
const FIELD_QUANTIZED: &str = "quantized";
const FIELD_TOKENIZER: &str = "tokenizer";
const FIELD_MATERIALIZED: &str = "materialized";
const FIELD_SPATIAL: &str = "spatial";
const FIELD_CONTAINMENT: &str = "contains";
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
const FIELD_PRIORITY: &str = "priority";
const FIELD_NOT_BEFORE: &str = "not_before";
const FIELD_DEDUPLICATE: &str = "deduplicate";
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
const FIELD_SPREAD: &str = "spread";
const FIELD_AUTO_SPLIT: &str = "auto_split";
const FIELD_EXPIRE: &str = "expire";
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
    /// `DEFINE PARAM` values, by name without the marker (ADR-0124 D2).
    ///
    /// Written **only when there is one**, so a database without params
    /// encodes to the exact object it encoded to before this field existed.
    pub params: BTreeMap<String, Value>,
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
        let mut fields = BTreeMap::from([
            (FIELD_ID.to_owned(), number(self.id.get())),
            (FIELD_NAMESPACE.to_owned(), number(self.namespace.get())),
            (FIELD_NAME.to_owned(), Value::from(self.name.as_str())),
        ]);
        if !self.params.is_empty() {
            fields.insert(FIELD_PARAMS.to_owned(), Value::Object(self.params.clone()));
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
        let fields = object(value, "database")?;
        Ok(Self {
            id: DatabaseId::new(field_id(fields, FIELD_ID, "database")?),
            namespace: NamespaceId::new(field_id(fields, FIELD_NAMESPACE, "database")?),
            name: field_name(fields, "database")?,
            params: match fields.get(FIELD_PARAMS) {
                Some(Value::Object(params)) => params.clone(),
                None => BTreeMap::new(),
                Some(other) => {
                    return Err(Error::CatalogMalformed {
                        entity: "database",
                        field: FIELD_PARAMS,
                        found: other.type_name(),
                    });
                }
            },
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
mod tests;
