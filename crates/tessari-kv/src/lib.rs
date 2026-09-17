//! Ordered key-value substrate for TessariDB.
//!
//! This crate is the lowest layer of the engine. It stores opaque byte keys in
//! lexicographic order and byte values, and it answers four questions: read one
//! key, scan a range, apply a batch of changes atomically, and refuse that batch
//! if a stated precondition no longer holds.
//!
//! # What this layer does not do
//!
//! It does not sequence, order or replay anything. Sequence numbers, the
//! replication log, MVCC versioning and transaction isolation all live *above*
//! this trait, because the engine owns its own ordering (ADR-0001) and a backend
//! inventing its own would compete with it.
//!
//! A backend is therefore a dumb, correct byte store. The full contract — what a
//! backend must guarantee and what it deliberately does not — is documented on
//! [`KvBackend`], and callers are expected to read it rather than assume the
//! guarantees a relational store would give.

#![forbid(unsafe_code)]

pub mod conformance;

mod backend;
mod batch;
mod error;
mod key;
mod keyspace;
mod memory;

pub use backend::{KvBackend, ScanDirection, ScanRequest, delete_range_by_scanning};
pub use batch::{Precondition, WriteBatch, WriteOp};
pub use error::{Error, ErrorCategory, Result};
pub use key::{Key, KeyRange, Value};
pub use keyspace::Keyspace;
pub use memory::MemoryBackend;
