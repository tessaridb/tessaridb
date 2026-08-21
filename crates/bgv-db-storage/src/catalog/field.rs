//! What a table declares about one of its fields.
//!
//! A field definition is a catalog entry like any other — an ordinary record in
//! the system tenancy (ADR-0009) — so it takes part in the transaction that
//! issued it, replicates through the same apply path, and is visible to a reader
//! at exactly the snapshot the reader holds.
//!
//! It lives in its own file rather than beside the other definitions because
//! [`super::definition`] is already at its size budget, and because a field
//! carries the one thing the others do not: a [`FieldKind`], which is the value
//! system's own vocabulary rather than the catalog's.

use std::collections::BTreeMap;

use bgv_db_encoding::decode_payload;
use bgv_db_types::{DatabaseId, FieldId, FieldKind, NamespaceId, RecordId, TableId, Value};

use super::definition::{field_id, field_name, number, object};
use super::{Catalog, Level, id_key, qualify, system};
use crate::error::{Error, Result};

const FIELD_ID: &str = "id";
const FIELD_NAME: &str = "name";
const FIELD_NAMESPACE: &str = "namespace";
const FIELD_DATABASE: &str = "database";
const FIELD_TABLE: &str = "table";
const FIELD_KIND: &str = "kind";

const ENTITY: &str = "field";

/// A declared field on a table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldDefinition {
    /// The field's id.
    pub id: FieldId,
    /// The namespace it belongs to.
    pub namespace: NamespaceId,
    /// The database it belongs to.
    pub database: DatabaseId,
    /// The table it is on.
    pub table: TableId,
    /// Its name, unique within that table.
    pub name: String,
    /// What values it accepts.
    pub kind: FieldKind,
}

impl FieldDefinition {
    /// The value written to the catalog.
    ///
    /// The kind is stored by its **spelling** rather than by an ordinal, so that
    /// reordering [`FieldKind`] cannot silently reinterpret every stored
    /// definition. A name costs a few bytes per definition and there are as many
    /// definitions as there are declared fields.
    #[must_use]
    pub fn to_value(&self) -> Value {
        Value::Object(BTreeMap::from([
            (FIELD_ID.to_owned(), number(self.id.get())),
            (FIELD_NAMESPACE.to_owned(), number(self.namespace.get())),
            (FIELD_DATABASE.to_owned(), number(self.database.get())),
            (FIELD_TABLE.to_owned(), number(self.table.get())),
            (FIELD_NAME.to_owned(), Value::from(self.name.as_str())),
            (FIELD_KIND.to_owned(), Value::from(self.kind.name())),
        ]))
    }

    /// Read a definition back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when a field is missing, holds the
    /// wrong type, or names a kind this binary does not know.
    pub fn from_value(value: &Value) -> Result<Self> {
        let fields = object(value, ENTITY)?;
        let Some(Value::String(spelling)) = fields.get(FIELD_KIND) else {
            return Err(Error::CatalogMalformed {
                entity: ENTITY,
                field: FIELD_KIND,
                found: fields.get(FIELD_KIND).map_or("none", Value::type_name),
            });
        };
        // An unknown spelling is corruption rather than a validation failure: it
        // was written by something that knew a kind this binary does not, and
        // guessing would apply the wrong constraint to real records.
        let Some(kind) = FieldKind::parse(spelling) else {
            return Err(Error::CatalogMalformed {
                entity: ENTITY,
                field: FIELD_KIND,
                found: "string",
            });
        };
        Ok(Self {
            id: FieldId::new(field_id(fields, FIELD_ID, ENTITY)?),
            namespace: NamespaceId::new(field_id(fields, FIELD_NAMESPACE, ENTITY)?),
            database: DatabaseId::new(field_id(fields, FIELD_DATABASE, ENTITY)?),
            table: TableId::new(field_id(fields, FIELD_TABLE, ENTITY)?),
            name: field_name(fields, ENTITY)?,
            kind,
        })
    }
}

impl Catalog<'_, '_> {
    /// Declare a field on an existing table.
    ///
    /// The declaration constrains what the field may hold from the moment it
    /// exists, including rows already there — enforcement lives in
    /// [`crate::schema`], on the same apply path that maintains indexes.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoSuchParent`] when the table does not exist, and
    /// [`Error::NameTaken`] when the field is already declared on it.
    pub fn create_field(
        &mut self,
        table: TableId,
        name: &str,
        kind: FieldKind,
    ) -> Result<FieldDefinition> {
        let Some(parent) = self.table(table)? else {
            return Err(Error::NoSuchParent {
                entity: "table",
                id: table.get(),
            });
        };
        let qualified = qualify(
            Level::Field,
            &[
                parent.namespace.get(),
                parent.database.get(),
                parent.id.get(),
            ],
            name,
        );
        self.reserve_name(&qualified)?;
        let id = FieldId::new(self.allocate(Level::Field)?);
        let definition = FieldDefinition {
            id,
            namespace: parent.namespace,
            database: parent.database,
            table,
            name: name.to_owned(),
            kind,
        };
        self.write(system::FIELDS, id.get(), &definition.to_value());
        self.claim_name(&qualified, id.get());
        Ok(definition)
    }

    /// Look a field up by id.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored definition cannot be read.
    pub fn field(&self, id: FieldId) -> Result<Option<FieldDefinition>> {
        self.read(system::FIELDS, id.get())?
            .as_ref()
            .map(FieldDefinition::from_value)
            .transpose()
    }

    /// Every field declared on one table.
    ///
    /// Reads the whole field catalog and filters, for the same reason
    /// [`Self::indexes_on`] does: honest at catalog scale, and the answer when
    /// it stops being honest is a cache keyed by the catalog's version rather
    /// than a cleverer scan.
    ///
    /// # Errors
    ///
    /// Returns an error when a stored definition cannot be read.
    pub fn fields_on(&self, table: TableId) -> Result<Vec<FieldDefinition>> {
        let mut found = Vec::new();
        for (_, bytes) in self.transaction.scan_table(
            system::SYSTEM_NAMESPACE,
            system::SYSTEM_DATABASE,
            system::FIELDS,
        )? {
            let definition = FieldDefinition::from_value(&decode_payload(&bytes)?)?;
            if definition.table == table {
                found.push(definition);
            }
        }
        Ok(found)
    }

    /// Drop a field's declaration and release its name.
    ///
    /// The records keep the field; only the constraint on it goes away. A
    /// declaration is a rule about what may be written, and removing the rule is
    /// not the same statement as removing the data.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored definition cannot be read.
    pub fn drop_field(&mut self, id: FieldId) -> Result<bool> {
        let Some(definition) = self.field(id)? else {
            return Ok(false);
        };
        let qualified = qualify(
            Level::Field,
            &[
                definition.namespace.get(),
                definition.database.get(),
                definition.table.get(),
            ],
            &definition.name,
        );
        self.transaction.delete(system::address(
            system::FIELDS,
            RecordId::Int(id_key(id.get())),
        ));
        self.transaction
            .delete(system::address(system::NAMES, RecordId::from(qualified)));
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn definition(kind: FieldKind) -> FieldDefinition {
        FieldDefinition {
            id: FieldId::new(4),
            namespace: NamespaceId::new(1),
            database: DatabaseId::new(2),
            table: TableId::new(3),
            name: "email".to_owned(),
            kind,
        }
    }

    #[test]
    fn a_definition_of_every_kind_round_trips() {
        for kind in FieldKind::all() {
            let original = definition(*kind);
            assert_eq!(
                FieldDefinition::from_value(&original.to_value()).unwrap(),
                original,
                "{} did not survive the catalog",
                kind.name()
            );
        }
    }

    #[test]
    fn the_kind_is_stored_by_spelling_so_reordering_the_enum_cannot_reinterpret_it() {
        let stored = definition(FieldKind::Decimal).to_value();
        let Value::Object(fields) = &stored else {
            unreachable!("a definition is an object")
        };
        assert_eq!(fields.get(FIELD_KIND), Some(&Value::from("decimal")));
    }

    #[test]
    fn a_kind_this_binary_does_not_know_is_refused_rather_than_guessed() {
        let mut stored = definition(FieldKind::String).to_value();
        if let Value::Object(fields) = &mut stored {
            fields.insert(FIELD_KIND.to_owned(), Value::from("geometry"));
        }
        let error = FieldDefinition::from_value(&stored).unwrap_err();
        assert_eq!(error.code(), "corruption");
    }

    #[test]
    fn a_definition_missing_its_kind_is_refused_and_names_the_field() {
        let mut stored = definition(FieldKind::String).to_value();
        if let Value::Object(fields) = &mut stored {
            fields.remove(FIELD_KIND);
        }
        let text = FieldDefinition::from_value(&stored)
            .unwrap_err()
            .to_string();
        assert!(text.contains(FIELD_KIND), "{text}");
    }
}
