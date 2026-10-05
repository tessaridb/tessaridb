//! The one mapping between JSON and this store's values (ADR-0116 D1).
//!
//! JSON has six types and this store has seventeen, so each direction is a
//! decision rather than a translation, and each is written once: the stream
//! reader, the HTTP surface and the language's `json::` functions all go through
//! here, so what `json::encode` answers is what `POST /script` shows and what
//! `json::parse` reads is what a Kafka consumer reads.

mod read;
mod write;

pub use read::{Malformed, read};
pub use write::{Names, referenced_tables, string, string_literal, write};
