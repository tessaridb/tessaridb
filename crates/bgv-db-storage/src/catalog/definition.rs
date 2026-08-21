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

use bgv_db_types::{DatabaseId, IndexId, NamespaceId, Number, Path, TableId, Value};

use crate::error::{Error, Result};

const FIELD_ID: &str = "id";
const FIELD_NAME: &str = "name";
const FIELD_NAMESPACE: &str = "namespace";
const FIELD_DATABASE: &str = "database";
const FIELD_TABLE: &str = "table";
const FIELD_FIELDS: &str = "fields";
const FIELD_UNIQUE: &str = "unique";
const FIELD_SEARCH: &str = "search";
const FIELD_VECTOR: &str = "vector";
const FIELD_SCHEMAFULL: &str = "schemafull";
const FIELD_EDGE: &str = "edge";

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
    /// Whether the table refuses a field it has no definition for.
    ///
    /// False is the schemaless default: declared fields are constrained and
    /// anything else passes. True is what turns a set of field declarations into
    /// a schema, because the mistake worth catching — a misspelled field name —
    /// writes a field nobody declared.
    pub schemafull: bool,
    /// Whether the table holds edges rather than plain records.
    ///
    /// An edge table is an ordinary table whose records carry `out` and `in`
    /// record references, and which carries an index on each of them — so that
    /// traversal is an index read rather than a scan, without the caller having
    /// had to know to declare those indexes. The flag is what `RELATE` checks
    /// before writing, because an edge nothing can traverse to is worse than a
    /// refusal.
    pub edge: bool,
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
            (FIELD_SCHEMAFULL.to_owned(), Value::Bool(self.schemafull)),
            (FIELD_EDGE.to_owned(), Value::Bool(self.edge)),
        ]))
    }

    /// Read a definition back.
    ///
    /// An entry written before a flag existed does not carry it, and reads as
    /// `false` — which is what such a table is. Refusing it instead would make an
    /// added property unreadable rather than absent.
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
            schemafull: flag(fields, FIELD_SCHEMAFULL, "table")?,
            edge: flag(fields, FIELD_EDGE, "table")?,
        })
    }
}

/// What kind of table to create.
///
/// A struct rather than two `bool` parameters in a row, for the reason
/// [`bgv_db_types::TableId`] is a newtype rather than a `u32`: `create_table(ns,
/// db, "follows", false, true)` compiles just as well with the pair transposed,
/// and would create a schemafull table where an edge table was meant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TableShape {
    /// Refuse a field the table has no declaration for.
    pub schemafull: bool,
    /// Hold edges: records carrying `out` and `in`, each with an index.
    pub edge: bool,
}

/// What a `DEFINE INDEX` says beyond which values it projects.
///
/// A struct rather than two booleans, for the reason `ids.rs` gives for
/// newtyping a `u32`: `create_index(id, "by_body", fields, false, true)`
/// compiles just as well transposed, and would make a unique index where a
/// search index was meant.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IndexShape {
    /// Whether a value may appear more than once.
    pub unique: bool,
    /// Whether the index holds terms rather than whole values.
    pub search: bool,
    /// The distance a vector index is built with, when it is one.
    pub vector: Option<VectorDistance>,
}

/// Which distance a vector index's graph is built and searched with.
///
/// **The index declares it, and there is no default**, because a default would
/// silently decide which queries the index can serve. A graph whose edges were
/// chosen by one distance approximates that distance and no other: cosine
/// measures an angle and euclidean measures a separation, and for vectors nobody
/// normalised they rank differently. Serving a cosine query from a euclidean
/// graph would return plausible neighbours that are not the nearest — the exact
/// failure this whole node is arranged to prevent.
///
/// A read whose distance does not match the index's gets the scan, which is
/// exact, and says so through the access path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VectorDistance {
    /// The angle between two vectors, as `1 - cos θ`.
    Cosine,
    /// The distance between two points.
    Euclidean,
}

impl VectorDistance {
    /// How it is written in a definition, and stored in the catalog.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Cosine => "cosine",
            Self::Euclidean => "euclidean",
        }
    }

    /// The distance this word names.
    #[must_use]
    pub fn parse(word: &str) -> Option<Self> {
        match word {
            "cosine" => Some(Self::Cosine),
            "euclidean" => Some(Self::Euclidean),
            _ => None,
        }
    }
}

/// An index on a table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexDefinition {
    /// The index's id, which every entry's key carries.
    pub id: IndexId,
    /// The namespace it belongs to.
    pub namespace: NamespaceId,
    /// The database it belongs to.
    pub database: DatabaseId,
    /// The table it is on.
    pub table: TableId,
    /// Its name, unique within that table.
    pub name: String,
    /// The values it indexes, in the order they are encoded.
    ///
    /// A path rather than a name, because an index may project a value nested
    /// inside the record: `address.city` is as indexable as `email`.
    ///
    /// Order is part of the index's identity: an index on `(a, b)` answers a
    /// query about `a` and one on `(b, a)` does not.
    pub fields: Vec<Path>,
    /// Whether this index holds **terms** rather than whole values.
    ///
    /// A search index projects one posting per term the analyzer finds, where an
    /// ordered index projects one entry per record. Which analyzer is used is
    /// the **field's** declaration, not this index's — see
    /// [`bgv_db_types::Analyzer`] for why that distinction is the whole design.
    pub search: bool,
    /// Whether a value may appear more than once.
    ///
    /// A unique index enforces it through the key layout — its entries carry no
    /// record id, so a second record with the same value writes the same key.
    pub unique: bool,
    /// The distance this index's graph is built with, when it is a vector index.
    ///
    /// It answers "which records are nearest this one" and nothing else, the way
    /// a search index answers a term and nothing else. It is the one index in
    /// this store whose answer is **approximate**, which is why a statement has
    /// to ask for it by name before it may serve one — and why the distance is
    /// declared rather than assumed.
    pub vector: Option<VectorDistance>,
}

impl IndexDefinition {
    /// The value written to the catalog.
    #[must_use]
    pub fn to_value(&self) -> Value {
        Value::Object(BTreeMap::from([
            (FIELD_ID.to_owned(), number(self.id.get())),
            (FIELD_NAMESPACE.to_owned(), number(self.namespace.get())),
            (FIELD_DATABASE.to_owned(), number(self.database.get())),
            (FIELD_TABLE.to_owned(), number(self.table.get())),
            (FIELD_NAME.to_owned(), Value::from(self.name.as_str())),
            (
                FIELD_FIELDS.to_owned(),
                Value::Array(
                    self.fields
                        .iter()
                        .map(|field| Value::from(field.to_string().as_str()))
                        .collect(),
                ),
            ),
            (FIELD_UNIQUE.to_owned(), Value::Bool(self.unique)),
            (FIELD_SEARCH.to_owned(), Value::Bool(self.search)),
            (
                FIELD_VECTOR.to_owned(),
                self.vector
                    .map_or(Value::None, |held| Value::from(held.name())),
            ),
        ]))
    }

    /// Read a definition back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when a field is missing or holds the
    /// wrong type.
    pub fn from_value(value: &Value) -> Result<Self> {
        let fields = object(value, "index")?;
        let malformed = |field: &'static str, found: &'static str| Error::CatalogMalformed {
            entity: "index",
            field,
            found,
        };
        let Some(Value::Array(names)) = fields.get(FIELD_FIELDS) else {
            return Err(malformed(
                FIELD_FIELDS,
                fields.get(FIELD_FIELDS).map_or("none", Value::type_name),
            ));
        };
        let indexed = names
            .iter()
            .map(|name| match name {
                // Stored as the text it was written as, so a definition made
                // before paths existed reads back as a one-step path and a dump
                // stays legible. Text that is not a path is corruption rather
                // than a bad request: nothing that reached the catalog could
                // have been one.
                Value::String(text) => {
                    Path::parse(text).ok_or_else(|| malformed(FIELD_FIELDS, "an unreadable path"))
                }
                other => Err(malformed(FIELD_FIELDS, other.type_name())),
            })
            .collect::<Result<Vec<Path>>>()?;
        let Some(Value::Bool(unique)) = fields.get(FIELD_UNIQUE) else {
            return Err(malformed(
                FIELD_UNIQUE,
                fields.get(FIELD_UNIQUE).map_or("none", Value::type_name),
            ));
        };
        Ok(Self {
            id: IndexId::new(field_id(fields, FIELD_ID, "index")?),
            namespace: NamespaceId::new(field_id(fields, FIELD_NAMESPACE, "index")?),
            database: DatabaseId::new(field_id(fields, FIELD_DATABASE, "index")?),
            table: TableId::new(field_id(fields, FIELD_TABLE, "index")?),
            name: field_name(fields, "index")?,
            fields: indexed,
            unique: *unique,
            // An index written before search existed is an ordered one, so
            // nothing on disk has to be migrated.
            search: flag(fields, FIELD_SEARCH, "index")?,
            // An index written before vector indexes existed holds no such
            // field and is not one, the same way one written before search was
            // an ordered index.
            vector: match fields.get(FIELD_VECTOR) {
                Some(Value::String(word)) => Some(VectorDistance::parse(word).ok_or_else(
                    || Error::CatalogMalformed {
                        entity: "index",
                        field: FIELD_VECTOR,
                        found: "a name that is not a distance",
                    },
                )?),
                _ => None,
            },
        })
    }
}

/// An identifier as it is stored: an integer, widened rather than cast.
pub(crate) fn number(id: u32) -> Value {
    Value::Number(Number::Integer(i64::from(id)))
}

pub(crate) fn object<'a>(
    value: &'a Value,
    entity: &'static str,
) -> Result<&'a BTreeMap<String, Value>> {
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

/// A boolean property of a definition.
///
/// Absent reads as `false`, so an entry written before the property existed is
/// readable rather than refused. A value of the wrong type is **not** read as
/// `false`: something wrote a well-formed value that is not a flag, which is an
/// integrity problem, and defaulting it would silently drop a constraint.
///
/// One reader for every flag, because a second copy that drifted would not fail
/// to compile — it would change what a table is.
pub(crate) fn flag(
    fields: &BTreeMap<String, Value>,
    field: &'static str,
    entity: &'static str,
) -> Result<bool> {
    match fields.get(field) {
        Some(Value::Bool(declared)) => Ok(*declared),
        None => Ok(false),
        Some(other) => Err(Error::CatalogMalformed {
            entity,
            field,
            found: other.type_name(),
        }),
    }
}

pub(crate) fn field_name(fields: &BTreeMap<String, Value>, entity: &'static str) -> Result<String> {
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
            schemafull: true,
            edge: false,
        };
        assert_eq!(
            TableDefinition::from_value(&table.to_value()).unwrap(),
            table
        );
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
            vector: None,
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
