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
//! header   "BGVDBLOG" <format:u8> <codec:u8> <writer:u32*3> <from:u64> <tail:u64>
//! record   <length:u32> <sequence:u64> <crc32:u32> <bytes…>
//! ```
//!
//! `from` is the first sequence the file holds, which is what makes an
//! **incremental** backup a thing a reader can check rather than a thing a
//! filename claims: a file starting at `from` restores onto a store standing at
//! `from - 1`, and onto no other.
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

use std::io::{Read, Write};

use bgv_db_encoding::{LogRecord, NodeVersion, StoreValue};
use bgv_db_storage::Store;
use bgv_db_types::Sequence;

mod check;

/// What every file of this kind begins with.
const MAGIC: &[u8; 8] = b"BGVDBLOG";

/// The format's own version, separate from the record codec's.
///
/// Two versions because they change for different reasons: the framing here can
/// gain a field without the records changing, and the records can change without
/// the framing moving.
///
/// It moved to 2 when the header gained the sequence a file **starts** at and
/// each record gained a checksum — a layout change, which is exactly what this
/// byte is for. A version-1 file is refused by name rather than misread.
///
/// It moved to 3 when the header gained the **build that wrote the file**. That
/// is a third question, separate from both of the versions above: the framing
/// can be identical and the records can decode perfectly while the build that
/// produced them meant something this one does not. See [`Head`].
const FORMAT: u8 = 3;

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

    /// The file was written by a build newer than this one.
    ///
    /// Refused, and deliberately **not** symmetric with an older file: an older
    /// backup restoring into a newer build is the ordinary case and the whole
    /// reason the version is recorded. The other direction is not, for the
    /// reason a newer on-disk format is refused — a newer writer may have given
    /// a record a meaning this build does not know, and the framing bytes cannot
    /// see that, because a newer build writes byte-identical framing.
    #[error(
        "this backup was written by version {found}; this build is {supported} \
         and will not guess at what a newer one meant"
    )]
    WrittenByNewer {
        /// The version that wrote the file.
        found: String,
        /// The version reading it.
        supported: String,
    },

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

    /// A restore into a store that is not where this file continues from.
    ///
    /// A whole backup starts at sequence 1 and needs an empty store; an
    /// incremental one starts at `from` and needs a store standing at
    /// `from - 1`. Anything else is not a restore: the sequences would land with
    /// a different meaning and the result would be a store no log explains.
    #[error("this backup continues from sequence {needs}; the store is at {found}")]
    WrongBase {
        /// Where the store would have to be.
        needs: u64,
        /// Where it actually is.
        found: u64,
    },

    /// A record whose bytes are not the bytes that were written.
    ///
    /// The check that framing cannot do. Raised rather than reported, because a
    /// record that decodes into something nobody wrote is worse than a restore
    /// that stops — and a caller who wants what is whole can verify first and
    /// restore to the last good sequence.
    #[error("the record at sequence {sequence} is damaged")]
    Damaged {
        /// Where in the log it was.
        sequence: u64,
    },
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// What a backup wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Written {
    /// How many log records it holds.
    pub records: u64,
    /// The first sequence it holds.
    pub from: Sequence,
    /// The sequence the store was at when it was taken.
    pub tail: Sequence,
    /// The build that wrote it.
    pub writer: NodeVersion,
}

/// What a backup turned out to hold, without any of it being applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verified {
    /// The build that wrote the file.
    pub written_by: NodeVersion,
    /// How many records read whole and checked out.
    pub records: u64,
    /// The first sequence the file says it holds.
    pub from: Sequence,
    /// The sequence the file says it was taken at.
    pub tail: Sequence,
    /// The last sequence that read whole and checked out.
    ///
    /// What a restore could safely be stopped at, which is the number somebody
    /// holding a damaged file actually needs.
    pub good_through: Sequence,
    /// Whether the file ended mid-record.
    pub truncated: bool,
}

/// What a restore applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Restored {
    /// The build that wrote the file.
    ///
    /// Reported rather than checked against anything beyond "not newer than
    /// this one": restoring an older backup into a newer build is the case this
    /// field exists to make visible, not one to refuse.
    pub written_by: NodeVersion,
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
    write_from(store, out, Sequence::new(1))
}

/// Write the part of a store's log at or after `from`.
///
/// An **incremental** backup. `from` is written into the header, so a reader
/// knows what the file continues from rather than being told by a filename —
/// and a restore refuses a store that is not standing exactly there.
///
/// `write_from(store, out, 1)` is a whole backup, which is what [`write`] is:
/// the two are one path, so an incremental restore exercises the code an
/// ordinary one does.
///
/// # Errors
///
/// Returns an error when the store or the stream fails.
pub fn write_from(store: &Store, out: &mut impl Write, from: Sequence) -> Result<Written> {
    let start = Sequence::new(from.get().max(1));
    let tail = store.committed_tail()?;
    let writer = NodeVersion::current();
    out.write_all(MAGIC)?;
    out.write_all(&[FORMAT, bgv_db_encoding::CODEC_VERSION])?;
    // Beside the other two versions, because it answers a question of the same
    // kind — and before the bounds, so that everything about *who wrote this* is
    // read before anything about *what it covers*.
    out.write_all(&writer.major.to_be_bytes())?;
    out.write_all(&writer.minor.to_be_bytes())?;
    out.write_all(&writer.patch.to_be_bytes())?;
    out.write_all(&start.get().to_be_bytes())?;
    out.write_all(&tail.get().to_be_bytes())?;

    let mut written = 0_u64;
    let mut from = start;
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
                    from: start,
                    tail,
                    writer,
                });
            }
            let bytes = record.encode();
            let body = bytes.as_slice();
            let length = u32::try_from(body.len()).unwrap_or(u32::MAX);
            out.write_all(&length.to_be_bytes())?;
            out.write_all(&sequence.get().to_be_bytes())?;
            out.write_all(&check::crc32(body).to_be_bytes())?;
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
        from: start,
        tail,
        writer,
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
    read_until(store, input, None)
}

/// Replay a backup, stopping at `upto` when one is given.
///
/// A **point-in-time** restore. The store is left holding exactly what it held
/// at that sequence, because the log *is* the store — there is no second
/// mechanism to rewind, and nothing to undo.
///
/// `upto` above what the file holds restores all of it; below what the store
/// needs as a base, nothing is applied. `read_until(store, input, None)` is a
/// whole restore, which is what [`read`] is.
///
/// # Errors
///
/// [`Error::NotABackup`] or [`Error::Unsupported`] before applying anything,
/// [`Error::WrongBase`] when the store is not where this file continues from,
/// [`Error::Damaged`] at the first record whose bytes are not the bytes that
/// were written, and the store's own error when a record cannot be applied.
pub fn read_until(
    store: &Store,
    input: &mut impl Read,
    upto: Option<Sequence>,
) -> Result<Restored> {
    let held = Head::read(input)?;
    // The store is checked against the file rather than against zero: a whole
    // backup continues from an empty store and an incremental one continues from
    // where its predecessor stopped, and both are the same question.
    let at = store.committed_tail()?;
    let needs = held.from.get().saturating_sub(1);
    if at.get() != needs {
        return Err(Error::WrongBase {
            needs,
            found: at.get(),
        });
    }

    let mut applied = 0_u64;
    let mut last = at;
    loop {
        let Some((sequence, body)) = next(input)? else {
            break;
        };
        let Some(body) = body else {
            return Ok(Restored {
                written_by: held.writer,
                records: applied,
                tail: held.tail,
                truncated: true,
            });
        };
        if let Some(upto) = upto
            && sequence.get() > upto.get()
        {
            // Stopped where the caller asked, which is not a truncation: the
            // file is whole and the store is deliberately behind it.
            return Ok(Restored {
                written_by: held.writer,
                records: applied,
                tail: held.tail,
                truncated: false,
            });
        }
        let record = LogRecord::decode(&body)?;
        store.apply_record(sequence, &record)?;
        applied = applied.saturating_add(1);
        last = sequence;
    }
    Ok(Restored {
        written_by: held.writer,
        records: applied,
        tail: held.tail,
        // A file cut cleanly *between* records ends the way a whole one does, so
        // the framing alone cannot tell them apart. The header can: a file
        // running from `from` to `tail` holds exactly that many records, and
        // fewer means the file lost some.
        truncated: last.get() < held.tail.get(),
    })
}

/// What a bootstrap left the node holding, and where it must continue from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bootstrapped {
    /// The build that wrote the prefix.
    pub written_by: NodeVersion,
    /// How many log records were applied.
    pub records: u64,
    /// The sequence to ask the leader for next.
    ///
    /// Taken from the node's **own** committed tail after the replay, never from
    /// what the prefix said it held — see [`bootstrap`].
    pub follow_from: Sequence,
    /// Whether the prefix ended mid-record.
    ///
    /// A truncated prefix leaves a node that is a correct copy of an *earlier*
    /// moment, not a broken one: `follow_from` is where it actually reached, so
    /// following resumes with nothing missed. Reported because the node is
    /// further behind than whoever sent the prefix intended.
    pub truncated: bool,
}

/// Bring an empty node up from a log prefix, and say where to follow from.
///
/// # Why this exists beside [`read`]
///
/// A caller could restore and then compute the position itself, and that is the
/// mistake this function is here to stop being made once per call site. Two
/// things make it sharper than a convenience.
///
/// The position is **one past** the last applied record, because the feed asks
/// for records *at or after* the position it is given — so following from the
/// applied tail re-delivers the record already applied, and following from one
/// past it is the same meaning [`bgv_db_storage::Changes`] already gives `next`.
///
/// And the position comes from the **node's own committed tail**, never from the
/// prefix's header. Those agree only when the whole prefix arrived. A prefix cut
/// in transit still restores everything before the cut, and a position taken
/// from the header would then place the node *past records it never received* —
/// a gap, silently, which is the one thing replication may not do. Taking it
/// from the store makes the node's claim about itself derive from what it holds.
///
/// # Errors
///
/// The same as [`read`]: [`Error::NotABackup`] or [`Error::Unsupported`] before
/// anything is applied, [`Error::WrongBase`] when the node is not standing where
/// this prefix continues from — which for a bootstrap means *not empty* — and
/// [`Error::Damaged`] at the first record whose bytes are not the bytes written.
pub fn bootstrap(store: &Store, input: &mut impl Read) -> Result<Bootstrapped> {
    let restored = read(store, input)?;
    let reached = store.committed_tail()?;
    Ok(Bootstrapped {
        written_by: restored.written_by,
        records: restored.records,
        follow_from: Sequence::new(reached.get().saturating_add(1)),
        truncated: restored.truncated,
    })
}

/// Read a backup without applying any of it.
///
/// What somebody does *before* the day they need it, and what a restore should
/// be preceded by on the day they do: it reads every record, checks every
/// checksum, and says how far the file is good for.
///
/// # Errors
///
/// [`Error::NotABackup`] or [`Error::Unsupported`] for a file this build cannot
/// read, and [`Error::Damaged`] at the first record whose bytes are not the
/// bytes that were written.
pub fn verify(input: &mut impl Read) -> Result<Verified> {
    let held = Head::read(input)?;
    let mut records = 0_u64;
    let mut good_through = Sequence::new(held.from.get().saturating_sub(1));
    loop {
        let Some((sequence, body)) = next(input)? else {
            break;
        };
        let Some(body) = body else {
            return Ok(Verified {
                written_by: held.writer,
                records,
                from: held.from,
                tail: held.tail,
                good_through,
                truncated: true,
            });
        };
        // Decoded as well as checksummed: a record whose bytes survived and
        // whose *shape* did not is a record a restore would fail on, and the
        // point of verifying is to find that out today.
        LogRecord::decode(&body)?;
        records = records.saturating_add(1);
        good_through = sequence;
    }
    Ok(Verified {
        written_by: held.writer,
        records,
        from: held.from,
        tail: held.tail,
        good_through,
        truncated: good_through.get() < held.tail.get(),
    })
}

/// What a backup's header says.
#[derive(Debug, Clone, Copy)]
struct Head {
    /// The build that wrote the file.
    writer: NodeVersion,
    from: Sequence,
    tail: Sequence,
}

impl Head {
    /// Read and check it, refusing anything this build cannot read **before**
    /// any record is looked at.
    fn read(input: &mut impl Read) -> Result<Self> {
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
        let format = versions.first().copied().unwrap_or(0);
        if format != FORMAT {
            return Err(Error::Unsupported {
                what: "format",
                found: format,
                supported: FORMAT,
            });
        }
        let codec = versions.get(1).copied().unwrap_or(0);
        if codec != bgv_db_encoding::CODEC_VERSION {
            return Err(Error::Unsupported {
                what: "record codec",
                found: codec,
                supported: bgv_db_encoding::CODEC_VERSION,
            });
        }
        let writer = Self::writer(input)?;
        // Newer is refused; older is not. The asymmetry is the point — see
        // `Error::WrittenByNewer`.
        let running = NodeVersion::current();
        if writer > running {
            return Err(Error::WrittenByNewer {
                found: writer.to_string(),
                supported: running.to_string(),
            });
        }
        let mut bounds = [0_u8; 16];
        input
            .read_exact(&mut bounds)
            .map_err(|_| Error::NotABackup)?;
        let (from, tail) = bounds.split_at(8);
        Ok(Self {
            writer,
            from: Sequence::new(u64::from_be_bytes(
                from.try_into().map_err(|_| Error::NotABackup)?,
            )),
            tail: Sequence::new(u64::from_be_bytes(
                tail.try_into().map_err(|_| Error::NotABackup)?,
            )),
        })
    }

    /// The three numbers naming the build that wrote the file.
    fn writer(input: &mut impl Read) -> Result<NodeVersion> {
        let mut raw = [0_u8; 12];
        input.read_exact(&mut raw).map_err(|_| Error::NotABackup)?;
        let (major, rest) = raw.split_at(4);
        let (minor, patch) = rest.split_at(4);
        Ok(NodeVersion {
            major: u32::from_be_bytes(major.try_into().map_err(|_| Error::NotABackup)?),
            minor: u32::from_be_bytes(minor.try_into().map_err(|_| Error::NotABackup)?),
            patch: u32::from_be_bytes(patch.try_into().map_err(|_| Error::NotABackup)?),
        })
    }
}

/// The next record: its sequence and its body, or `None` at a clean end.
///
/// A body of `None` means the file was cut inside this record.
fn next(input: &mut impl Read) -> Result<Option<(Sequence, Option<Vec<u8>>)>> {
    let mut header = [0_u8; 16];
    match fill(input, &mut header)? {
        Filled::Empty => return Ok(None),
        Filled::Short => return Ok(Some((Sequence::new(0), None))),
        Filled::Whole => {}
    }
    let (framing, rest) = header.split_at(4);
    let (numbered, checked) = rest.split_at(8);
    let length = usize::try_from(u32::from_be_bytes(
        framing.try_into().map_err(|_| Error::NotABackup)?,
    ))
    .unwrap_or(0);
    let sequence = Sequence::new(u64::from_be_bytes(
        numbered.try_into().map_err(|_| Error::NotABackup)?,
    ));
    let expected = u32::from_be_bytes(checked.try_into().map_err(|_| Error::NotABackup)?);

    let mut body = vec![0_u8; length];
    if !matches!(fill(input, &mut body)?, Filled::Whole) {
        return Ok(Some((sequence, None)));
    }
    if check::crc32(&body) != expected {
        return Err(Error::Damaged {
            sequence: sequence.get(),
        });
    }
    Ok(Some((sequence, Some(body))))
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
