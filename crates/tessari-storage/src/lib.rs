//! Records and transactions for TessariDB.
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

mod adjacency;
mod catalog;
mod error;
mod feed;
mod graph;
mod index;
mod log;
mod node;
mod reclaim;
mod running;
mod schema;
mod snapshots;
mod store;
mod transaction;

pub use catalog::{
    AnalyzerDefinition, Authority, Catalog, ConsumerDefinition, DatabaseDefinition, EDGE_IN,
    EDGE_OUT, EdgeDeclaration, EdgeKindDefinition, EdgeOrder, FieldDefinition, FieldShape,
    GrantDefinition, GraphDefinition, Held, IndexDefinition, IndexShape, Kind, Mapped,
    NamespaceDefinition, OnFailure, RECORD_LEVEL, Reach, ReplicaDefinition, Role, SYSTEM_DATABASE,
    SYSTEM_NAMESPACE, TableDefinition, TableKind, TableShape, UserDefinition, VECTOR_FIELD,
    VectorDeclaration, VectorDistance, Verb,
};
// Re-exported because `ReplicaDefinition` carries one: a caller that can read
// the field but cannot name its type has a public API it cannot use.
pub use error::{Error, Result};
pub use feed::{Change, ChangeKind, Changes, Subscription, Watch};
pub use graph::vector_of;
pub use reclaim::Reclaimed;
pub use running::{Progress, Running};
pub use store::{Health, Store};
pub use tessari_encoding::{BUILD_VERSION, Roles};
pub use transaction::{Nearby, Neighbour, RecordAddress, Region, StoredRecord, Transaction};
