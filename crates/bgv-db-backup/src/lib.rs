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
//! means the restore path is [`bgv_db_storage::Store::apply_record`], the same
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
//! header   "BGVDBLOG" <format:u8> <codec:u8> <tail:u64>
//! record   <length:u32> <sequence:u64> <bytes…>
//! ```
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

use std::io::{Read, Write};

use bgv_db_encoding::{LogRecord, StoreValue};
use bgv_db_storage::Store;
use bgv_db_types::Sequence;

/// What every file of this kind begins with.
const MAGIC: &[u8; 8] = b"BGVDBLOG";

/// The format's own version, separate from the record codec's.
///
/// Two versions because they change for different reasons: the framing here can
/// gain a field without the records changing, and the records can change without
/// the framing moving.
const FORMAT: u8 = 1;

/// How many log records are read from the store at a time.
///
/// A backup that needed the whole log in memory would fail when it is most
/// needed. Five hundred is a page, not a limit.
const PAGE: usize = 500;

/// What went wrong.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The stream could not be read or written.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// The store refused the work.
    #[error(transparent)]
    Store(#[from] bgv_db_storage::Error),

    /// A record could not be decoded.
    #[error(transparent)]
    Encoding(#[from] bgv_db_encoding::Error),

    /// The file does not begin the way one of these does.
    #[error("this is not a bgv-db backup")]
    NotABackup,

    /// A format or codec version this build does not read.
    ///
    /// Refused rather than attempted, because a decoder that guesses at a
    /// version it does not know produces records nobody wrote.
    #[error("this backup is {what} version {found}; this build reads {supported}")]
    Unsupported {
        /// Which version — the framing's or the records'.
        what: &'static str,
        /// The version the file carries.
        found: u8,
        /// The version this build reads.
        supported: u8,
    },

    /// A restore into a store that already holds something.
    ///
    /// Merging a backup into a populated store is not a restore: the sequences
    /// would collide with a different meaning, and the result would be a store
    /// neither log explains. Refused rather than reconciled.
    #[error("a restore needs an empty store; this one is at sequence {tail}")]
    NotEmpty {
        /// Where the store already is.
        tail: u64,
    },
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// What a backup wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Written {
    /// How many log records it holds.
    pub records: u64,
    /// The sequence the store was at when it was taken.
    pub tail: Sequence,
}

/// What a restore applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Restored {
    /// How many records were applied.
    pub records: u64,
    /// The sequence the file said it held.
    pub tail: Sequence,
    /// Whether the file ended mid-record.
    ///
    /// Reported rather than raised: an interrupted backup is still most of a
    /// store, and the caller is the one who knows whether most is enough.
    pub truncated: bool,
}

/// Write a store's log to `out`.
///
/// # Errors
///
/// Returns an error when the store or the stream fails.
pub fn write(store: &Store, out: &mut impl Write) -> Result<Written> {
    let tail = store.committed_tail()?;
    out.write_all(MAGIC)?;
    out.write_all(&[FORMAT, bgv_db_encoding::CODEC_VERSION])?;
    out.write_all(&tail.get().to_be_bytes())?;

    let mut written = 0_u64;
    let mut from = Sequence::new(1);
    loop {
        let page = store.log_records(from, PAGE)?;
        if page.is_empty() {
            break;
        }
        for (sequence, record) in &page {
            if sequence.get() > tail.get() {
                // A write that landed after the backup began is simply not in
                // it. The tail in the header is what makes that honest rather
                // than arbitrary.
                return Ok(Written {
                    records: written,
                    tail,
                });
            }
            let bytes = record.encode();
            let body = bytes.as_slice();
            let length = u32::try_from(body.len()).unwrap_or(u32::MAX);
            out.write_all(&length.to_be_bytes())?;
            out.write_all(&sequence.get().to_be_bytes())?;
            out.write_all(body)?;
            written = written.saturating_add(1);
        }
        let Some((last, _)) = page.last() else {
            break;
        };
        from = Sequence::new(last.get().saturating_add(1));
    }
    Ok(Written {
        records: written,
        tail,
    })
}

/// Replay a backup into an **empty** store.
///
/// # Errors
///
/// Returns [`Error::NotABackup`] or [`Error::Unsupported`] before applying
/// anything, [`Error::NotEmpty`] when the store is not empty, and the store's
/// own error when a record cannot be applied.
pub fn read(store: &Store, input: &mut impl Read) -> Result<Restored> {
    // The store is checked first, so a wrong target is refused before the file
    // is even read.
    let at = store.committed_tail()?;
    if at.get() > 0 {
        return Err(Error::NotEmpty { tail: at.get() });
    }

    let mut magic = [0_u8; 8];
    input
        .read_exact(&mut magic)
        .map_err(|_| Error::NotABackup)?;
    if &magic != MAGIC {
        return Err(Error::NotABackup);
    }
    let mut versions = [0_u8; 2];
    input
        .read_exact(&mut versions)
        .map_err(|_| Error::NotABackup)?;
    if versions[0] != FORMAT {
        return Err(Error::Unsupported {
            what: "format",
            found: versions[0],
            supported: FORMAT,
        });
    }
    if versions[1] != bgv_db_encoding::CODEC_VERSION {
        return Err(Error::Unsupported {
            what: "record codec",
            found: versions[1],
            supported: bgv_db_encoding::CODEC_VERSION,
        });
    }
    let mut tail = [0_u8; 8];
    input.read_exact(&mut tail).map_err(|_| Error::NotABackup)?;
    let tail = Sequence::new(u64::from_be_bytes(tail));

    let mut applied = 0_u64;
    loop {
        let mut header = [0_u8; 12];
        match fill(input, &mut header)? {
            Filled::Empty => break,
            Filled::Short => {
                return Ok(Restored {
                    records: applied,
                    tail,
                    truncated: true,
                });
            }
            Filled::Whole => {}
        }
        let length = usize::try_from(u32::from_be_bytes([
            header[0], header[1], header[2], header[3],
        ]))
        .unwrap_or(0);
        let sequence = Sequence::new(u64::from_be_bytes([
            header[4], header[5], header[6], header[7], header[8], header[9], header[10],
            header[11],
        ]));
        let mut body = vec![0_u8; length];
        if !matches!(fill(input, &mut body)?, Filled::Whole) {
            return Ok(Restored {
                records: applied,
                tail,
                truncated: true,
            });
        }
        let record = LogRecord::decode(&body)?;
        store.apply_record(sequence, &record)?;
        applied = applied.saturating_add(1);
    }
    Ok(Restored {
        records: applied,
        tail,
        // A file cut cleanly *between* records ends the way a whole one does, so
        // the framing alone cannot tell them apart. The header can: sequences
        // start at one and cannot have gaps, so a backup taken at tail `n` holds
        // exactly `n` records, and fewer means the file lost some.
        truncated: applied < tail.get(),
    })
}

/// How much of a buffer a read managed to fill.
enum Filled {
    /// All of it.
    Whole,
    /// Some of it, and then the stream ended — a cut record.
    Short,
    /// None of it, which is the clean end of the file.
    Empty,
}

/// Read exactly as much as `into` holds, distinguishing a clean end from a cut.
///
/// `read_exact` collapses the two, and they mean different things: a file that
/// ends between records is complete, and one that ends inside a record is not.
fn fill(input: &mut impl Read, into: &mut [u8]) -> std::io::Result<Filled> {
    let mut held = 0;
    while held < into.len() {
        let Some(slot) = into.get_mut(held..) else {
            break;
        };
        let read = input.read(slot)?;
        if read == 0 {
            return Ok(if held == 0 {
                Filled::Empty
            } else {
                Filled::Short
            });
        }
        held = held.saturating_add(read);
    }
    Ok(Filled::Whole)
}
