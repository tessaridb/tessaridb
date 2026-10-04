//! Backing up a store as its log, and restoring it by replay.
//!
//! # Why the log is the whole backup
//!
//! ADR-0001 made the log the source of truth, and
//! `invariant-state-is-a-function-of-log` states the consequence: the records,
//! the indexes, the catalog, the search statistics and the vector graph are all
//! **derived** from the log by a pure function. Nothing in this store is
//! knowable only from its state.
//!
//! So a copy of the log is a complete backup, and a restore is a replay. That is
//! not a shortcut taken to write less code — it is the design paying out, and it
//! means the restore path is [`tessari_storage::Store::apply_record`], the same
//! function a replica runs and the same one a commit runs once it has chosen its
//! sequence. A restore therefore exercises code that is exercised constantly,
//! rather than a second path written for disasters and run once a year.
//!
//! A physical copy of the keyspaces would restore faster and would verify
//! nothing. It would also bind the backup format to the engine underneath, which
//! is what ADR-0004's "RocksDB first, not RocksDB only" exists to avoid.
//!
//! # What that makes testable
//!
//! If a restored store differs from the original in **any** respect, then
//! something in the store is not derived from the log — which is a defect in the
//! store rather than in the backup. The tests restore a store that has exercised
//! every engine this project has and compare the two keyspace by keyspace.
//! Nothing else here can make that assertion.
//!
//! # The format
//!
//! ```text
//! head     "TESSARILOG" <format:u8> <codec:u8> <writer:u32*3> <sections:u32>
//! frame    <tag:u8>
//!   tag 1  section  <home:9> <writer:16> <from:u64> <tail:u64>
//!   tag 2  record   <length:u32> <sequence:u64> <crc32:u32> <bytes…>
//! ```
//!
//! # Why a file holds sections rather than a log
//!
//! A store used to hold one log and now holds one per range (S6.2), so a file
//! carrying *the* log would read as whole, restore without error, and be missing
//! every record written into a database — the one failure a backup exists to
//! prevent. Each log gets a section, and the file is the store rather than a
//! part of it (Q-624).
//!
//! `from` is the first sequence that section holds, which is what makes an
//! **incremental** backup a thing a reader can check rather than a thing a
//! filename claims: a section starting at `from` restores onto a store whose
//! log stands at `from - 1`, and onto no other. The bounds sit **after** the
//! home because a position counts in one log and means nothing without knowing
//! which (Q-621).
//!
//! Frames carry a tag for the reason records carry a length: a boundary that
//! has to be *inferred* — from a record count, or from a sequence reaching the
//! section's tail — is a decoder wandering into the next section and
//! succeeding. `sections` is in the head instead so that a reader knows how many
//! to expect **before** applying any of them, which is what lets a restore that
//! cannot span several refuse one without having half-applied it.
//!
//! The CRC is over the record's body and exists because framing catches a file
//! that was *cut* and nothing about a file that is the right length and holds
//! the wrong bytes. It detects **corruption**, which is what happens to files;
//! it does not detect **tampering**, which needs a key and a threat model this
//! format does not have.
//!
//! `writer` is the build that produced the file, and it is a third version
//! rather than a repetition of the first two. The framing version says how to
//! find the records; the codec version says how to decode one; neither says what
//! the build that wrote them **meant**. A file from an older build restores and
//! is reported, which is the ordinary case and the reason to record it at all; a
//! file from a newer one is refused, because a newer writer may have given a
//! record a meaning this build does not know and the framing cannot see that.
//!
//! The header exists so a restore refuses a file it cannot read **before**
//! applying any of it: a half-applied restore is worse than a refused one,
//! because it looks like a store. `tail` is the sequence the backup was taken
//! at, so a reader knows what it is holding before it reads it.
//!
//! Records are length-framed so a truncated file is caught at the record that
//! was cut, rather than by a decoder wandering into the next one and succeeding.
//!
//! # A truncated backup restores what it has
//!
//! A backup interrupted at record nine thousand is nine thousand records of
//! data, and refusing it entirely would throw away the thing somebody is holding
//! in a bad week. It applies what is whole, stops at the cut, and reports the
//! count — the caller decides what that is worth.

#![forbid(unsafe_code)]
// `expect_used` and `as_conversions` govern production code; a test states its own expectations.
#![cfg_attr(test, allow(clippy::expect_used, clippy::as_conversions))]

use std::io::Write;

pub use error::{Error, Result};
pub use reading::{bootstrap, read, read_until, verify};
pub use report::{Bootstrapped, LogSpan, Restored, Verified, VerifiedLog, Written};
pub use state::{
    STATE_MAGIC, StateTaken, is_state, read_state, verify_state, write_state, write_state_within,
};
use tessari_encoding::{LogId, NodeVersion, StoreValue, Writer};
use tessari_storage::Store;
use tessari_types::{DatabaseId, NamespaceId, Reach, Sequence, ShardId, TableId};

pub use writing::{only_log, write, write_from};

mod check;
mod error;
mod format;
mod layout;
mod reading;
mod report;
mod state;
mod writing;

use layout::{
    FORMAT, FRAME_RECORD, FRAME_SECTION, FRAME_SHARD_SECTION, LEGACY_MAGIC, MAGIC, PAGE, log_bytes,
    log_in, shard_log_bytes, shard_log_in,
};
use reading::Filled;
