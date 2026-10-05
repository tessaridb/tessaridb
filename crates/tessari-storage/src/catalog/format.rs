//! The format an operator finalized the store to (ADR-0118 D3).
//!
//! One record, beside the failover policy in the cluster's policy table rather
//! than in a table of its own: a new system table is itself a format change, so a
//! store could not record its own finalize in one. An older build reads only the
//! failover policy's key there and never sees this row.
//!
//! The record is what travels; each replica, on applying it, raises the
//! node-local stamp (`FormatVersionKey`) in the same batch.

use std::collections::BTreeMap;

use tessari_encoding::{FormatVersion, Mutation, RecordValue, decode_payload, encode_payload};
use tessari_types::{Number, RecordId, Value};

use super::definition::{count_of, object};
use super::{Catalog, SYSTEM_DATABASE, SYSTEM_NAMESPACE, system};
use crate::error::{Error, Result};

const ROW: &str = "format";
const ENTITY: &str = "format";
const FIELD_VERSION: &str = "version";

fn key() -> RecordId {
    RecordId::from(ROW)
}

fn to_value(version: FormatVersion) -> Value {
    Value::Object(BTreeMap::from([(
        FIELD_VERSION.to_owned(),
        Value::Number(Number::Integer(i64::from(version.get()))),
    )]))
}

fn from_value(value: &Value) -> Result<FormatVersion> {
    let fields = object(value, ENTITY)?;
    let held = fields.get(FIELD_VERSION).ok_or(Error::CatalogMalformed {
        entity: ENTITY,
        field: FIELD_VERSION,
        found: "nothing",
    })?;
    let version = u32::try_from(count_of(held, ENTITY, FIELD_VERSION)?).map_err(|_| {
        Error::CatalogMalformed {
            entity: ENTITY,
            field: FIELD_VERSION,
            found: "a number past any format",
        }
    })?;
    Ok(FormatVersion::new(version))
}

impl Catalog<'_, '_> {
    /// Record that the store is finalized to `version`, for every replica to
    /// apply.
    pub fn finalize_format(&mut self, version: FormatVersion) {
        self.transaction.put(
            system::address(system::FAILOVER, key()),
            encode_payload(&to_value(version)).into_bytes(),
        );
    }

    /// The format the store was last finalized to, or `None` when nobody has
    /// finalized it — which is not the same answer as the format it holds.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored record cannot be read.
    pub fn finalized_format(&self) -> Result<Option<FormatVersion>> {
        let Some(payload) = self
            .transaction
            .get(&system::address(system::FAILOVER, key()))?
        else {
            return Ok(None);
        };
        from_value(&decode_payload(&payload)?).map(Some)
    }
}

/// The format a mutation finalizes the store to, when it is that record.
pub(crate) fn finalized_in(mutation: &Mutation) -> Result<Option<FormatVersion>> {
    if mutation.namespace != SYSTEM_NAMESPACE
        || mutation.database != SYSTEM_DATABASE
        || mutation.table != system::FAILOVER
        || mutation.id != key()
    {
        return Ok(None);
    }
    let RecordValue::Present(payload) = mutation.value.value() else {
        return Ok(None);
    };
    from_value(&decode_payload(payload)?).map(Some)
}
