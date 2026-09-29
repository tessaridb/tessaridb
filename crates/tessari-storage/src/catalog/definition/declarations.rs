//! What a series, view, vault, vector, queue and edge declaration holds.

use super::{
    FIELD_ATTEMPTS, FIELD_DESCENDING, FIELD_DIMENSION, FIELD_DISTANCE, FIELD_EVENT_TIME,
    FIELD_FROM, FIELD_KEY_ID, FIELD_ORDER, FIELD_READ, FIELD_RETAIN, FIELD_TIMEOUT, FIELD_TO,
    FIELD_WRAPPED, VectorDistance, field_id, flag, number, object,
};
use crate::error::{Error, Result};
use std::collections::BTreeMap;
use tessari_types::{Duration, TableId, Value};
use tessari_vault::{KeyId, Wrapped};

/// How long a series table answers with a record.
///
/// One field, and it is the whole capability, so it has no default for the
/// reason [`QueueDeclaration::timeout`] has none: a series table that keeps
/// everything is a table, and a retention the store guessed would drop somebody's
/// records at a boundary nobody chose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeriesDeclaration {
    /// How far back the answer reaches.
    ///
    /// Measured from the instant the read happens, against the millisecond the
    /// record's identity carries. Not stored on the record, unlike a queue's
    /// deadline, because there is nothing to write it to: the rule is a property
    /// of the table and applies to records written before it as well as after.
    pub retain: Duration,
    /// The `datetime` field each record's identity is minted from, when the
    /// series is ordered by event time (ADR-0088 §1); `None` is arrival time.
    ///
    /// Absent from a declaration written before the clause existed, which then
    /// reads back as arrival time — what that declaration meant.
    pub time: Option<String>,
}

impl SeriesDeclaration {
    /// The value written inside the table's catalog entry.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut fields = BTreeMap::from([(FIELD_RETAIN.to_owned(), Value::Duration(self.retain))]);
        if let Some(time) = &self.time {
            fields.insert(FIELD_EVENT_TIME.to_owned(), Value::String(time.clone()));
        }
        Value::Object(fields)
    }

    /// Read a declaration back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when the retention is missing or is
    /// not a duration.
    pub fn from_value(value: &Value) -> Result<Self> {
        const ENTITY: &str = "series";
        let fields = object(value, ENTITY)?;
        let Some(Value::Duration(retain)) = fields.get(FIELD_RETAIN) else {
            return Err(Error::CatalogMalformed {
                entity: ENTITY,
                field: FIELD_RETAIN,
                found: fields
                    .get(FIELD_RETAIN)
                    .map_or("none", tessari_types::Value::type_name),
            });
        };
        let time = match fields.get(FIELD_EVENT_TIME) {
            None => None,
            Some(Value::String(time)) => Some(time.clone()),
            Some(other) => {
                return Err(Error::CatalogMalformed {
                    entity: ENTITY,
                    field: FIELD_EVENT_TIME,
                    found: other.type_name(),
                });
            }
        };
        Ok(Self {
            retain: *retain,
            time,
        })
    }
}

/// The read a view names.
///
/// # Text, and not a serialised tree
///
/// The same choice a field's `DEFAULT` makes and for the reason stated there —
/// the storage layer cannot evaluate a TessariQL expression, so a definition
/// keeps the text it was written as and the layer that owns the language parses
/// it back. Two properties follow that a stored tree would not have: `INFO`
/// answers with the statement somebody typed rather than a re-rendered one that
/// happens to mean the same thing, and a view written before a clause existed
/// cannot decode into a read that silently lost it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewDeclaration {
    /// The read, exactly as it was written.
    pub read: String,
}

impl ViewDeclaration {
    /// The value written inside the table's catalog entry.
    #[must_use]
    pub fn to_value(&self) -> Value {
        Value::Object(BTreeMap::from([(
            FIELD_READ.to_owned(),
            Value::from(self.read.as_str()),
        )]))
    }

    /// Read a declaration back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when the read is missing or is not a
    /// string.
    pub fn from_value(value: &Value) -> Result<Self> {
        const ENTITY: &str = "view";
        let fields = object(value, ENTITY)?;
        let Some(Value::String(read)) = fields.get(FIELD_READ) else {
            return Err(Error::CatalogMalformed {
                entity: ENTITY,
                field: FIELD_READ,
                found: fields
                    .get(FIELD_READ)
                    .map_or("none", tessari_types::Value::type_name),
            });
        };
        Ok(Self { read: read.clone() })
    }
}

/// The key a vault's records are sealed under, sealed itself.
///
/// On the kind rather than beside it, for the reason [`EdgeDeclaration`] rides
/// on `Edge`: a field beside the kind would make "carries a key but is not a
/// vault" representable, and that state is a table full of records nothing can
/// ever open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultDeclaration {
    /// The vault's own key, sealed under the store's master key.
    ///
    /// Every record in the vault has its data key wrapped under this one, so
    /// this single value is what stands between a stolen backend and every
    /// secret the vault holds — and it is itself unreadable without a
    /// passphrase that is never stored anywhere.
    pub key: Wrapped,
}

/// How wide a vector store's vectors are, and what distance searches them.
///
/// Both are on the kind rather than beside it, for the reason
/// [`EdgeDeclaration`] rides on `Edge`: a pair of fields would make "declares a
/// width but is not a vector store" representable, and that is the state the
/// type exists to abolish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VectorDeclaration {
    /// How many components every vector in the store holds.
    ///
    /// A `u32` because that is how this file stores every small integer, and
    /// because the parser refuses a width no `u32` could carry — a store that
    /// could not write its own declaration back would report a width it was
    /// never given.
    pub dimension: u32,
    /// The distance its index is built and searched with.
    pub distance: VectorDistance,
}

impl VectorDeclaration {
    /// The value written inside the table's catalog entry.
    #[must_use]
    pub fn to_value(&self) -> Value {
        Value::Object(BTreeMap::from([
            (FIELD_DIMENSION.to_owned(), number(self.dimension)),
            (FIELD_DISTANCE.to_owned(), Value::from(self.distance.name())),
        ]))
    }

    /// Read a declaration back.
    ///
    /// A distance this build does not recognise is **refused**, on the same
    /// reasoning `identity_kind` records: an unknown word is a store already
    /// searched some other way, and reading it as `cosine` would answer a
    /// nearest-neighbour question from a graph built for a different geometry —
    /// plausible neighbours that are not the nearest.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when the width or the distance is
    /// missing, holds the wrong type, or names a distance this build has not.
    pub fn from_value(value: &Value) -> Result<Self> {
        const ENTITY: &str = "vector store";
        let fields = object(value, ENTITY)?;
        let Some(Value::String(distance)) = fields.get(FIELD_DISTANCE) else {
            return Err(Error::CatalogMalformed {
                entity: ENTITY,
                field: FIELD_DISTANCE,
                found: fields
                    .get(FIELD_DISTANCE)
                    .map_or("none", tessari_types::Value::type_name),
            });
        };
        Ok(Self {
            dimension: field_id(fields, FIELD_DIMENSION, ENTITY)?,
            distance: VectorDistance::parse(distance).ok_or(Error::CatalogMalformed {
                entity: ENTITY,
                field: FIELD_DISTANCE,
                found: "a distance this build does not have",
            })?,
        })
    }
}

/// How long a queue holds a claim, and how many times it hands a record out.
///
/// The timeout is the whole capability, which is why it has no default: a queue
/// whose holds never lapse is a table with two extra fields, and a queue whose
/// timeout the store guessed would hand work to a second worker at a moment
/// nobody chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueDeclaration {
    /// How long a claim holds a record before it lapses.
    ///
    /// Added to the instant the claiming session reads, once, and written into
    /// the record — so the deadline in the log is a value every node agrees
    /// about rather than a computation each one repeats against its own clock.
    pub timeout: Duration,
    /// How many times one record may be handed out, when a ceiling was declared.
    ///
    /// `None` is unlimited, which is a legitimate choice for a queue whose work
    /// cannot poison and a visible one, because it is what leaving the clause
    /// out says. A record that reaches the ceiling stops being claimable and
    /// stays where it is: the dead letter is a predicate, not a second table.
    pub attempts: Option<u32>,
}

impl QueueDeclaration {
    /// The value written inside the table's catalog entry.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut fields =
            BTreeMap::from([(FIELD_TIMEOUT.to_owned(), Value::Duration(self.timeout))]);
        // Written only when it was declared, on the bucket ceiling's contract
        // rather than a flag's: an attempt ceiling nobody named is absent rather
        // than zero, and zero is the one number that would have to mean
        // "unlimited" while reading as "never hand this out".
        if let Some(ceiling) = self.attempts {
            fields.insert(FIELD_ATTEMPTS.to_owned(), number(ceiling));
        }
        Value::Object(fields)
    }

    /// Read a declaration back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when the timeout is missing or is not
    /// a duration, or when the attempt ceiling is not a number.
    pub fn from_value(value: &Value) -> Result<Self> {
        const ENTITY: &str = "queue";
        let fields = object(value, ENTITY)?;
        let Some(Value::Duration(timeout)) = fields.get(FIELD_TIMEOUT) else {
            return Err(Error::CatalogMalformed {
                entity: ENTITY,
                field: FIELD_TIMEOUT,
                found: fields
                    .get(FIELD_TIMEOUT)
                    .map_or("none", tessari_types::Value::type_name),
            });
        };
        Ok(Self {
            timeout: *timeout,
            attempts: match fields.get(FIELD_ATTEMPTS) {
                Some(_) => Some(field_id(fields, FIELD_ATTEMPTS, ENTITY)?),
                None => None,
            },
        })
    }
}

/// The pair an edge table joins, and the order its edges are held in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeDeclaration {
    /// The table an edge may leave.
    pub from: TableId,
    /// The table an edge may arrive at.
    pub to: TableId,
    /// The order neighbours are held in, if one was declared.
    ///
    /// A **key-grammar** property rather than a query-time one: it becomes the
    /// suffix of the endpoint index's key, which is what makes "the ten most
    /// recent" a bounded read of adjacent keys instead of reading every edge and
    /// sorting. It is also why it cannot be changed later without rewriting
    /// every edge index in every store.
    pub order: Option<EdgeOrder>,
}

/// The order an edge table holds one node's edges in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeOrder {
    /// The edge field the order reads.
    pub field: String,
    /// Whether the order runs downward.
    pub descending: bool,
}

impl VaultDeclaration {
    /// The value written inside the table's catalog entry.
    ///
    /// Two opaque byte strings. Nothing here is a secret — the wrapped key is
    /// ciphertext under the store's master key, and the identifier is a random
    /// name rather than anything derived from key material — so the catalog can
    /// hold them the way it holds any other declaration.
    #[must_use]
    pub fn to_value(&self) -> Value {
        Value::Object(BTreeMap::from([
            (
                FIELD_KEY_ID.to_owned(),
                Value::Bytes(self.key.key_id.bytes().to_vec()),
            ),
            (
                FIELD_WRAPPED.to_owned(),
                Value::Bytes(self.key.sealed.clone()),
            ),
        ]))
    }

    /// Read a declaration back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when either part is missing or holds
    /// the wrong type. A vault whose key cannot be read is refused rather than
    /// treated as a vault with no key: the second reads as an empty store and
    /// would let a caller declare fields on it and write records that nothing
    /// could ever open.
    pub fn from_value(value: &Value) -> Result<Self> {
        const ENTITY: &str = "vault";
        let fields = object(value, ENTITY)?;
        let Some(Value::Bytes(key_id)) = fields.get(FIELD_KEY_ID) else {
            return Err(Error::CatalogMalformed {
                entity: ENTITY,
                field: FIELD_KEY_ID,
                found: "missing or not bytes",
            });
        };
        let key_id = <[u8; KeyId::BYTES]>::try_from(key_id.as_slice()).map_err(|_| {
            Error::CatalogMalformed {
                entity: ENTITY,
                field: FIELD_KEY_ID,
                found: "the wrong number of bytes",
            }
        })?;
        let Some(Value::Bytes(sealed)) = fields.get(FIELD_WRAPPED) else {
            return Err(Error::CatalogMalformed {
                entity: ENTITY,
                field: FIELD_WRAPPED,
                found: "missing or not bytes",
            });
        };
        Ok(Self {
            key: Wrapped {
                key_id: KeyId::adopt(key_id),
                sealed: sealed.clone(),
            },
        })
    }
}

impl EdgeDeclaration {
    /// The value written inside the table's catalog entry.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut fields = BTreeMap::from([
            (FIELD_FROM.to_owned(), number(self.from.get())),
            (FIELD_TO.to_owned(), number(self.to.get())),
        ]);
        if let Some(order) = &self.order {
            fields.insert(FIELD_ORDER.to_owned(), Value::from(order.field.as_str()));
            fields.insert(FIELD_DESCENDING.to_owned(), Value::Bool(order.descending));
        }
        Value::Object(fields)
    }

    /// Read a declaration back.
    ///
    /// A direction without a field is refused rather than read as an unordered
    /// edge table: the two are different key grammars, and reading one as the other
    /// would answer a bounded neighbour read from an index that does not hold
    /// the order it claims.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when an endpoint is missing or holds
    /// the wrong type, or when the order is only half present.
    pub fn from_value(value: &Value) -> Result<Self> {
        let fields = object(value, "edge endpoints")?;
        let order = match fields.get(FIELD_ORDER) {
            None if fields.contains_key(FIELD_DESCENDING) => {
                return Err(Error::CatalogMalformed {
                    entity: "edge endpoints",
                    field: FIELD_ORDER,
                    found: "a direction with no field to order by",
                });
            }
            None => None,
            Some(Value::String(field)) => Some(EdgeOrder {
                field: field.clone(),
                descending: flag(fields, FIELD_DESCENDING, "edge endpoints")?,
            }),
            Some(other) => {
                return Err(Error::CatalogMalformed {
                    entity: "edge endpoints",
                    field: FIELD_ORDER,
                    found: other.type_name(),
                });
            }
        };
        Ok(Self {
            from: TableId::new(field_id(fields, FIELD_FROM, "edge endpoints")?),
            to: TableId::new(field_id(fields, FIELD_TO, "edge endpoints")?),
            order,
        })
    }
}
