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
// `expect_used` and `as_conversions` govern production code; a test states its own expectations.
#![cfg_attr(test, allow(clippy::expect_used, clippy::as_conversions))]

#[cfg(test)]
mod across_model;
mod adjacency;
mod attempts;
mod audit;
mod bounded;
mod cardinality;
mod catalog;
#[cfg(test)]
mod catalog_rows_tests;
mod collections;
mod covering;
mod decisions;
mod error;
mod expiry;
mod failover;
mod feed;
mod followers;
mod gate;
mod graph;
mod holds;
mod index;
#[cfg(test)]
mod index_unchanged_tests;
mod intents;
mod lapse;
mod lease;
mod lines;
mod log;
mod log_holds;
mod node;
mod ordering;
mod pruning;
mod quarantine;
mod reclaim;
mod retention;
mod rollup_states;
mod running;
mod sampled_shards;
mod schema;
mod sealing;
mod series;
mod served;
mod shards;
mod snapshots;
mod state;
mod statistics;
mod store;
mod tailmarks;
mod tally;
mod topic;
mod transaction;
mod vault;
mod views;

pub use catalog::{
    AnalyzerDefinition, Authority, AutoSplit, CLAIMED_BY_CONSUMER, CLAIMED_BY_INSTANCE, Catalog,
    ConsumerDefinition, DatabaseDefinition, EDGE_IN, EDGE_OUT, EdgeDeclaration, EdgeKindDefinition,
    EdgeOrder, EngineField, EngineMember, EventDeclaration, Eviction, FailoverDefinition,
    FailoverStamp, Feed, FieldDefinition, FieldShape, GEO_FIELD, GrantDefinition, GraphDefinition,
    Greeter, GroupDeclaration, GroupState, Held, InFlight, IndexDefinition, IndexShape, JoinTicket,
    Kind, LeadershipDefinition, Mapped, NamespaceDefinition, OnFailure, PublicAppend,
    QUEUE_ATTEMPTS, QUEUE_CLAIMED_BY, QUEUE_CLAIMED_UNTIL, QueueDeclaration, RECORD_LEVEL, Reach,
    ReplicaDefinition, Role, RollupCompute, RollupDeclaration, RollupFold, SYSTEM_DATABASE,
    SYSTEM_NAMESPACE, SearchCosts, SeriesDeclaration, ShardMap, ShardSpan, SpaceDeclaration,
    SpaceLimit, StoredKind, TableDefinition, TableKind, TableShape, TopicDeclaration, UNIT_WEIGHT,
    UserDefinition, VECTOR_FIELD, VaultCustody, VaultDeclaration, VectorDeclaration,
    VectorDistance, Verb, ViewDeclaration, WordSet, WordSetKind, another_node_may_write, governing,
    names_a_peer, the_row_a_greeting_binds,
};
// Exported because a refinement figure is only readable beside the relation it
// was measured under, and that relation is a decision this crate takes.
pub use catalog::VaultRoot;
/// The system tables, by id and name — part of the written format (`docs/key-grammar.md` §9).
pub use catalog::system::ALL as SYSTEM_TABLES;
pub use collections::{Collection, Collections, Currency, Upstream, UpstreamReport};
pub use covering::MEASURED_RELATION;
pub use decisions::Decisions;
pub use expiry::Expired;
pub use lapse::Lapsed;
pub use log_holds::LogHold;
pub use retention::{Retention, RetentionSource};
pub use topic::{Message, Messages};
// Re-exported because `ReplicaDefinition` carries one: a caller that can read
// the field but cannot name its type has a public API it cannot use.
pub use attempts::Attempts;
pub use audit::{
    Administered, AuditDevice, AuditTrail, DeviceRefused, VaultRead, administered,
    entries as audit_entries, reads_by,
};
pub use error::{ConflictWith, Error, Result};
pub use failover::Failover;
pub use feed::{Change, ChangeKind, Changes, History, Merged, Subject, Subscription, Watch};
pub use followers::FollowerLag;
pub use graph::{Graph as VectorGraph, Matched, filtered_ceiling, vector_of};
pub use lease::{GUARD as LEASE_GUARD, Lease, TTL as LEASE_TTL};
pub use ordering::{Horizon, MergedHistory, Page, in_writer_order};
pub use pruning::{Pruned, Trimmed};
pub use reclaim::Reclaimed;
pub use running::{Progress, Running};
pub use sampled_shards::{SampledShard, SampledTable};
pub use schema::{Violation, violations};
pub use sealing::{
    KEYS_FIELD, VAULT_RECIPIENT, add_recipient, initialise_root, mint_own_vault_key,
    mint_vault_key, open_data_key, open_field, recipients, remove_recipient, reseal_named,
    rewrap_own_vault, seal_secrets, unseal_own_vault, vault_key_scope,
};
pub use state::{StateReader, TopicHead};
pub use statistics::{estimate_equality, estimate_range};
pub use store::{Health, Store};
pub use tally::AcrossOutcome;
pub use tessari_encoding::{
    BUILD_VERSION, Decision, IndexStatistics, LogId, NODE_ID_LEN, Roles, TransactionId,
    TransactionRecord, Writer,
};
pub use transaction::{
    AcrossPart, Committed, Expansion, FieldedPostings, Neighbour, PlacesNearest, RecordAddress,
    Region, SearchCounts, StoredRecord, Transaction, Window,
};
pub use vault::{OpenVault, SealState};
pub use views::ViewState;
