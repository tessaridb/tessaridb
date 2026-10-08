//! `DEDUPLICATE <window>` on a queue and a topic (ADR-0124 D8).
//!
//! A queue remembers each identity it was given and a topic each key its
//! messages carried, for the window counted from the FIRST write, as a record in
//! the table's unnameable companion (`Catalog::seen_named`). The record carries
//! the window's end as its expiry instant, so it replicates with the write that
//! made it and is reclaimed by the pass that reclaims every expired version —
//! there is no sweep of its own and nothing to keep in step with a clock.
//!
//! A `CREATE` that finds a marker writes nothing, including no marker: the
//! window is not stretched by the repeats inside it.

use tessari_encoding::encode_payload;
use tessari_ql::Span;
use tessari_storage::{Catalog, RecordAddress, TableDefinition, TableKind, Transaction};
use tessari_types::{Number, RecordId, Value};

use crate::error::{Error, Result};
use crate::session::Session;

/// Where a write's key is remembered, and until when.
pub(super) struct Marker {
    address: RecordAddress,
    until: u64,
}

impl Marker {
    /// Whether the key was written less than the window ago — this
    /// transaction's own earlier write included.
    pub(super) fn seen(&self, transaction: &mut Transaction<'_>) -> Result<bool> {
        Ok(transaction.get(&self.address)?.is_some())
    }

    /// Remember the key until the window closes.
    pub(super) fn leave(self, transaction: &mut Transaction<'_>) {
        let empty = Value::Object(std::collections::BTreeMap::new());
        transaction.put(self.address.clone(), encode_payload(&empty).into_bytes());
        transaction.expire_pending(&self.address, self.until);
    }
}

impl Session<'_> {
    /// The marker a write into `table` is judged by, or `None` when the table
    /// does not deduplicate or the write carries no key.
    ///
    /// A queue's key is the record's identity, so a write that has none yet —
    /// one the store names — has nothing to repeat. A topic's key is the field
    /// it declares; a message without it is appended.
    ///
    /// # Errors
    ///
    /// [`Error::DeduplicationKey`] when a topic's key holds a value no identity
    /// can be made of, and a store failure otherwise.
    pub(super) fn dedup_marker(
        &self,
        transaction: &mut Transaction<'_>,
        definition: Option<&TableDefinition>,
        identity: Option<&RecordId>,
        payload: &Value,
        span: Span,
    ) -> Result<Option<Marker>> {
        let Some(definition) = definition else {
            return Ok(None);
        };
        let (window, key) = match &definition.kind {
            TableKind::Queue(declared) => match (declared.deduplicate, identity) {
                (Some(window), Some(identity)) => (window, identity.clone()),
                _ => return Ok(None),
            },
            TableKind::Topic(declared) => {
                let Some((window, field)) = &declared.deduplicate else {
                    return Ok(None);
                };
                let held = match payload {
                    Value::Object(fields) => fields.get(field),
                    _ => None,
                };
                let key = match held {
                    None | Some(Value::None | Value::Null) => return Ok(None),
                    Some(Value::Number(Number::Integer(key))) => RecordId::Int(*key),
                    Some(Value::String(key)) => RecordId::Text(key.clone()),
                    Some(Value::Uuid(key)) => RecordId::Uuid(*key),
                    Some(Value::Bytes(key)) => RecordId::Bytes(key.clone()),
                    Some(other) => {
                        return Err(Error::DeduplicationKey {
                            field: field.clone(),
                            found: other.type_name(),
                            span,
                        });
                    }
                };
                (*window, key)
            }
            _ => return Ok(None),
        };
        // Made in the transaction that declared the window, so its absence is a
        // catalog that disagrees with itself rather than a table to skip.
        let seen = Catalog::new(transaction)
            .table_id(
                definition.namespace,
                definition.database,
                &Catalog::seen_named(&definition.name),
            )?
            .ok_or(tessari_storage::Error::CatalogMalformed {
                entity: "table",
                field: "deduplicate",
                found: "a window with no marker table",
            })?;
        let length = i128::from(window.seconds())
            .saturating_mul(1_000)
            .saturating_add(i128::from(window.nanos() / 1_000_000));
        let until = transaction
            .clock()
            .saturating_add(u64::try_from(length).unwrap_or(u64::MAX));
        Ok(Some(Marker {
            address: RecordAddress::new(definition.namespace, definition.database, seen, key),
            until,
        }))
    }
}
