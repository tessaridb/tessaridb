//! When a table's shards are split and merged without being asked
//! (ADR-0113 D2), as the table definition stores it.

use std::collections::BTreeMap;

use tessari_types::{Number, Value};

use super::count_of;
use crate::error::{Error, Result};

const ABOVE: &str = "above";
const WRITES_PER_SECOND: &str = "writes_per_second";
const MERGE_BELOW: &str = "merge_below";

/// The bounds the store line's leader keeps a table's shards within.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoSplit {
    /// A shard holding more records than this is split.
    pub above: u64,
    /// A shard taking more writes a second than this is split, however small.
    pub writes_per_second: Option<u64>,
    /// Two neighbouring shards holding fewer records than this together are
    /// merged.
    pub merge_below: u64,
}

impl AutoSplit {
    /// As the definition stores it.
    pub(crate) fn to_value(self) -> Value {
        let number =
            |held: u64| Value::Number(Number::Integer(i64::try_from(held).unwrap_or(i64::MAX)));
        let mut fields = BTreeMap::new();
        fields.insert(ABOVE.to_owned(), number(self.above));
        if let Some(writes) = self.writes_per_second {
            fields.insert(WRITES_PER_SECOND.to_owned(), number(writes));
        }
        fields.insert(MERGE_BELOW.to_owned(), number(self.merge_below));
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
        let bound = |name: &'static str| {
            fields
                .get(name)
                .map(|held| count_of(held, "table", "auto_split"))
                .transpose()
        };
        Ok(Self {
            above: bound(ABOVE)?.ok_or_else(|| malformed("no upper bound"))?,
            writes_per_second: bound(WRITES_PER_SECOND)?,
            merge_below: bound(MERGE_BELOW)?.ok_or_else(|| malformed("no lower bound"))?,
        })
    }
}

fn malformed(found: &'static str) -> Error {
    Error::CatalogMalformed {
        entity: "table",
        field: "auto_split",
        found,
    }
}
