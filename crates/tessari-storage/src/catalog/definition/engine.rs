//! What one table contributes to a `DEFINE SEARCH` (ADR-0105).
//!
//! A search is a database-level name over **member indexes**, one per table.
//! Everything a member needs to maintain its postings and to be read back as
//! part of the search travels on the member itself — the search's name, its
//! analyzer and stop words, and each field's weight and options — so the
//! index path keeps it in step with the table's records exactly as it keeps a
//! field index, and a member never has to consult a second catalog entry to be
//! written.

use std::collections::BTreeMap;

use tessari_types::{Number, Value};

use crate::error::{Error, Result};

const ENTITY: &str = "search member";

/// One table's part of a search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineMember {
    /// The search's name, which every member of one search carries.
    pub search: String,
    /// The analyzer every member field and every query is read with.
    pub analyzer: String,
    /// The stop-word set a query drops, when the search names one.
    pub stopwords: Option<String>,
    /// Each field's weight and options, in the order of the index's fields.
    pub fields: Vec<EngineField>,
}

/// One field of a member: how much it weighs and what it answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineField {
    /// The BM25F weight in thousandths, so a definition stays `Eq` and a
    /// weight written `2.5` is stored as exactly `2500`.
    pub weight: u32,
    /// Whether a fuzzy word may be answered here.
    pub fuzzy: bool,
    /// Whether a prefix word may be answered here.
    pub prefix: bool,
    /// Whether a quoted phrase may be answered here.
    pub phrase: bool,
    /// The synonym set this field also answers a word with.
    pub synonyms: Option<String>,
    /// Whether `search::snippet()` may take its window from this field.
    pub snippet: bool,
}

/// The weight a field has when none is written: one, in thousandths.
pub const UNIT_WEIGHT: u32 = 1000;

impl EngineMember {
    /// The value stored under the index definition's `engine` field.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut fields = BTreeMap::from([
            ("search".to_owned(), Value::from(self.search.as_str())),
            ("analyzer".to_owned(), Value::from(self.analyzer.as_str())),
            (
                "fields".to_owned(),
                Value::Array(self.fields.iter().map(EngineField::to_value).collect()),
            ),
        ]);
        if let Some(stopwords) = &self.stopwords {
            fields.insert("stopwords".to_owned(), Value::from(stopwords.as_str()));
        }
        Value::Object(fields)
    }

    /// Read a member back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when a part is missing or of the
    /// wrong type.
    pub fn from_value(value: &Value) -> Result<Self> {
        let Value::Object(fields) = value else {
            return Err(malformed("engine", value.type_name()));
        };
        let Some(Value::Array(declared)) = fields.get("fields") else {
            return Err(malformed("fields", "none"));
        };
        Ok(Self {
            search: text(fields, "search")?,
            analyzer: text(fields, "analyzer")?,
            stopwords: optional_text(fields, "stopwords")?,
            fields: declared
                .iter()
                .map(EngineField::from_value)
                .collect::<Result<Vec<_>>>()?,
        })
    }
}

impl EngineField {
    fn to_value(&self) -> Value {
        let mut fields = BTreeMap::from([
            (
                "weight".to_owned(),
                Value::Number(Number::Integer(i64::from(self.weight))),
            ),
            ("fuzzy".to_owned(), Value::Bool(self.fuzzy)),
            ("prefix".to_owned(), Value::Bool(self.prefix)),
            ("phrase".to_owned(), Value::Bool(self.phrase)),
            ("snippet".to_owned(), Value::Bool(self.snippet)),
        ]);
        if let Some(synonyms) = &self.synonyms {
            fields.insert("synonyms".to_owned(), Value::from(synonyms.as_str()));
        }
        Value::Object(fields)
    }

    fn from_value(value: &Value) -> Result<Self> {
        let Value::Object(fields) = value else {
            return Err(malformed("fields", value.type_name()));
        };
        let weight = match fields.get("weight") {
            Some(Value::Number(Number::Integer(weight))) => u32::try_from(*weight)
                .ok()
                .filter(|weight| *weight > 0)
                .ok_or_else(|| malformed("weight", "not a positive weight"))?,
            other => return Err(malformed("weight", other.map_or("none", Value::type_name))),
        };
        let flag = |name: &'static str| match fields.get(name) {
            Some(Value::Bool(held)) => Ok(*held),
            other => Err(malformed(name, other.map_or("none", Value::type_name))),
        };
        Ok(Self {
            weight,
            fuzzy: flag("fuzzy")?,
            prefix: flag("prefix")?,
            phrase: flag("phrase")?,
            snippet: flag("snippet")?,
            synonyms: optional_text(fields, "synonyms")?,
        })
    }
}

fn malformed(field: &'static str, found: &'static str) -> Error {
    Error::CatalogMalformed {
        entity: ENTITY,
        field,
        found,
    }
}

fn text(fields: &BTreeMap<String, Value>, field: &'static str) -> Result<String> {
    match fields.get(field) {
        Some(Value::String(held)) => Ok(held.clone()),
        other => Err(malformed(field, other.map_or("none", Value::type_name))),
    }
}

fn optional_text(fields: &BTreeMap<String, Value>, field: &'static str) -> Result<Option<String>> {
    match fields.get(field) {
        None => Ok(None),
        Some(Value::String(held)) => Ok(Some(held.clone())),
        Some(other) => Err(malformed(field, other.type_name())),
    }
}
