//! That a table's records expire (ADR-0122 A1), as the table definition stores
//! it.
//!
//! Written only when a table declared it, so a table that said nothing encodes
//! exactly as it did before the clause existed.

use std::collections::BTreeMap;

use tessari_types::{Duration, Value};

use crate::error::{Error, Result};

const AFTER: &str = "after";
const RETIRED: &str = "retired";

/// A table's expiry declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableExpiry {
    /// The lifetime a record gets when it is created and its write names none.
    ///
    /// `None` is a bare `EXPIRE`: a record expires only when a write says so.
    pub after: Option<Duration>,
    /// Whether `DROP EXPIRE` retired the declaration (ADR-0122 A6).
    ///
    /// A retired table stamps no default and takes no new expiry, while the
    /// instants its records already carry stand — so it is still a table whose
    /// plain writes keep the instant they find, and the declaration is kept
    /// rather than removed.
    pub retired: bool,
}

impl TableExpiry {
    /// The lifetime a created record gets, when the declaration is in force.
    #[must_use]
    pub const fn default_lifetime(&self) -> Option<Duration> {
        if self.retired { None } else { self.after }
    }

    /// As the definition stores it.
    pub(crate) fn to_value(self) -> Value {
        let mut fields = BTreeMap::new();
        if let Some(after) = self.after {
            fields.insert(AFTER.to_owned(), Value::Duration(after));
        }
        if self.retired {
            fields.insert(RETIRED.to_owned(), Value::Bool(true));
        }
        Value::Object(fields)
    }

    /// Read back what [`Self::to_value`] wrote.
    ///
    /// # Errors
    ///
    /// [`Error::CatalogMalformed`] for a value of another shape.
    pub(crate) fn from_value(value: &Value) -> Result<Self> {
        let Value::Object(fields) = value else {
            return Err(malformed(value.type_name()));
        };
        let after = match fields.get(AFTER) {
            None => None,
            Some(Value::Duration(after)) => Some(*after),
            Some(other) => return Err(malformed(other.type_name())),
        };
        let retired = match fields.get(RETIRED) {
            None => false,
            Some(Value::Bool(retired)) => *retired,
            Some(other) => return Err(malformed(other.type_name())),
        };
        Ok(Self { after, retired })
    }
}

fn malformed(found: &'static str) -> Error {
    Error::CatalogMalformed {
        entity: "table",
        field: "expire",
        found,
    }
}
