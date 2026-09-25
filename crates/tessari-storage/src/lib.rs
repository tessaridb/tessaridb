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
mod audit;
mod cardinality;
mod catalog;
mod collections;
mod covering;
mod error;
mod expiry;
mod failover;
mod feed;
mod followers;
mod graph;
mod index;
mod lease;
mod lines;
mod log;
mod node;
mod ordering;
mod pruning;
mod reclaim;
mod running;
mod schema;
mod sealing;
mod series;
mod served;
mod shards;
mod snapshots;
mod store;
mod tailmarks;
mod transaction;
mod vault;

pub use catalog::{
    AnalyzerDefinition, Authority, CLAIMED_BY_CONSUMER, CLAIMED_BY_INSTANCE, Catalog,
    ConsumerDefinition, DatabaseDefinition, EDGE_IN, EDGE_OUT, EdgeDeclaration, EdgeKindDefinition,
    EdgeOrder, FailoverDefinition, FailoverStamp, FieldDefinition, FieldShape, GEO_FIELD,
    GrantDefinition, GraphDefinition, Held, IndexDefinition, IndexShape, Kind,
    LeadershipDefinition, Mapped, NamespaceDefinition, OnFailure, QUEUE_ATTEMPTS, QUEUE_CLAIMED_BY,
    QUEUE_CLAIMED_UNTIL, QueueDeclaration, RECORD_LEVEL, Reach, ReplicaDefinition, Role,
    SYSTEM_DATABASE, SYSTEM_NAMESPACE, SeriesDeclaration, ShardMap, ShardSpan, StoredKind,
    TableDefinition, TableKind, TableShape, UserDefinition, VECTOR_FIELD, VaultDeclaration,
    VectorDeclaration, VectorDistance, Verb, ViewDeclaration, another_node_may_write, governing,
    names_a_peer, the_row_a_greeting_binds,
};
// Exported because a refinement figure is only readable beside the relation it
// was measured under, and that relation is a decision this crate takes.
pub use catalog::VaultRoot;
pub use collections::{Collection, Collections, Currency};
pub use covering::MEASURED_RELATION;
pub use expiry::Expired;
// Re-exported because `ReplicaDefinition` carries one: a caller that can read
// the field but cannot name its type has a public API it cannot use.
pub use audit::{AuditDevice, AuditTrail, VaultRead, entries as audit_entries, reads_by};
pub use error::{Error, Result};
pub use failover::Failover;
pub use feed::{Change, ChangeKind, Changes, History, Subject, Subscription, Watch};
pub use followers::FollowerLag;
pub use graph::vector_of;
pub use lease::{GUARD as LEASE_GUARD, Lease, TTL as LEASE_TTL};
pub use ordering::{Horizon, MergedHistory, Page, in_writer_order};
pub use pruning::{Pruned, Trimmed};
pub use reclaim::Reclaimed;
pub use running::{Progress, Running};
pub use schema::{Violation, violations};
pub use sealing::{
    KEYS_FIELD, VAULT_RECIPIENT, add_recipient, initialise_root, mint_vault_key, open_data_key,
    open_field, recipients, remove_recipient, reseal_named, seal_secrets, vault_key_scope,
};
pub use store::{Health, Store};
pub use tessari_encoding::{BUILD_VERSION, LogId, NODE_ID_LEN, Roles, Writer};
pub use transaction::{
    Expansion, Nearby, Neighbour, RecordAddress, Region, StoredRecord, Transaction, Window,
};
pub use vault::OpenVault;
