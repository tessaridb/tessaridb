//! Key grammar and value codec for `bgv-db`.
//!
//! This crate sits directly above the key-value substrate and directly below
//! everything else. It owns the byte-level schema of the store: which bytes name
//! a record, in what order they sort, and how a stored value declares its own
//! format.
//!
//! Its reason to exist as a layer is that nothing above it should build a key by
//! hand. A key assembled at a call site is a schema decision taken somewhere
//! nobody will look for it, and the resulting mistakes — a component encoded in
//! the wrong order, a missing terminator, an index entry that disagrees with the
//! record it points at — are silent. They return wrong results rather than
//! errors.
//!
//! # What is settled here, permanently
//!
//! - The key-kind tag table ([`KeyKind`]). Tags are never reused or renumbered.
//! - The component encodings ([`KeyWriter`], [`KeyReader`]). Byte order equals
//!   logical order, which is what makes every range scan and every index correct.
//! - The record key layout ([`RecordKey`]), including the MVCC version suffix.
//! - The value header ([`CODEC_VERSION`]), which every stored value carries.
//!
//! Changing any of those on a store that already holds data is a rebuild from an
//! export, not a migration. `docs/key-grammar.md` is the normative statement and
//! records which decisions are fixed and which can still move.

#![forbid(unsafe_code)]

mod error;
mod index_keys;
mod index_value;
mod keys;
mod kind;
mod order;
mod payload;
mod record_id;
mod value;

pub use error::{Error, Result};
pub use index_keys::{
    INDEX_PREFIX_LEN, IndexAddress, IndexTarget, IndexValues, NoPayload, SecondaryIndexKey,
    UniqueIndexKey,
};
pub use keys::{
    AppliedPositionKey, FormatVersionKey, LogKey, RecordKey, StoreKey, TABLE_PREFIX_LEN,
};
pub use kind::KeyKind;
pub use order::{KeyReader, KeyWriter};
pub use payload::{decode as decode_payload, encode as encode_payload};
pub use value::{CODEC_VERSION, FormatVersion, LogRecord, Mutation, RecordValue, StoreValue};
