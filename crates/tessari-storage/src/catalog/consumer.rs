//! The stream consumers this store has been told to run.
//!
//! A consumer is a catalog entry like a replica: an ordinary record in the
//! system tenancy (ADR-0009), so declaring one takes part in the transaction
//! that issued it and reaches every node through the same apply path.
//!
//! # Why the declaration replicates and the position does not
//!
//! ADR-0018's replay test decides it, and it is sharp: *what happens when this
//! is replayed on another machine — does that machine become confused about
//! which one it is?*
//!
//! A **declaration** replayed elsewhere makes that node start the same consumer
//! in the same group, and the broker then spreads the topic's partitions across
//! the two of them. That is the wanted behaviour, and it is also what stops the
//! failure a clustered version of this feature documents about itself: nodes
//! disagreeing about a consumer's settings leave some partitions read twice and
//! others read by nobody. A declaration arriving through the log cannot disagree
//! with itself.
//!
//! A **position** replayed elsewhere would tell that node it had already read
//! messages it has never seen. So which partitions this process holds, where it
//! had reached, and whether it is running at all stay local — they live beside
//! the node's own identity, and nothing here writes them.
//!
//! # What a consumer refuses to promise
//!
//! Two commits happen into two different systems — this store, and the broker's
//! offset — and no transaction spans both. The order is the decision: the store
//! commit comes **first**, which chooses duplicates over loss, because a
//! duplicate is what a record identity can absorb and a loss is not.
//!
//! So this is at-least-once with idempotent application by record identity, it
//! is not exactly-once, and [`ConsumerDefinition`] carries the mapping that makes
//! the idempotence real rather than hoped for.

use std::collections::BTreeMap;

use tessari_encoding::decode_payload;
use tessari_types::{DatabaseId, NamespaceId, RecordId, TableId, Value};

use super::definition::{field_id, field_name, number, object};
use super::{Catalog, Level, id_key, qualify, system};
use crate::error::{Error, Result};

const FIELD_ID: &str = "id";
const FIELD_NAME: &str = "name";
const FIELD_BROKERS: &str = "brokers";
const FIELD_TOPIC: &str = "topic";
const FIELD_GROUP: &str = "group";
const FIELD_FORMAT: &str = "format";
const FIELD_IDENTITY: &str = "identity";
const FIELD_MAPPING: &str = "mapping";
const FIELD_NAMESPACE: &str = "namespace";
const FIELD_DATABASE: &str = "database";
const FIELD_TABLE: &str = "table";
const FIELD_ON_FAILURE: &str = "on_failure";
const FIELD_PARALLELISM: &str = "parallelism";
const FIELD_DECLARER: &str = "declarer";

const ENTITY: &str = "consumer";

/// What a consumer does with a message it cannot apply.
///
/// Stored as the word rather than as a number, so a definition read by an
/// operator says what it does. Two values and no third: a skip mode would let
/// data loss be chosen by whoever did not type the clause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnFailure {
    /// Halt the consumer and record why.
    Stop,
    /// Retry, then park the payload where the language can find it.
    Quarantine,
}

impl OnFailure {
    /// The word this is written and stored as.
    #[must_use]
    pub const fn spelling(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Quarantine => "quarantine",
        }
    }

    /// Read one back from the word it was stored as.
    #[must_use]
    pub fn parse(word: &str) -> Option<Self> {
        match word {
            "stop" => Some(Self::Stop),
            "quarantine" => Some(Self::Quarantine),
            _ => None,
        }
    }
}

/// One message field, and what the record calls it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mapped {
    /// Where to read it in the message, as a dotted route.
    pub from: String,
    /// What it is called in the record.
    pub to: String,
}

/// A declared consumer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsumerDefinition {
    /// Its id.
    pub id: u32,
    /// The name it is known by, unique across the store.
    pub name: String,
    /// The brokers to reach, as written.
    ///
    /// Stored as written for the reason a replica's endpoint is: whether an
    /// address resolves is a question for whoever dials it, and refusing an
    /// unreachable one at declaration time would make the statement's success
    /// depend on the network being up at the moment it ran.
    pub brokers: Vec<String>,
    /// The topic to read.
    pub topic: String,
    /// The consumer group, as declared.
    ///
    /// Never derived from the node id. Deriving it would be a bug that appears
    /// only in a cluster: every node would form its own group, and every node
    /// would then consume every message.
    pub group: String,
    /// How a message becomes fields, as the word that was written.
    pub format: String,
    /// Which message field carries the record's identity.
    ///
    /// This is what makes a replayed message converge to one record instead of
    /// two, so it is the field the delivery claim rests on.
    pub identity: String,
    /// Which message fields become which record fields.
    ///
    /// A field nobody named does not land.
    pub mapping: Vec<Mapped>,
    /// The namespace the destination table is in.
    pub namespace: NamespaceId,
    /// The database the destination table is in.
    pub database: DatabaseId,
    /// The table the records land in.
    pub destination: TableId,
    /// What happens to a message that cannot be applied.
    pub on_failure: OnFailure,
    /// How many consumers this declaration runs on each node.
    ///
    /// Never zero: a declaration that runs nothing is a consumer an operator
    /// believes is consuming.
    pub parallelism: u32,
    /// The user who declared it, and whose authority its writes carry.
    ///
    /// # Why a consumer has an identity at all
    ///
    /// Without one, the authority question is asked **once**, at
    /// `DEFINE CONSUMER`, and never again — so demoting the declarer, revoking
    /// their authority or deleting the account outright does not stop the
    /// writing, because there is no identity in the loop for any of those to act
    /// on. The rule the rest of this store follows is that a thing acts with the
    /// authority of whoever asked for it, and this is that rule reaching the one
    /// path that had escaped it.
    ///
    /// `None` for a consumer declared before this field existed. Those keep
    /// running unbound rather than stopping on upgrade — narrowing them would be
    /// an outage delivered as a migration — and `INFO FOR CONSUMER` reports the
    /// absence so an operator can find them and redeclare. An absence that
    /// nothing surfaces is the same as no field at all.
    pub declarer: Option<u32>,
}

impl ConsumerDefinition {
    /// The value written to the catalog.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let brokers = self
            .brokers
            .iter()
            .map(|broker| Value::from(broker.as_str()))
            .collect();
        let mapping = self
            .mapping
            .iter()
            .map(|pair| {
                Value::Object(BTreeMap::from([
                    ("from".to_owned(), Value::from(pair.from.as_str())),
                    ("to".to_owned(), Value::from(pair.to.as_str())),
                ]))
            })
            .collect();
        let mut fields = BTreeMap::from([
            (FIELD_ID.to_owned(), number(self.id)),
            (FIELD_NAME.to_owned(), Value::from(self.name.as_str())),
            (FIELD_BROKERS.to_owned(), Value::Array(brokers)),
            (FIELD_TOPIC.to_owned(), Value::from(self.topic.as_str())),
            (FIELD_GROUP.to_owned(), Value::from(self.group.as_str())),
            (FIELD_FORMAT.to_owned(), Value::from(self.format.as_str())),
            (
                FIELD_IDENTITY.to_owned(),
                Value::from(self.identity.as_str()),
            ),
            (FIELD_MAPPING.to_owned(), Value::Array(mapping)),
            (FIELD_NAMESPACE.to_owned(), number(self.namespace.get())),
            (FIELD_DATABASE.to_owned(), number(self.database.get())),
            (FIELD_TABLE.to_owned(), number(self.destination.get())),
            (
                FIELD_ON_FAILURE.to_owned(),
                Value::from(self.on_failure.spelling()),
            ),
            (FIELD_PARALLELISM.to_owned(), number(self.parallelism)),
        ]);
        // Written only when there is one, so a record predating the field and a
        // record whose declarer is unknown are the same shape rather than two
        // that a reader has to tell apart.
        if let Some(declarer) = self.declarer {
            fields.insert(FIELD_DECLARER.to_owned(), number(declarer));
        }
        Value::Object(fields)
    }

    /// Read a definition back.
    ///
    /// Every field is required, and a missing one is refused rather than
    /// defaulted. That is the opposite of the rule a replica's `roles` follows,
    /// and deliberately: a replica gained a field after the fact, so absence
    /// there means *declared before this existed*. Nothing here was ever
    /// optional, so absence here means the record is not a consumer — and
    /// defaulting a broker list, a group or a failure policy would start a
    /// background writer against settings nobody chose.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when a field is missing or holds the
    /// wrong type.
    pub fn from_value(value: &Value) -> Result<Self> {
        let fields = object(value, ENTITY)?;
        let on_failure = OnFailure::parse(text(fields, FIELD_ON_FAILURE)?.as_str()).ok_or(
            Error::CatalogMalformed {
                entity: ENTITY,
                field: FIELD_ON_FAILURE,
                found: "a failure policy",
            },
        )?;
        let parallelism = field_id(fields, FIELD_PARALLELISM, ENTITY)?;
        if parallelism == 0 {
            return Err(Error::CatalogMalformed {
                entity: ENTITY,
                field: FIELD_PARALLELISM,
                found: "zero",
            });
        }
        Ok(Self {
            id: field_id(fields, FIELD_ID, ENTITY)?,
            name: field_name(fields, ENTITY)?,
            brokers: strings(fields, FIELD_BROKERS)?,
            topic: text(fields, FIELD_TOPIC)?,
            group: text(fields, FIELD_GROUP)?,
            format: text(fields, FIELD_FORMAT)?,
            identity: text(fields, FIELD_IDENTITY)?,
            mapping: mapping_in(fields)?,
            namespace: NamespaceId::new(field_id(fields, FIELD_NAMESPACE, ENTITY)?),
            database: DatabaseId::new(field_id(fields, FIELD_DATABASE, ENTITY)?),
            destination: TableId::new(field_id(fields, FIELD_TABLE, ENTITY)?),
            on_failure,
            parallelism,
            // Absent means *declared before this field existed*, which is the
            // rule a replica's `roles` follows and the opposite of every other
            // field here. It is admissible precisely because it was never
            // optional at declaration: a consumer written by this binary always
            // carries one, so an absence is an age rather than a choice.
            declarer: match fields.get(FIELD_DECLARER) {
                None => None,
                Some(_) => Some(field_id(fields, FIELD_DECLARER, ENTITY)?),
            },
        })
    }
}

/// A string field a stored definition must carry.
fn text(fields: &BTreeMap<String, Value>, field: &'static str) -> Result<String> {
    let Some(Value::String(held)) = fields.get(field) else {
        return Err(Error::CatalogMalformed {
            entity: ENTITY,
            field,
            found: fields.get(field).map_or("none", Value::type_name),
        });
    };
    Ok(held.clone())
}

/// A list of strings a stored definition must carry, with at least one entry.
fn strings(fields: &BTreeMap<String, Value>, field: &'static str) -> Result<Vec<String>> {
    let Some(Value::Array(held)) = fields.get(field) else {
        return Err(Error::CatalogMalformed {
            entity: ENTITY,
            field,
            found: fields.get(field).map_or("none", Value::type_name),
        });
    };
    let mut read = Vec::with_capacity(held.len());
    for entry in held {
        let Value::String(text) = entry else {
            return Err(Error::CatalogMalformed {
                entity: ENTITY,
                field,
                found: entry.type_name(),
            });
        };
        read.push(text.clone());
    }
    // An empty broker list is a consumer that can never connect, which would
    // start and then fail forever rather than being refused once.
    if read.is_empty() {
        return Err(Error::CatalogMalformed {
            entity: ENTITY,
            field,
            found: "an empty list",
        });
    }
    Ok(read)
}

/// The field mapping a stored definition carries.
fn mapping_in(fields: &BTreeMap<String, Value>) -> Result<Vec<Mapped>> {
    let Some(Value::Array(held)) = fields.get(FIELD_MAPPING) else {
        return Err(Error::CatalogMalformed {
            entity: ENTITY,
            field: FIELD_MAPPING,
            found: fields.get(FIELD_MAPPING).map_or("none", Value::type_name),
        });
    };
    let mut read = Vec::with_capacity(held.len());
    for entry in held {
        let pair = object(entry, ENTITY)?;
        read.push(Mapped {
            from: text(pair, "from")?,
            to: text(pair, "to")?,
        });
    }
    // A mapping naming nothing would land an empty record for every message —
    // the shape of a consumer that appears to work and writes no data.
    if read.is_empty() {
        return Err(Error::CatalogMalformed {
            entity: ENTITY,
            field: FIELD_MAPPING,
            found: "an empty mapping",
        });
    }
    Ok(read)
}

impl Catalog<'_, '_> {
    /// Declare a consumer.
    ///
    /// The name is claimed across the whole store rather than inside the
    /// destination's database, which follows what a user and a replica already
    /// do: a consumer is an operational object an operator refers to by one
    /// name, and two of them sharing a name in different databases would make
    /// `DROP CONSUMER orders_in` a question rather than an instruction.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NameTaken`] when the name is already declared.
    pub fn create_consumer(
        &mut self,
        definition: &ConsumerDefinition,
    ) -> Result<ConsumerDefinition> {
        let qualified = qualify(Level::Consumer, &[], &definition.name);
        self.reserve_name(&qualified)?;
        let id = self.allocate(Level::Consumer)?;
        let stored = ConsumerDefinition {
            id,
            ..definition.clone()
        };
        self.write(system::CONSUMERS, id, &stored.to_value());
        self.claim_name(&qualified, id);
        Ok(stored)
    }

    /// Forget a consumer.
    ///
    /// # Errors
    ///
    /// Returns an error when the substrate fails.
    pub fn drop_consumer(&mut self, consumer: &ConsumerDefinition) -> Result<()> {
        let qualified = qualify(Level::Consumer, &[], &consumer.name);
        self.transaction.delete(system::address(
            system::CONSUMERS,
            RecordId::Int(id_key(consumer.id)),
        ));
        self.transaction
            .delete(system::address(system::NAMES, RecordId::from(qualified)));
        Ok(())
    }

    /// Every declared consumer, in name order.
    ///
    /// Sorted here for the reason the peer list is: the catalog hands these back
    /// in whatever order they were declared, and an answer whose shape depends
    /// on that is two answers to one question.
    ///
    /// # Errors
    ///
    /// Returns an error when a stored definition cannot be read.
    pub fn consumers(&self) -> Result<Vec<ConsumerDefinition>> {
        let mut found = Vec::new();
        for (_, payload) in self.transaction.scan_table(
            system::SYSTEM_NAMESPACE,
            system::SYSTEM_DATABASE,
            system::CONSUMERS,
        )? {
            found.push(ConsumerDefinition::from_value(&decode_payload(&payload)?)?);
        }
        found.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(found)
    }
}
