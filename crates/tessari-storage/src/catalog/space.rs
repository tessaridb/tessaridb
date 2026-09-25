//! A key-value space as its own table kind, and the limit it may carry (G036).
//!
//! A space used to be declared as a plain schemaless table, so `INFO` wrote it
//! back as `DEFINE TABLE … SCHEMALESS` and the declaring word was lost. It is its
//! own kind now for that reason and for the one that made it necessary: a space
//! may carry a **limit**, a count of keys it holds at most, and a rule for what
//! happens at the limit.
//!
//! # A count of keys, not a size in bytes
//!
//! A count is exact, the same on every node and free to keep — the store already
//! counts each table's records in every commit. A size in bytes is none of those
//! on a log-structured store, where the bytes a key occupies depend on versions
//! and compaction nobody asked about.

use std::collections::BTreeMap;

use tessari_types::{Number, Value};

use super::definition::object;
use crate::error::{Error, Result};

/// What a space does when a commit would take it past its limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Eviction {
    /// Remove the least recently modified keys — the ones whose newest version
    /// is oldest — until the space is back at its limit. The default.
    Modified,
    /// Refuse the commit (`SpaceFull`).
    Refuse,
}

/// The most keys a space holds, and what it does at that point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpaceLimit {
    /// Never zero: a space that may hold nothing is not a space.
    pub max: u64,
    /// What a commit that would pass `max` does.
    pub eviction: Eviction,
}

/// A space's declaration: `None` for a space with no limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SpaceDeclaration {
    /// The limit, when the space declared one.
    pub limit: Option<SpaceLimit>,
}

const ENTITY: &str = "space";
const FIELD_MAX: &str = "max";
const FIELD_EVICT: &str = "evict";
const EVICT_MODIFIED: &str = "modified";
const EVICT_NONE: &str = "none";

impl SpaceDeclaration {
    /// The value written inside the table's catalog entry.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut fields = BTreeMap::new();
        if let Some(limit) = self.limit {
            let max = i64::try_from(limit.max).unwrap_or(i64::MAX);
            fields.insert(FIELD_MAX.to_owned(), Value::Number(Number::Integer(max)));
            let evict = match limit.eviction {
                Eviction::Modified => EVICT_MODIFIED,
                Eviction::Refuse => EVICT_NONE,
            };
            fields.insert(FIELD_EVICT.to_owned(), Value::from(evict));
        }
        Value::Object(fields)
    }

    /// Read a declaration back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when a limit is present but is not a
    /// positive integer, or names a rule this build does not have.
    pub fn from_value(value: &Value) -> Result<Self> {
        let fields = object(value, ENTITY)?;
        let Some(max) = fields.get(FIELD_MAX) else {
            return Ok(Self::default());
        };
        let max = match max {
            Value::Number(Number::Integer(max)) if *max > 0 => u64::try_from(*max).ok(),
            _ => None,
        }
        .ok_or(Error::CatalogMalformed {
            entity: ENTITY,
            field: FIELD_MAX,
            found: "not a positive integer",
        })?;
        let eviction = match fields.get(FIELD_EVICT) {
            Some(Value::String(rule)) if rule == EVICT_MODIFIED => Eviction::Modified,
            Some(Value::String(rule)) if rule == EVICT_NONE => Eviction::Refuse,
            _ => {
                return Err(Error::CatalogMalformed {
                    entity: ENTITY,
                    field: FIELD_EVICT,
                    found: "an eviction rule this build does not have",
                });
            }
        };
        Ok(Self {
            limit: Some(SpaceLimit { max, eviction }),
        })
    }
}

#[cfg(test)]
mod tests;
