//! The persistent backend for `bgv-db`.
//!
//! This crate implements the same [`KvBackend`](bgv_db_kv::KvBackend) contract
//! the in-memory backend implements, on a log-structured merge-tree engine. The
//! layers above — the key grammar, the record store, transactions — run on
//! either one without knowing which.
//!
//! # What this crate is for
//!
//! Two implementations of one contract is not redundancy. The in-memory backend
//! keeps the test suite fast enough to run on every change; this one is what the
//! data actually lives in. A contract with a single implementation is a
//! description of that implementation, and the conformance suite that both run
//! is what makes it a contract instead.
//!
//! # Where the engine stops
//!
//! Engine types do not leave this crate. Nothing above it sees a region handle,
//! an engine status, an iterator or a raw byte slice with engine lifetime — it
//! sees keys, values, batches and the substrate's error categories. That
//! boundary is the reason a second engine, or a different one, is a new crate
//! rather than a change spread across the codebase.
//!
//! # The durability promise
//!
//! Stated per level in [`Durability`] and proven by killing the writing process,
//! not by assertion. The default is the level a system of record needs.

mod backend;
mod error;
mod options;

pub use backend::LsmBackend;
pub use options::{Durability, StoreConfig, effective_options_files};
