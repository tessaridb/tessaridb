//! Records and transactions for `bgv-db`.
//!
//! This crate turns the ordered byte store below it into a record store with a
//! declared isolation level. It owns three things and no more: the store handle
//! and its metadata, the transaction, and the conflict rule.
//!
//! # The isolation level is declared, not emergent
//!
//! Transactions run at **snapshot isolation**. That is a decision with a written
//! rationale, not a consequence of how the code happened to be built: the record
//! layout stores versions inline under each record, newest first, so a snapshot
//! is one sequence number and a read is a seek. The level is named here, in the
//! transaction's documentation, and in the error taxonomy.
//!
//! Snapshot isolation permits write skew and phantoms. Those are documented on
//! [`Transaction`] and demonstrated by the test suite, because an anomaly a user
//! discovers in production was not really part of the contract.
//!
//! Serializable is not foreclosed: it is this level plus read-set tracking, over
//! the same bytes on disk.

#![forbid(unsafe_code)]

mod catalog;
mod error;
mod index;
mod log;
mod reclaim;
mod schema;
mod snapshots;
mod store;
mod transaction;

pub use catalog::{
    Catalog, DatabaseDefinition, FieldDefinition, IndexDefinition, NamespaceDefinition,
    SYSTEM_DATABASE, SYSTEM_NAMESPACE, TableDefinition,
};
pub use error::{Error, Result};
pub use reclaim::Reclaimed;
pub use store::Store;
pub use transaction::{RecordAddress, Transaction};
