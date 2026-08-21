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

use std::collections::BTreeMap;

use bgv_db_types::{DatabaseId, NamespaceId, Number, TableId, Value};

use crate::error::{Error, Result};

const FIELD_ID: &str = "id";
const FIELD_NAME: &str = "name";
const FIELD_NAMESPACE: &str = "namespace";
const FIELD_DATABASE: &str = "database";

/// A namespace: the outermost tenancy level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespaceDefinition {
    /// The namespace's id.
    pub id: NamespaceId,
    /// Its name, which may change without moving anything.
    pub name: String,
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

/// A table within a database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableDefinition {
    /// The table's id.
    pub id: TableId,
    /// The namespace it belongs to.
    pub namespace: NamespaceId,
    /// The database it belongs to.
    pub database: DatabaseId,
    /// Its name, unique within that database.
    pub name: String,
}

impl NamespaceDefinition {
    /// The value written to the catalog.
    #[must_use]
    pub fn to_value(&self) -> Value {
        Value::Object(BTreeMap::from([
            (FIELD_ID.to_owned(), number(self.id.get())),
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
        let fields = object(value, "namespace")?;
        Ok(Self {
            id: NamespaceId::new(field_id(fields, FIELD_ID, "namespace")?),
            name: field_name(fields, "namespace")?,
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

impl TableDefinition {
    /// The value written to the catalog.
    #[must_use]
    pub fn to_value(&self) -> Value {
        Value::Object(BTreeMap::from([
            (FIELD_ID.to_owned(), number(self.id.get())),
            (FIELD_NAMESPACE.to_owned(), number(self.namespace.get())),
            (FIELD_DATABASE.to_owned(), number(self.database.get())),
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
        let fields = object(value, "table")?;
        Ok(Self {
            id: TableId::new(field_id(fields, FIELD_ID, "table")?),
            namespace: NamespaceId::new(field_id(fields, FIELD_NAMESPACE, "table")?),
            database: DatabaseId::new(field_id(fields, FIELD_DATABASE, "table")?),
            name: field_name(fields, "table")?,
        })
    }
}

/// An identifier as it is stored: an integer, widened rather than cast.
pub(crate) fn number(id: u32) -> Value {
    Value::Number(Number::Integer(i64::from(id)))
}

fn object<'a>(value: &'a Value, entity: &'static str) -> Result<&'a BTreeMap<String, Value>> {
    match value {
        Value::Object(fields) => Ok(fields),
        other => Err(Error::CatalogMalformed {
            entity,
            field: FIELD_ID,
            found: other.type_name(),
        }),
    }
}

/// An identifier read back out of a stored value.
///
/// An integer too wide for the identifier is refused rather than narrowed: a
/// truncated id addresses a different entity, and nothing downstream could tell.
pub(crate) fn id_of(value: &Value, entity: &'static str, field: &'static str) -> Result<u32> {
    let malformed = |found: &'static str| Error::CatalogMalformed {
        entity,
        field,
        found,
    };
    let Value::Number(Number::Integer(raw)) = value else {
        return Err(malformed(value.type_name()));
    };
    u32::try_from(*raw).map_err(|_| malformed("number"))
}

pub(crate) fn field_id(
    fields: &BTreeMap<String, Value>,
    field: &'static str,
    entity: &'static str,
) -> Result<u32> {
    match fields.get(field) {
        Some(value) => id_of(value, entity, field),
        None => Err(Error::CatalogMalformed {
            entity,
            field,
            found: "none",
        }),
    }
}

fn field_name(fields: &BTreeMap<String, Value>, entity: &'static str) -> Result<String> {
    match fields.get(FIELD_NAME) {
        Some(Value::String(name)) => Ok(name.clone()),
        other => Err(Error::CatalogMalformed {
            entity,
            field: FIELD_NAME,
            found: other.map_or("none", Value::type_name),
        }),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn every_definition_round_trips_through_its_value() {
        let namespace = NamespaceDefinition {
            id: NamespaceId::new(7),
            name: "prod".to_owned(),
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
        };
        assert_eq!(
            TableDefinition::from_value(&table.to_value()).unwrap(),
            table
        );
    }

    #[test]
    fn a_definition_missing_a_field_is_refused_and_names_it() {
        let fields = BTreeMap::from([(FIELD_ID.to_owned(), number(1))]);
        let error = TableDefinition::from_value(&Value::Object(fields)).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("namespace"), "{text}");
    }

    #[test]
    fn a_definition_that_is_not_an_object_is_refused() {
        let error = NamespaceDefinition::from_value(&Value::from("prod")).unwrap_err();
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
