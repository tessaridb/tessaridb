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
    /// `IN MEMORY`: the space's keys live in node memory only — no log record,
    /// no copy on a follower, none in a backup, none after a restart (ADR-0130).
    pub memory: bool,
}

const ENTITY: &str = "space";
const FIELD_MAX: &str = "max";
const FIELD_EVICT: &str = "evict";
const FIELD_MEMORY: &str = "memory";
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
        if self.memory {
            fields.insert(FIELD_MEMORY.to_owned(), Value::Bool(true));
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
        let memory = match fields.get(FIELD_MEMORY) {
            None => false,
            Some(Value::Bool(memory)) => *memory,
            Some(_) => {
                return Err(Error::CatalogMalformed {
                    entity: ENTITY,
                    field: FIELD_MEMORY,
                    found: "not a boolean",
                });
            }
        };
        let Some(max) = fields.get(FIELD_MAX) else {
            return Ok(Self {
                limit: None,
                memory,
            });
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
            memory,
        })
    }
}

#[cfg(test)]
mod tests;
