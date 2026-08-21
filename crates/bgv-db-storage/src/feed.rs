//! Reading what changed, out of the log that already records it.
//!
//! # Derived, not built
//!
//! Every commit writes a log record carrying exactly what it changed, in address
//! order, and that record has been carrying replication since SG2.T7. A change
//! feed is a **projection of it** rather than a second mechanism — so it needs
//! no state of its own, cannot disagree with what was committed, and a replica's
//! feed over the same log is identical to the leader's. That is the property
//! index maintenance and schema validation already have, and it is why both are
//! computed from the log rather than transmitted alongside it.
//!
//! # A change says what a record became, not what it was
//!
//! Two kinds, `Written` and `Removed` — not created, updated and deleted. The
//! log carries the new state, so calling a write a *creation* means knowing what
//! stood there at that sequence, which is MVCC history rather than the current
//! state. A feed that consulted the current state instead would label a record
//! created and then changed as an update, and **a wrong label is worse than a
//! missing one**: a subscriber can tell new from changed by keeping its own set,
//! and cannot recover from being told the wrong thing.
//!
//! # The catalog is not in the feed
//!
//! A catalog entry is an ordinary record in the system tenancy (ADR-0009), which
//! is exactly what makes replication and index maintenance work and exactly what
//! a subscriber watching `users` does not want. Schema evolution is a different
//! feed with a different shape; conflating them would put rows nobody asked for
//! into every subscription.

use bgv_db_encoding::{LogRecord, RecordValue, decode_payload};
use bgv_db_types::{DatabaseId, NamespaceId, RecordId, Sequence, TableId, Value};

use crate::catalog::{SYSTEM_DATABASE, SYSTEM_NAMESPACE};
use crate::error::Result;

/// What a read of the feed found, and where to resume.
///
/// The position is not a convenience. A log record can produce **no changes** —
/// a commit that only touched the catalog does exactly that — so a subscriber
/// given only a list cannot tell "nothing has happened" from "nothing I care
/// about has happened", and would ask for the same records forever. Returning
/// where the read reached is what lets it advance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Changes {
    /// What changed, oldest first, in the log's own order.
    pub changes: Vec<Change>,
    /// The sequence to ask from next.
    ///
    /// One past the last record read, or the sequence asked for when there was
    /// nothing to read — so passing it back always means "whatever is new".
    pub next: Sequence,
}

/// What happened to one record, at one sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// The commit this change was part of.
    ///
    /// Shared by every change of the same commit, which is what lets a
    /// subscriber apply them as the unit they were written as.
    pub sequence: Sequence,
    /// The namespace the record belongs to.
    pub namespace: NamespaceId,
    /// The database within it.
    pub database: DatabaseId,
    /// The table within that.
    pub table: TableId,
    /// The record's identity.
    pub id: RecordId,
    /// What became of it.
    pub kind: ChangeKind,
}

/// What became of a record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeKind {
    /// The record now holds this value.
    ///
    /// Whether that is its first value or its fifth is not said, because the log
    /// does not carry it — see the module documentation.
    Written(Value),
    /// The record is no longer there.
    Removed,
}

/// The changes one log record carries, in the order the log carries them.
///
/// # Errors
///
/// Returns a decoding failure when a payload cannot be read. A payload that
/// cannot be decoded is corruption, not a change to skip: skipping it would give
/// a subscriber a feed that silently disagrees with the store.
pub(crate) fn changes_in(sequence: Sequence, record: &LogRecord) -> Result<Vec<Change>> {
    let mut changes = Vec::new();
    for mutation in record.mutations() {
        if mutation.namespace == SYSTEM_NAMESPACE && mutation.database == SYSTEM_DATABASE {
            continue;
        }
        let kind = match &mutation.value {
            RecordValue::Present(payload) => ChangeKind::Written(decode_payload(payload)?),
            RecordValue::Tombstone => ChangeKind::Removed,
        };
        changes.push(Change {
            sequence,
            namespace: mutation.namespace,
            database: mutation.database,
            table: mutation.table,
            id: mutation.id.clone(),
            kind,
        });
    }
    Ok(changes)
}
