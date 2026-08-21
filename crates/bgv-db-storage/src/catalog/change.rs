//! Reading a mutation as a catalog change.
//!
//! A definition is an ordinary record (ADR-0009), so a caller that needs to know
//! what a log record says about the catalog would otherwise have to know where
//! the catalog is stored. Decoding lives here instead: the system tables stay
//! private to this module tree, and a caller asks what changed.

use bgv_db_encoding::{Mutation, RecordValue, decode_payload};

use super::definition::IndexDefinition;
use super::field::FieldDefinition;
use super::{SYSTEM_DATABASE, SYSTEM_NAMESPACE, TableDefinition, system};
use crate::error::Result;
use crate::transaction::RecordAddress;

/// What a mutation says about the catalog, if it is addressed at one.
///
/// Decoding lives here rather than at the call site so that the system tables
/// stay private to this module: a caller asks what changed, not where it is
/// stored.
#[derive(Debug)]
pub(crate) enum CatalogChange {
    /// A table was defined, or its definition rewritten.
    TableDefined(Box<TableDefinition>),
    /// A field was declared on a table.
    FieldDefined(Box<FieldDefinition>),
    /// A field declaration was removed. The address is where the outgoing
    /// definition can still be read, in the state this record applies on top of.
    FieldDropped(RecordAddress),
}

/// Read a mutation as a catalog change, or `None` when it is ordinary data.
///
/// # Errors
///
/// Returns [`Error::CatalogMalformed`] when a definition cannot be decoded.
pub(crate) fn catalog_change(mutation: &Mutation) -> Result<Option<CatalogChange>> {
    if mutation.namespace != SYSTEM_NAMESPACE || mutation.database != SYSTEM_DATABASE {
        return Ok(None);
    }
    let present = match &mutation.value {
        RecordValue::Present(payload) => Some(decode_payload(payload)?),
        RecordValue::Tombstone => None,
    };
    Ok(match (mutation.table, present) {
        (table, Some(value)) if table == system::TABLES => Some(CatalogChange::TableDefined(
            Box::new(TableDefinition::from_value(&value)?),
        )),
        (table, Some(value)) if table == system::FIELDS => Some(CatalogChange::FieldDefined(
            Box::new(FieldDefinition::from_value(&value)?),
        )),
        (table, None) if table == system::FIELDS => Some(CatalogChange::FieldDropped(
            system::address(system::FIELDS, mutation.id.clone()),
        )),
        _ => None,
    })
}

/// The index this mutation defines, if it defines one.
///
/// A catalog entry is an ordinary record (ADR-0009), so `DEFINE INDEX` reaches
/// the log as a write into the system index table. Index maintenance asks this
/// so that it can build a new index's entries in the same commit that defines
/// it, rather than leaving an index that knows nothing about the rows already
/// in its table.
///
/// # Errors
///
/// Returns [`crate::Error::CatalogMalformed`] when the record in the index table is not
/// a definition, and a decoding failure otherwise.
pub(crate) fn defined_index(mutation: &Mutation) -> Result<Option<IndexDefinition>> {
    if mutation.namespace != SYSTEM_NAMESPACE
        || mutation.database != SYSTEM_DATABASE
        || mutation.table != system::INDEXES
    {
        return Ok(None);
    }
    // A dropped index has nothing to build. Its existing entries are left where
    // they are — unreachable, because index ids are never reused — and reclaiming
    // them is its own piece of work (Q-33), not this one's.
    let RecordValue::Present(payload) = &mutation.value else {
        return Ok(None);
    };
    IndexDefinition::from_value(&decode_payload(payload)?).map(Some)
}
