//! How a reach is written into a catalog row and read back.

use super::super::definition::number;
use super::{
    FIELD_DATABASE, FIELD_NAMESPACE, FIELD_REACH, FIELD_SHARD, FIELD_TABLE, REACH_DATABASE,
    REACH_NAMESPACE, REACH_SHARD, REACH_STORE, Reach,
};
use crate::error::{Error, Result};
use std::collections::BTreeMap;
use tessari_types::{DatabaseId, NamespaceId, Number, ShardId, TableId, Value};

/// The catalog's encoding of a [`Reach`].
///
/// A trait rather than inherent methods because the shape itself lives a layer
/// below — a log key carries a reach, and store keys are encoded under this
/// crate — while this encoding raises **this** crate's malformed-catalog error
/// and belongs with the catalog that reads it. The method syntax at the call
/// sites is unchanged.
pub(crate) trait ReachCodec: Sized {
    /// This reach, as a catalog record stores it.
    fn to_value(self) -> Value;

    /// Read one back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when the stored value is not a reach.
    fn from_value(value: &Value, entity: &'static str, field: &'static str) -> Result<Reach>;
}

impl ReachCodec for Reach {
    /// This reach, as a catalog record stores it.
    ///
    /// Tagged rather than inferred from which ids are present, because
    /// [`Reach::Store`] carries no ids at all and an object with no ids would
    /// then be the same bytes as an object somebody wrote wrong. The tag makes
    /// the whole store a thing that was said rather than a thing the reader
    /// assumed.
    ///
    /// # Why the codec lives beside the type and not beside its first caller
    ///
    /// It has two callers now — a peer's subscription and a leadership's range —
    /// and a reach that encoded one way in one row and another way in the other
    /// would be two on-disk spellings of one type. Two readings of the same
    /// bytes is a thing that can disagree with itself, which is the reason the
    /// log record carries no mutation count either.
    fn to_value(self) -> Value {
        let (namespace, database) = self.parts();
        let mut fields = BTreeMap::from([(
            FIELD_REACH.to_owned(),
            Value::from(match self {
                Reach::Store => REACH_STORE,
                Reach::Namespace(_) => REACH_NAMESPACE,
                Reach::Database(_, _) => REACH_DATABASE,
                Reach::Shard(..) => REACH_SHARD,
            }),
        )]);
        if let Some(namespace) = namespace {
            fields.insert(FIELD_NAMESPACE.to_owned(), number(namespace.get()));
        }
        if let Some(database) = database {
            fields.insert(FIELD_DATABASE.to_owned(), number(database.get()));
        }
        if let Reach::Shard(_, _, table, shard) = self {
            fields.insert(FIELD_TABLE.to_owned(), number(table.get()));
            fields.insert(FIELD_SHARD.to_owned(), number(shard.get()));
        }
        Value::Object(fields)
    }

    /// Read a reach back from the value [`Self::to_value`] wrote.
    ///
    /// `entity` and `field` are carried so the refusal names the row the caller
    /// was reading rather than this type: a malformed reach is a defect in some
    /// definition, and a reader told only *reach* has to guess which one.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when the value is not an object, the
    /// tag is missing or unknown, or a tag's ids are absent or out of range.
    fn from_value(value: &Value, entity: &'static str, field: &'static str) -> Result<Reach> {
        let malformed = || Error::CatalogMalformed {
            entity,
            field,
            found: "reach",
        };
        let Value::Object(inner) = value else {
            return Err(Error::CatalogMalformed {
                entity,
                field,
                found: value.type_name(),
            });
        };
        let Some(Value::String(tag)) = inner.get(FIELD_REACH) else {
            return Err(malformed());
        };
        let id = |field: &'static str| -> Option<u32> {
            match inner.get(field) {
                Some(Value::Number(Number::Integer(raw))) => u32::try_from(*raw).ok(),
                _ => None,
            }
        };
        match tag.as_str() {
            REACH_STORE => Ok(Reach::Store),
            REACH_NAMESPACE => id(FIELD_NAMESPACE)
                .map(|namespace| Reach::Namespace(NamespaceId::new(namespace)))
                .ok_or_else(malformed),
            REACH_DATABASE => match (id(FIELD_NAMESPACE), id(FIELD_DATABASE)) {
                (Some(namespace), Some(database)) => Ok(Reach::Database(
                    NamespaceId::new(namespace),
                    DatabaseId::new(database),
                )),
                _ => Err(malformed()),
            },
            REACH_SHARD => match (
                id(FIELD_NAMESPACE),
                id(FIELD_DATABASE),
                id(FIELD_TABLE),
                id(FIELD_SHARD).filter(|shard| *shard != 0),
            ) {
                (Some(namespace), Some(database), Some(table), Some(shard)) => Ok(Reach::Shard(
                    NamespaceId::new(namespace),
                    DatabaseId::new(database),
                    TableId::new(table),
                    ShardId::new(shard),
                )),
                _ => Err(malformed()),
            },
            _ => Err(malformed()),
        }
    }
}
