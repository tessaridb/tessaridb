//! What a table declares about how text becomes terms.
//!
//! An analyzer is a catalog entry like any other — an ordinary record in the
//! system tenancy (ADR-0009) — so declaring one takes part in the transaction
//! that issued it and replicates through the same apply path.
//!
//! It is **named and free-standing** rather than written inline on a field,
//! because two fields that must be searched the same way have to be able to say
//! so once. A field then attaches one by name, and the store's search reads it
//! from the schema rather than from any index — see [`tessari_types::Analyzer`]
//! for why that distinction is the whole design.

use std::collections::BTreeMap;

use tessari_encoding::decode_payload;
use tessari_types::{Analyzer, Filter, RecordId, Value};

use super::definition::{field_id, field_name, number, object};
use super::{Catalog, Level, id_key, qualify, system};
use crate::error::{Error, Result};

const FIELD_ID: &str = "id";
const FIELD_NAME: &str = "name";
const FIELD_FILTERS: &str = "filters";

const ENTITY: &str = "analyzer";

/// A named analyzer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalyzerDefinition {
    /// Its id.
    pub id: u32,
    /// Its name, unique across the store.
    ///
    /// Not scoped to a database, because an analyzer describes a language
    /// rather than a tenant's data, and a store that had to redeclare
    /// `lowercase` per database would make every schema longer for nothing.
    pub name: String,
    /// How it turns text into terms.
    pub analyzer: Analyzer,
}

impl AnalyzerDefinition {
    /// The value written to the catalog.
    ///
    /// Filters are stored by **name**, in order, for the reason a field kind is
    /// stored by spelling: reordering the enum must not silently reinterpret a
    /// stored definition, and the order of the chain is part of what it means.
    #[must_use]
    pub fn to_value(&self) -> Value {
        Value::Object(BTreeMap::from([
            (FIELD_ID.to_owned(), number(self.id)),
            (FIELD_NAME.to_owned(), Value::from(self.name.as_str())),
            (
                FIELD_FILTERS.to_owned(),
                Value::Array(
                    self.analyzer
                        .filters()
                        .iter()
                        .map(|filter| Value::from(filter.name()))
                        .collect(),
                ),
            ),
        ]))
    }

    /// Read a definition back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when a field is missing, holds the
    /// wrong type, or names a filter this binary does not know.
    pub fn from_value(value: &Value) -> Result<Self> {
        let fields = object(value, ENTITY)?;
        let malformed = |found: &'static str| Error::CatalogMalformed {
            entity: ENTITY,
            field: FIELD_FILTERS,
            found,
        };
        let Some(Value::Array(names)) = fields.get(FIELD_FILTERS) else {
            return Err(malformed(
                fields.get(FIELD_FILTERS).map_or("none", Value::type_name),
            ));
        };
        let filters = names
            .iter()
            .map(|name| match name {
                // An unknown filter is corruption rather than a bad request: it
                // was written by something that knew a filter this binary does
                // not, and guessing would analyse text differently from the
                // store that wrote it.
                Value::String(text) => {
                    Filter::parse(text).ok_or_else(|| malformed("an unknown filter"))
                }
                other => Err(malformed(other.type_name())),
            })
            .collect::<Result<Vec<Filter>>>()?;
        Ok(Self {
            id: field_id(fields, FIELD_ID, ENTITY)?,
            name: field_name(fields, ENTITY)?,
            analyzer: Analyzer::new(filters),
        })
    }
}

impl Catalog<'_, '_> {
    /// Declare an analyzer.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NameTaken`] when the name is already declared.
    pub fn create_analyzer(
        &mut self,
        name: &str,
        analyzer: Analyzer,
    ) -> Result<AnalyzerDefinition> {
        let qualified = qualify(Level::Analyzer, &[], name);
        self.reserve_name(&qualified)?;
        let id = self.allocate(Level::Analyzer)?;
        let definition = AnalyzerDefinition {
            id,
            name: name.to_owned(),
            analyzer,
        };
        self.write(system::ANALYZERS, id, &definition.to_value());
        self.claim_name(&qualified, id);
        Ok(definition)
    }

    /// Look an analyzer up by id.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored definition cannot be read.
    pub fn analyzer(&self, id: u32) -> Result<Option<AnalyzerDefinition>> {
        self.read(system::ANALYZERS, id)?
            .as_ref()
            .map(AnalyzerDefinition::from_value)
            .transpose()
    }

    /// Remove an analyzer's declaration and release its name.
    ///
    /// Answers `false` when there was nothing under that id, so the caller can
    /// tell "removed" from "was not there" without a second read.
    ///
    /// **Whether anything still attaches this analyzer is not asked here.** A
    /// field names its analyzer by name rather than by id
    /// (see [`super::FieldDefinition::analyzer`]), so nothing in the catalog
    /// enforces the link and nothing here can. The refusal lives with the
    /// statement, where the span that names the offending field lives too —
    /// the same division [`Self::drop_table`] keeps by not removing the records
    /// of the table it drops.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored definition cannot be read.
    pub fn drop_analyzer(&mut self, id: u32) -> Result<bool> {
        let Some(definition) = self.analyzer(id)? else {
            return Ok(false);
        };
        let qualified = qualify(Level::Analyzer, &[], &definition.name);
        self.transaction.delete(system::address(
            system::ANALYZERS,
            RecordId::Int(id_key(id)),
        ));
        self.transaction
            .delete(system::address(system::NAMES, RecordId::from(qualified)));
        Ok(true)
    }

    /// Every declared analyzer.
    ///
    /// # Errors
    ///
    /// Returns an error when a stored definition cannot be read.
    pub fn analyzers(&self) -> Result<Vec<AnalyzerDefinition>> {
        let mut found = Vec::new();
        for (_, payload) in self.transaction.scan_table(
            system::SYSTEM_NAMESPACE,
            system::SYSTEM_DATABASE,
            system::ANALYZERS,
        )? {
            found.push(AnalyzerDefinition::from_value(&decode_payload(&payload)?)?);
        }
        Ok(found)
    }
}
