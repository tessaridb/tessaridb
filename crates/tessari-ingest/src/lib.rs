//! Running the stream consumers the catalog declares.
//!
//! A consumer is **declared** in TessariQL and stored in the catalog
//! (ADR-0023). This crate is the half that carries the declaration out: it reads
//! messages, shapes them into records, writes them, and only then tells the
//! broker where it got to.
//!
//! # The one ordering that is the whole guarantee
//!
//! Two commits happen into two different systems — this store's transaction, and
//! the broker's offset — and no transaction spans both. The store commit comes
//! **first**. That chooses duplicates over loss, because a duplicate is what a
//! record identity can absorb and a loss is not.
//!
//! So what this crate provides is **at-least-once delivery with idempotent
//! application by record identity**. It is not exactly-once, and nothing here or
//! in the documentation says otherwise.
//!
//! # Why the broker is behind a trait
//!
//! Every property this has to prove — that a poison message does not stall a
//! partition, that a kill between the two commits yields a duplicate and never a
//! loss, that parallelism actually runs work at the same time, that a restart
//! resumes — needs a source that can be made to fail on command. A real broker
//! inside `cargo test` cannot do that, and a suite that needs one is a suite that
//! does not run.
//!
//! So the runner consumes [`Source`]. The Kafka client is one implementation of
//! it (behind the `kafka` feature, ADR-0024), and the tests use another.

mod apply;
mod json;
#[cfg(feature = "kafka")]
mod kafka;
mod runner;
mod source;

pub use apply::{Shaped, shape};
pub use json::{Malformed, read};
pub use runner::{Broker, Runner, Started};
pub use source::{Message, Source, SourceError};

#[cfg(feature = "kafka")]
pub use kafka::Kafka;
