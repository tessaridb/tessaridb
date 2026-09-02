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

use tessari_encoding::decode_payload;
use tessari_types::{
    Assertion, DatabaseId, FieldId, FieldKind, NamespaceId, RecordId, TableId, Value,
};

use super::definition::{field_id, field_name, flag, number, object};
use super::{Catalog, Level, id_key, qualify, system};
use crate::error::{Error, Result};

const FIELD_ID: &str = "id";
const FIELD_NAME: &str = "name";
const FIELD_NAMESPACE: &str = "namespace";
const FIELD_DATABASE: &str = "database";
const FIELD_TABLE: &str = "table";
const FIELD_KIND: &str = "kind";
const FIELD_REQUIRED: &str = "required";
const FIELD_DEFAULT: &str = "default";
const FIELD_ANALYZER: &str = "analyzer";
const FIELD_ASSERT: &str = "assert";

const ENTITY: &str = "field";

/// What a declaration says beyond the field's type.
///
/// A struct rather than two more parameters, for the reason `ids.rs` gives for
/// newtyping a `u32`: `create_field(id, "email", FieldKind::String, true, None)`
/// compiles just as well with the boolean meaning something else.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FieldShape {
    /// Whether the field must hold a value.
    pub required: bool,
    /// The expression a write uses when it supplies none, as written.
    pub default: Option<String>,
    /// The analyzer that turns this field's text into terms, by name.
    ///
    /// On the **field** and not on an index, which is the whole design: an
    /// analyzer on an index would let adding one change what a search finds.
    pub analyzer: Option<String>,
    /// What the value must satisfy beyond its type.
    ///
    /// Already lowered, unlike `default`, and the difference says who checks
    /// each: a default is evaluated by the **session** when a write supplies no
    /// value, so it can stay as written; an assertion is checked by the
    /// **store** on its apply path, where nothing can parse TessariQL and where a
    /// replica has to reach the same verdict from the record alone.
    pub assert: Option<Assertion>,
}

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
    /// Whether the field must hold a value: present, and not `null`.
    ///
    /// One marker rather than two, deliberately. The store keeps `none` and
    /// `null` apart everywhere else, so covering both here is a choice: it is
    /// what a caller means by "required", and a field that must be present but
    /// may hold nothing is a constraint that constrains almost nothing. The
    /// distinction stays available on every field that is not required.
    pub required: bool,
    /// The expression a write uses when it supplies no value, as written.
    ///
    /// Stored as **text** and parsed by the layer that can parse it. The store
    /// cannot evaluate a TessariQL expression — the language sits above it — and a
    /// default does not need it to: the value is materialised by the session
    /// before the record is written, so a replica applies a record that already
    /// carries it.
    pub default: Option<String>,
    /// The analyzer this field's text is turned into terms by, if any.
    ///
    /// Held by **name** rather than by id, so a dump reads without a second
    /// lookup and so the attachment survives an analyzer being redeclared.
    pub analyzer: Option<String>,
    /// What the value must satisfy beyond its type.
    ///
    /// **Lowered**, unlike `default`, and the contrast says who checks each. A
    /// default is evaluated by the session before the record is written, so it
    /// can stay as text. An assertion is checked by the store on its apply path,
    /// where nothing can parse TessariQL and where a replica must reach the same
    /// verdict from the record alone — so what is stored is the constraint
    /// itself rather than the sentence that described it.
    pub assert: Option<Assertion>,
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
            (
                FIELD_KIND.to_owned(),
                Value::from(self.kind.name().as_ref()),
            ),
            (FIELD_REQUIRED.to_owned(), Value::Bool(self.required)),
            (
                FIELD_DEFAULT.to_owned(),
                self.default.as_deref().map_or(Value::None, Value::from),
            ),
            (
                FIELD_ANALYZER.to_owned(),
                self.analyzer.as_deref().map_or(Value::None, Value::from),
            ),
            (
                FIELD_ASSERT.to_owned(),
                self.assert
                    .as_ref()
                    .map_or(Value::None, Assertion::to_value),
            ),
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
            // A definition written before either existed reads as neither, so
            // nothing on disk has to be migrated.
            required: flag(fields, FIELD_REQUIRED, ENTITY)?,
            default: optional_text(fields, FIELD_DEFAULT)?,
            analyzer: optional_text(fields, FIELD_ANALYZER)?,
            assert: optional_assertion(fields)?,
        })
    }
}

/// The assertion a definition carries, if it carries one.
///
/// A value that is present but describes no assertion is **corruption** rather
/// than an absent one: it was written by something that knew a constraint this
/// binary does not, and reading it as "no constraint" would let a write land
/// that the node which wrote the definition would refuse.
fn optional_assertion(fields: &BTreeMap<String, Value>) -> Result<Option<Assertion>> {
    match fields.get(FIELD_ASSERT) {
        None | Some(Value::None) => Ok(None),
        Some(held) => Assertion::from_value(held)
            .map(Some)
            .ok_or(Error::CatalogMalformed {
                entity: ENTITY,
                field: FIELD_ASSERT,
                found: held.type_name(),
            }),
    }
}

/// A field that may be absent, and is text when it is there.
fn optional_text(fields: &BTreeMap<String, Value>, field: &'static str) -> Result<Option<String>> {
    match fields.get(field) {
        Some(Value::String(written)) => Ok(Some(written.clone())),
        None | Some(Value::None) => Ok(None),
        Some(other) => Err(Error::CatalogMalformed {
            entity: ENTITY,
            field,
            found: other.type_name(),
        }),
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
        shape: FieldShape,
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
            required: shape.required,
            default: shape.default,
            analyzer: shape.analyzer,
            assert: shape.assert,
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
        let mut found = self.fields()?;
        found.retain(|definition| definition.table == table);
        Ok(found)
    }

    /// Every field declared anywhere in the store.
    ///
    /// The unfiltered form of [`Self::fields_on`], which is what a question
    /// about a **store-wide** name needs: an analyzer is declared once for the
    /// whole store rather than per database, so asking whether one is still
    /// attached is a question no single table can answer.
    ///
    /// # Errors
    ///
    /// Returns an error when a stored definition cannot be read.
    pub fn fields(&self) -> Result<Vec<FieldDefinition>> {
        let mut found = Vec::new();
        for (_, bytes) in self.transaction.scan_table(
            system::SYSTEM_NAMESPACE,
            system::SYSTEM_DATABASE,
            system::FIELDS,
        )? {
            found.push(FieldDefinition::from_value(&decode_payload(&bytes)?)?);
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
            required: false,
            default: None,
            assert: None,
            analyzer: None,
        }
    }

    #[test]
    fn a_declaration_round_trips_everything_it_declares() {
        let mut original = definition(FieldKind::Datetime);
        original.required = true;
        original.default = Some("time::now()".to_owned());
        original.analyzer = Some("simple".to_owned());
        assert_eq!(
            FieldDefinition::from_value(&original.to_value()).unwrap(),
            original
        );
    }

    #[test]
    fn a_declaration_written_before_either_existed_requires_nothing_and_fills_nothing() {
        // Every definition on disk today predates both, so nothing migrates.
        let fields = BTreeMap::from([
            (FIELD_ID.to_owned(), number(4)),
            (FIELD_NAMESPACE.to_owned(), number(1)),
            (FIELD_DATABASE.to_owned(), number(2)),
            (FIELD_TABLE.to_owned(), number(3)),
            (FIELD_NAME.to_owned(), Value::from("email")),
            (FIELD_KIND.to_owned(), Value::from("string")),
        ]);
        let read = FieldDefinition::from_value(&Value::Object(fields)).unwrap();
        assert!(!read.required);
        assert_eq!(read.default, None);
    }

    #[test]
    fn a_stored_default_that_is_not_text_is_corruption() {
        let mut fields = BTreeMap::from([
            (FIELD_ID.to_owned(), number(4)),
            (FIELD_NAMESPACE.to_owned(), number(1)),
            (FIELD_DATABASE.to_owned(), number(2)),
            (FIELD_TABLE.to_owned(), number(3)),
            (FIELD_NAME.to_owned(), Value::from("email")),
            (FIELD_KIND.to_owned(), Value::from("string")),
        ]);
        fields.insert(FIELD_DEFAULT.to_owned(), Value::Bool(true));
        let error = FieldDefinition::from_value(&Value::Object(fields)).unwrap_err();
        assert_eq!(error.code(), "corruption");
    }

    #[test]
    fn a_definition_of_every_kind_round_trips() {
        for kind in FieldKind::all() {
            let original = definition(kind.clone());
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
            // The point of the fixture is a kind written by a *newer* binary, so
            // the word has to be one no `FieldKind` holds. `geometry` stood here
            // until the value system gained the type and quietly turned this
            // test into an assertion that a real kind is corruption.
            fields.insert(FIELD_KIND.to_owned(), Value::from("tesseract"));
        }
        let error = FieldDefinition::from_value(&stored).unwrap_err();
        assert_eq!(error.code(), "corruption");
    }

    #[test]
    fn every_kind_this_binary_knows_reads_back_as_itself() {
        // The other half, and the reason the test above could rot unnoticed:
        // nothing asserted that the known kinds *are* known, so adding one broke
        // a negative fixture with no positive one to contradict it.
        for kind in FieldKind::all() {
            let stored = definition(kind.clone()).to_value();
            let read = FieldDefinition::from_value(&stored);
            assert!(
                read.is_ok(),
                "{} did not read back: {:?}",
                kind.name(),
                read.as_ref().err()
            );
            // The assertion above is what fails the test; this only unwraps what
            // it has already established, without a `panic!` the lints refuse.
            let Ok(read) = read else { continue };
            assert_eq!(
                read.kind,
                *kind,
                "{} read back as another kind",
                kind.name()
            );
        }
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
