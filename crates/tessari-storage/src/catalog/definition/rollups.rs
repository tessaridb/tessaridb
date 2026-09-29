//! What a rollup declaration holds (ADR-0088 §6 and its W6 amendment).
//!
//! A rollup is carried by its **source** series, as a list: the writer that
//! maintains it reads the list from the catalog at its own snapshot, so the
//! list may change without the per-process series registry — whose entries
//! assume a table's kind never changes — having to learn anything.

use std::collections::BTreeMap;

use tessari_types::{Duration, TableId, Value};

use super::{field_id, number, object};
use crate::error::{Error, Result};

const ENTITY: &str = "rollup";
const FIELD_TABLE: &str = "table";
const FIELD_WINDOW: &str = "window";
const FIELD_BY: &str = "by";
const FIELD_COMPUTES: &str = "computes";
const FIELD_NAME: &str = "name";
const FIELD_FOLD: &str = "fold";
const FIELD_OF: &str = "of";

/// The folds a rollup keeps: the ones that merge exactly from the row alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RollupFold {
    /// `count(*)`, or `count(f)` for the records holding `f`.
    Count,
    /// `sum(f)`.
    Sum,
    /// `min(f)`.
    Min,
    /// `max(f)`.
    Max,
}

impl RollupFold {
    /// How the fold is written.
    #[must_use]
    pub const fn spelling(self) -> &'static str {
        match self {
            Self::Count => "count",
            Self::Sum => "sum",
            Self::Min => "min",
            Self::Max => "max",
        }
    }

    /// The fold a word spells, if a rollup keeps it.
    #[must_use]
    pub fn parse(word: &str) -> Option<Self> {
        [Self::Count, Self::Sum, Self::Min, Self::Max]
            .into_iter()
            .find(|fold| fold.spelling().eq_ignore_ascii_case(word))
    }
}

/// One `COMPUTE fold(field) AS name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollupCompute {
    /// The field of the rollup row it answers in.
    pub name: String,
    /// The fold.
    pub fold: RollupFold,
    /// The raw record's field it folds, or `None` for `count(*)`.
    pub of: Option<String>,
}

/// One rollup of a series: where it is kept and what it computes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollupDeclaration {
    /// The rollup's own table — an event-time series ordered by `window`.
    pub table: TableId,
    /// The width of a window, whole seconds.
    pub window: Duration,
    /// The raw field a row is kept per, when the rollup is `BY` one.
    pub by: Option<String>,
    /// What each row computes, in the order written.
    pub computes: Vec<RollupCompute>,
}

impl RollupDeclaration {
    /// The value written inside the source table's catalog entry.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let computes = self
            .computes
            .iter()
            .map(|compute| {
                let mut fields = BTreeMap::from([
                    (FIELD_NAME.to_owned(), Value::String(compute.name.clone())),
                    (
                        FIELD_FOLD.to_owned(),
                        Value::String(compute.fold.spelling().to_owned()),
                    ),
                ]);
                if let Some(of) = &compute.of {
                    fields.insert(FIELD_OF.to_owned(), Value::String(of.clone()));
                }
                Value::Object(fields)
            })
            .collect();
        let mut fields = BTreeMap::from([
            (FIELD_TABLE.to_owned(), number(self.table.get())),
            (FIELD_WINDOW.to_owned(), Value::Duration(self.window)),
            (FIELD_COMPUTES.to_owned(), Value::Array(computes)),
        ]);
        if let Some(by) = &self.by {
            fields.insert(FIELD_BY.to_owned(), Value::String(by.clone()));
        }
        Value::Object(fields)
    }

    /// Read a declaration back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when a field is missing or of the
    /// wrong kind.
    pub fn from_value(value: &Value) -> Result<Self> {
        let fields = object(value, ENTITY)?;
        let table = TableId::new(field_id(fields, FIELD_TABLE, ENTITY)?);
        let Some(Value::Duration(window)) = fields.get(FIELD_WINDOW) else {
            return Err(malformed(FIELD_WINDOW, fields.get(FIELD_WINDOW)));
        };
        let by = text(fields, FIELD_BY)?;
        let Some(Value::Array(held)) = fields.get(FIELD_COMPUTES) else {
            return Err(malformed(FIELD_COMPUTES, fields.get(FIELD_COMPUTES)));
        };
        let mut computes = Vec::with_capacity(held.len());
        for compute in held {
            let fields = object(compute, ENTITY)?;
            let Some(name) = text(fields, FIELD_NAME)? else {
                return Err(malformed(FIELD_NAME, None));
            };
            let fold = text(fields, FIELD_FOLD)?
                .as_deref()
                .and_then(RollupFold::parse)
                .ok_or_else(|| malformed(FIELD_FOLD, fields.get(FIELD_FOLD)))?;
            computes.push(RollupCompute {
                name,
                fold,
                of: text(fields, FIELD_OF)?,
            });
        }
        Ok(Self {
            table,
            window: *window,
            by,
            computes,
        })
    }
}

/// An optional text field.
fn text(fields: &BTreeMap<String, Value>, field: &'static str) -> Result<Option<String>> {
    match fields.get(field) {
        None => Ok(None),
        Some(Value::String(held)) => Ok(Some(held.clone())),
        Some(other) => Err(malformed(field, Some(other))),
    }
}

fn malformed(field: &'static str, found: Option<&Value>) -> Error {
    Error::CatalogMalformed {
        entity: ENTITY,
        field,
        found: found.map_or("none", Value::type_name),
    }
}
