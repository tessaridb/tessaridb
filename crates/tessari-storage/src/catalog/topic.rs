//! A topic as its own table kind: an append-only order of messages (G037).
//!
//! A topic keeps every message it is given, in the order the messages were
//! committed, and hands each a position from 1. It declares three optional
//! things: how long a message is kept, the most bytes one message may encode
//! to, and whether a caller nobody signed in may append — and how often.
//!
//! # Why an anonymous append names its rate in the declaration
//!
//! A closed store refuses every caller that has not signed in. Opening one
//! table to them is a door, and a door with no limit is a way to fill the
//! store from anywhere. So the rate and the size are part of what opens it:
//! the statement that says "anyone may append" is refused unless it also says
//! how much and how often, and there is no default to forget.

use std::collections::BTreeMap;

use tessari_types::{Duration, Number, Value};

use super::definition::object;
use crate::error::{Error, Result};

/// How often a caller nobody signed in may append.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublicAppend {
    /// Appends allowed per `per`, on each node. Never zero.
    pub rate: u64,
    /// The window the rate counts in. Never zero.
    pub per: Duration,
}

/// A topic's declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TopicDeclaration {
    /// How long a message is kept after it was appended, or forever.
    pub retain: Option<Duration>,
    /// The most bytes one message may encode to, or no bound.
    pub max_bytes: Option<u64>,
    /// Whether, and how often, an anonymous caller may append.
    pub public: Option<PublicAppend>,
}

const ENTITY: &str = "topic";
const FIELD_RETAIN: &str = "retain";
const FIELD_MAX_BYTES: &str = "max_bytes";
const FIELD_PUBLIC_RATE: &str = "public_rate";
const FIELD_PUBLIC_PER: &str = "public_per";

fn positive(fields: &BTreeMap<String, Value>, field: &'static str) -> Result<Option<u64>> {
    match fields.get(field) {
        None => Ok(None),
        Some(Value::Number(Number::Integer(held))) if *held > 0 => Ok(u64::try_from(*held).ok()),
        Some(_) => Err(Error::CatalogMalformed {
            entity: ENTITY,
            field,
            found: "not a positive integer",
        }),
    }
}

fn duration(fields: &BTreeMap<String, Value>, field: &'static str) -> Result<Option<Duration>> {
    match fields.get(field) {
        None => Ok(None),
        Some(Value::Duration(held)) => Ok(Some(*held)),
        Some(other) => Err(Error::CatalogMalformed {
            entity: ENTITY,
            field,
            found: other.type_name(),
        }),
    }
}

fn integer(count: u64) -> Value {
    Value::Number(Number::Integer(i64::try_from(count).unwrap_or(i64::MAX)))
}

impl TopicDeclaration {
    /// The value written inside the table's catalog entry.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut fields = BTreeMap::new();
        if let Some(retain) = self.retain {
            fields.insert(FIELD_RETAIN.to_owned(), Value::Duration(retain));
        }
        if let Some(max) = self.max_bytes {
            fields.insert(FIELD_MAX_BYTES.to_owned(), integer(max));
        }
        if let Some(public) = self.public {
            fields.insert(FIELD_PUBLIC_RATE.to_owned(), integer(public.rate));
            fields.insert(FIELD_PUBLIC_PER.to_owned(), Value::Duration(public.per));
        }
        Value::Object(fields)
    }

    /// Read a declaration back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when a field holds the wrong type, or
    /// when a public rate is stored without its window or the other way round.
    pub fn from_value(value: &Value) -> Result<Self> {
        let fields = object(value, ENTITY)?;
        let public = match (
            positive(fields, FIELD_PUBLIC_RATE)?,
            duration(fields, FIELD_PUBLIC_PER)?,
        ) {
            (None, None) => None,
            (Some(rate), Some(per)) => Some(PublicAppend { rate, per }),
            _ => {
                return Err(Error::CatalogMalformed {
                    entity: ENTITY,
                    field: FIELD_PUBLIC_RATE,
                    found: "a public rate without its window",
                });
            }
        };
        Ok(Self {
            retain: duration(fields, FIELD_RETAIN)?,
            max_bytes: positive(fields, FIELD_MAX_BYTES)?,
            public,
        })
    }
}

#[cfg(test)]
mod tests;
