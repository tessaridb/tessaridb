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

use std::io::{Read, Write};

use tessari_encoding::{LogId, LogRecord, NodeVersion, StoreValue, Writer};
use tessari_storage::Store;
use tessari_types::{DatabaseId, NamespaceId, Reach, Sequence};

mod check;

/// What every file of this kind begins with.
const MAGIC: &[u8; 10] = b"TESSARILOG";

/// What a file written before the name was corrected begins with.
///
/// Read, never written. See [`Head::magic`] for why it is still read.
const LEGACY_MAGIC: &[u8; 8] = b"TESSALOG";

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
///
/// It moved to 4 when a store stopped holding one log. The file now carries a
/// section per log, and every frame carries a tag — a layout change that a
/// version-3 reader would misread rather than refuse, which is what this byte
/// prevents.
const FORMAT: u8 = 4;

/// A frame that opens a section: one log, and the range of it this file holds.
const FRAME_SECTION: u8 = 1;

/// A frame that carries one log record, belonging to the section above it.
const FRAME_RECORD: u8 = 2;

/// A log as the twenty-five fixed bytes a section names it with.
///
/// Written out here rather than borrowed from the key encoding because a backup
/// file is its own format: the key grammar may be re-laid out without every file
/// ever written becoming unreadable, and the two moving together by accident is
/// exactly what a separate format is for.
///
/// The writer is part of the name and not an optional tail. A section that named
/// the home alone would restore two writers' logs into one — silently, because
/// every record in them is valid and only the counters collide — which is the
/// failure a per-writer log exists to prevent.
fn log_bytes(log: LogId) -> [u8; 25] {
    let mut bytes = [0_u8; 25];
    bytes[..9].copy_from_slice(&home_bytes(log.home));
    bytes[9..].copy_from_slice(&log.writer.bytes());
    bytes
}

/// The log [`log_bytes`] wrote, or `None` for a reach variant this build does
/// not know.
fn log_in(bytes: [u8; 25]) -> Option<LogId> {
    let homed: [u8; 9] = bytes[..9].try_into().ok()?;
    let writer: [u8; 16] = bytes[9..].try_into().ok()?;
    Some(LogId::new(home_in(homed)?, Writer::new(writer)))
}

/// A reach as the nine fixed bytes a log key carries it in.
fn home_bytes(home: Reach) -> [u8; 9] {
    let (variant, namespace, database) = match home {
        Reach::Store => (0_u8, 0_u32, 0_u32),
        Reach::Namespace(namespace) => (1, namespace.get(), 0),
        Reach::Database(namespace, database) => (2, namespace.get(), database.get()),
    };
    let mut bytes = [0_u8; 9];
    bytes[0] = variant;
    bytes[1..5].copy_from_slice(&namespace.to_be_bytes());
    bytes[5..9].copy_from_slice(&database.to_be_bytes());
    bytes
}

/// The reach [`home_bytes`] wrote, or `None` for a variant this build does not
/// know.
fn home_in(bytes: [u8; 9]) -> Option<Reach> {
    let namespace = NamespaceId::new(u32::from_be_bytes(bytes[1..5].try_into().ok()?));
    let database = DatabaseId::new(u32::from_be_bytes(bytes[5..9].try_into().ok()?));
    match bytes[0] {
        0 => Some(Reach::Store),
        1 => Some(Reach::Namespace(namespace)),
        2 => Some(Reach::Database(namespace, database)),
        _ => None,
    }
}

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
    Store(#[from] tessari_storage::Error),

    /// A record could not be decoded.
    #[error(transparent)]
    Encoding(#[from] tessari_encoding::Error),

    /// One sequence was named where several logs are in play.
    ///
    /// A whole backup and a whole restore span every log a store holds. The two
    /// surfaces bounded by a single sequence — an incremental backup `FROM n`
    /// and a point-in-time restore `UPTO n` — cannot: sequence 500 in one log
    /// and sequence 500 in another are unrelated moments, and a number that
    /// silently meant the first of them would produce a store no log explains.
    /// Refused rather than guessed at until both surfaces name a position per
    /// log (Q-624, Q-621).
    #[error(
        "{what} names one sequence and {logs} logs are in play; \
         a sequence counts in one log alone"
    )]
    ManyLogs {
        /// Which surface named the sequence.
        what: &'static str,
        /// How many logs are in play.
        logs: usize,
    },

    /// The file does not begin the way one of these does.
    #[error("this is not a TessariDB backup")]
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

/// One log, and the range of it a file's section holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogSpan {
    /// The log the section holds.
    pub log: LogId,
    /// The first sequence of it the section holds.
    pub from: Sequence,
    /// The sequence that log was at when the section was taken.
    pub tail: Sequence,
}

/// What a backup wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    /// How many log records it holds, across every section.
    pub records: u64,
    /// What each section covers, in the order they were written.
    ///
    /// A list rather than one pair because a store holds a log per range, and
    /// reporting the last section's bounds as the file's would be a number that
    /// is right about a part and wrong about the whole.
    pub logs: Vec<LogSpan>,
    /// The build that wrote it.
    pub writer: NodeVersion,
}

/// One section of a file, as reading it without applying it found it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifiedLog {
    /// What the section says it covers.
    pub span: LogSpan,
    /// The last sequence in it that read whole and checked out.
    ///
    /// What a restore of this log could safely be stopped at, which is the
    /// number somebody holding a damaged file actually needs.
    pub good_through: Sequence,
}

/// What a backup turned out to hold, without any of it being applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    /// The build that wrote the file.
    pub written_by: NodeVersion,
    /// How many records read whole and checked out, across every section.
    pub records: u64,
    /// What each section says it holds, and how far it actually reads.
    pub logs: Vec<VerifiedLog>,
    /// Whether the file ended mid-record, or before every section arrived.
    pub truncated: bool,
}

/// What a restore applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restored {
    /// The build that wrote the file.
    ///
    /// Reported rather than checked against anything beyond "not newer than
    /// this one": restoring an older backup into a newer build is the case this
    /// field exists to make visible, not one to refuse.
    pub written_by: NodeVersion,
    /// How many records were applied, across every section.
    pub records: u64,
    /// What each section the restore reached said it covered.
    pub logs: Vec<LogSpan>,
    /// Whether the file ended mid-record, or before every section arrived.
    ///
    /// Reported rather than raised: an interrupted backup is still most of a
    /// store, and the caller is the one who knows whether most is enough.
    pub truncated: bool,
}

/// Write every log a store holds to `out`.
///
/// One section per log, in the order [`tessari_storage::Store::homes`] lists
/// them — which puts the store's own log first, so the namespace and database
/// definitions a range's records depend on are restored before those records
/// are.
///
/// # Errors
///
/// Returns an error when the store or the stream fails.
pub fn write(store: &Store, out: &mut impl Write) -> Result<Written> {
    let homes = store.logs()?;
    let sections = u32::try_from(homes.len()).unwrap_or(u32::MAX);
    let writer = write_head(out, sections)?;
    let mut records = 0_u64;
    let mut logs = Vec::with_capacity(homes.len());
    for home in homes {
        let (written, span) = write_section(store, out, home, Sequence::new(1))?;
        records = records.saturating_add(written);
        logs.push(span);
    }
    Ok(Written {
        records,
        logs,
        writer,
    })
}

/// Write the part of one log at or after `from`.
///
/// An **incremental** backup, and a one-section file. `from` is written into the
/// section, so a reader knows what the file continues from rather than being
/// told by a filename — and a restore refuses a store whose log for that home is
/// not standing exactly there.
///
/// `write_from(store, out, home, 1)` is a whole backup **of that one log**, not
/// of the store: a store holding several logs is backed up whole by [`write`].
/// The two share every byte of their framing, so an incremental restore
/// exercises the code an ordinary one does.
///
/// # Errors
///
/// Returns an error when the store or the stream fails.
pub fn write_from(
    store: &Store,
    out: &mut impl Write,
    log: LogId,
    from: Sequence,
) -> Result<Written> {
    let writer = write_head(out, 1)?;
    let (records, span) = write_section(store, out, log, from)?;
    Ok(Written {
        records,
        logs: vec![span],
        writer,
    })
}

/// The one log a sequence-bounded backup can name, or a refusal.
///
/// A store that has only ever been written through one leader holds one log, and
/// that log is what an incremental backup counts in. A store holding several is
/// refused rather than partly written: `FROM n` would silently mean the first
/// log's sequence `n` and leave every other log out of a file that reads as
/// whole (Q-624).
///
/// An empty store answers the store's own log, unattributed — it has no log
/// yet, and nothing has written into one to name a writer for.
///
/// # Errors
///
/// Returns [`Error::ManyLogs`] when the store holds more than one log, and the
/// store's own failure when they cannot be listed.
pub fn only_log(store: &Store) -> Result<LogId> {
    match store.logs()?.as_slice() {
        [] => Ok(LogId::unattributed(Reach::Store)),
        [log] => Ok(*log),
        many => Err(Error::ManyLogs {
            what: "an incremental backup",
            logs: many.len(),
        }),
    }
}

/// Write what a file begins with, and answer the build that wrote it.
fn write_head(out: &mut impl Write, sections: u32) -> Result<NodeVersion> {
    let writer = NodeVersion::current();
    out.write_all(MAGIC)?;
    out.write_all(&[FORMAT, tessari_encoding::CODEC_VERSION])?;
    // Beside the other two versions, because it answers a question of the same
    // kind — and before anything about what the file covers, so that everything
    // about *who wrote this* is read first.
    out.write_all(&writer.major.to_be_bytes())?;
    out.write_all(&writer.minor.to_be_bytes())?;
    out.write_all(&writer.patch.to_be_bytes())?;
    // How many sections follow, in the head rather than discovered by reading
    // them: a restore bounded by one sequence has to refuse a multi-log file
    // BEFORE it applies the first section, and after the first section it is too
    // late to refuse anything.
    out.write_all(&sections.to_be_bytes())?;
    Ok(writer)
}

/// Write one log's section, and answer how many records went into it.
fn write_section(
    store: &Store,
    out: &mut impl Write,
    log: LogId,
    from: Sequence,
) -> Result<(u64, LogSpan)> {
    let start = Sequence::new(from.get().max(1));
    let tail = store.committed_tail(log)?;
    out.write_all(&[FRAME_SECTION])?;
    // The home before the bounds, because the bounds mean nothing without it:
    // a position counts in one log, and a section that named a range without
    // naming which log it counted in would restore onto the wrong base with no
    // error (Q-621).
    out.write_all(&log_bytes(log))?;
    out.write_all(&start.get().to_be_bytes())?;
    out.write_all(&tail.get().to_be_bytes())?;
    let span = LogSpan {
        log,
        from: start,
        tail,
    };

    let mut written = 0_u64;
    let mut from = start;
    loop {
        let page = store.log_records(log, from, PAGE)?;
        if page.is_empty() {
            break;
        }
        for (sequence, record) in &page {
            if sequence.get() > tail.get() {
                // A write that landed after the backup began is simply not in
                // it. The tail in the section is what makes that honest rather
                // than arbitrary.
                return Ok((written, span));
            }
            let bytes = record.encode();
            let body = bytes.as_slice();
            let length = u32::try_from(body.len()).unwrap_or(u32::MAX);
            out.write_all(&[FRAME_RECORD])?;
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
    Ok((written, span))
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
    let head = Head::read(input)?;
    // Refused here, before a byte of any section is applied: after the first
    // section there is no way to refuse that is not a half-applied restore, and
    // this is the whole reason the section count lives in the head.
    if upto.is_some() && head.sections != 1 {
        return Err(Error::ManyLogs {
            what: "a point-in-time restore",
            logs: usize::try_from(head.sections).unwrap_or(usize::MAX),
        });
    }

    let mut applied = 0_u64;
    // The logs this file has opened, so a later section can tell *a range this
    // file is restoring* from *a range the target already held*.
    let mut opened: Vec<LogId> = Vec::new();
    let mut logs: Vec<LogSpan> = Vec::new();
    let mut open: Option<(LogSpan, Sequence)> = None;
    let mut truncated = false;
    loop {
        let Some(frame) = Frame::next(input)? else {
            break;
        };
        match frame {
            Frame::Cut => {
                truncated = true;
                break;
            }
            Frame::Section(span) => {
                // The store is checked against the section rather than against
                // zero: a whole backup continues from an empty log and an
                // incremental one continues from where its predecessor stopped,
                // and both are the same question.
                let at = store.committed_tail(span.log)?;
                let needs = span.from.get().saturating_sub(1);
                if at.get() != needs {
                    return Err(Error::WrongBase {
                        needs,
                        found: at.get(),
                    });
                }
                // And the same question asked of the RANGE, which the check
                // above stopped answering the moment a home could hold more
                // than one log. A file restored into a store that already holds
                // that range under a DIFFERENT writer passes the line above
                // perfectly — the target's copy of this log is empty, because
                // it never had one — and merges two stores that were never a
                // cluster, silently. What a restore may continue from is what
                // this file has itself put there.
                for held in store.logs_of(span.log.home)? {
                    if held == span.log || opened.contains(&held) {
                        continue;
                    }
                    if store.committed_tail(held)?.get() > 0 {
                        return Err(Error::WrongBase {
                            needs: 0,
                            found: store.committed_tail(held)?.get(),
                        });
                    }
                }
                opened.push(span.log);
                if let Some((span, reached)) = open.replace((span, at)) {
                    truncated = truncated || reached.get() < span.tail.get();
                    logs.push(span);
                }
            }
            Frame::Record { sequence, body } => {
                let Some((section, reached)) = open.as_mut() else {
                    // A record before any section names the log it belongs to.
                    // There is no defensible guess: applying it into the store's
                    // own log would put a range's records in the wrong counter.
                    return Err(Error::NotABackup);
                };
                if let Some(upto) = upto
                    && sequence.get() > upto.get()
                {
                    // Stopped where the caller asked, which is not a truncation:
                    // the file is whole and the store is deliberately behind it.
                    break;
                }
                let record = LogRecord::decode(&body)?;
                // The writer the SECTION named, which is the same fact the
                // home already was: a restore files a record where the backup
                // read it from and never where the restoring node happens to
                // write. Deriving the writer from the record is not available
                // and would be wrong if it were.
                store.apply_record(section.log.writer, sequence, &record)?;
                applied = applied.saturating_add(1);
                *reached = sequence;
            }
        }
    }
    if let Some((span, reached)) = open {
        // A file cut cleanly *between* records ends the way a whole one does, so
        // the framing alone cannot tell them apart. The section can: one running
        // from `from` to `tail` holds exactly that many records, and fewer means
        // the file lost some. `upto` is the exception — there the store is
        // deliberately behind — and it only ever reaches one section.
        truncated = truncated || (upto.is_none() && reached.get() < span.tail.get());
        logs.push(span);
    }
    Ok(Restored {
        written_by: head.writer,
        records: applied,
        // A file that ends before every section it promised arrived lost whole
        // logs, not a tail — which the per-section check above cannot see,
        // because the sections it never reached left no trace in the stream.
        truncated: truncated || logs.len() < usize::try_from(head.sections).unwrap_or(0),
        logs,
    })
}

/// What a bootstrap left the node holding, and where it must continue from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bootstrapped {
    /// The build that wrote the prefix.
    pub written_by: NodeVersion,
    /// How many log records were applied.
    pub records: u64,
    /// The sequence to ask the leader for next, per log the node now holds.
    ///
    /// Taken from the node's **own** committed tail after the replay, never from
    /// what the prefix said it held — see [`bootstrap`]. One entry per log,
    /// because a node holding several has several positions and a single number
    /// would be right about one of them (Q-620, Q-621).
    pub follow_from: Vec<(LogId, Sequence)>,
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
/// past it is the same meaning [`tessari_storage::Changes`] already gives `next`.
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
    let mut follow_from = Vec::with_capacity(restored.logs.len());
    for log in store.logs()? {
        let reached = store.committed_tail(log)?;
        follow_from.push((log, Sequence::new(reached.get().saturating_add(1))));
    }
    Ok(Bootstrapped {
        written_by: restored.written_by,
        records: restored.records,
        follow_from,
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
    let head = Head::read(input)?;
    let mut records = 0_u64;
    let mut logs: Vec<VerifiedLog> = Vec::new();
    let mut open: Option<VerifiedLog> = None;
    let mut truncated = false;
    loop {
        let Some(frame) = Frame::next(input)? else {
            break;
        };
        match frame {
            Frame::Cut => {
                truncated = true;
                break;
            }
            Frame::Section(span) => {
                let opening = VerifiedLog {
                    span,
                    good_through: Sequence::new(span.from.get().saturating_sub(1)),
                };
                if let Some(done) = open.replace(opening) {
                    truncated = truncated || done.good_through.get() < done.span.tail.get();
                    logs.push(done);
                }
            }
            Frame::Record { sequence, body } => {
                let Some(current) = open.as_mut() else {
                    return Err(Error::NotABackup);
                };
                // Decoded as well as checksummed: a record whose bytes survived
                // and whose *shape* did not is a record a restore would fail on,
                // and the point of verifying is to find that out today.
                LogRecord::decode(&body)?;
                records = records.saturating_add(1);
                current.good_through = sequence;
            }
        }
    }
    if let Some(done) = open {
        truncated = truncated || done.good_through.get() < done.span.tail.get();
        logs.push(done);
    }
    Ok(Verified {
        written_by: head.writer,
        records,
        truncated: truncated || logs.len() < usize::try_from(head.sections).unwrap_or(0),
        logs,
    })
}

/// What a backup file begins with, before any section.
#[derive(Debug, Clone, Copy)]
struct Head {
    /// The build that wrote the file.
    writer: NodeVersion,
    /// How many sections — that is, how many logs — the file holds.
    sections: u32,
}

impl Head {
    /// Read and check it, refusing anything this build cannot read **before**
    /// any record is looked at.
    fn read(input: &mut impl Read) -> Result<Self> {
        Self::magic(input)?;
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
        if codec != tessari_encoding::CODEC_VERSION {
            return Err(Error::Unsupported {
                what: "record codec",
                found: codec,
                supported: tessari_encoding::CODEC_VERSION,
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
        let mut counted = [0_u8; 4];
        input
            .read_exact(&mut counted)
            .map_err(|_| Error::NotABackup)?;
        Ok(Self {
            writer,
            sections: u32::from_be_bytes(counted),
        })
    }

    /// Read the name the file begins with, accepting the one older files carry.
    ///
    /// The old name dropped two letters out of the product's own stem and was
    /// corrected, which moved the bytes a new file starts with. A file written
    /// before that is still a backup, and this header's whole asymmetry —
    /// a newer writer refused, an older one welcomed — says that older files
    /// are the ordinary case. Refusing one over a rename would contradict that,
    /// and it would do it by reporting `NotABackup`, which is not true of the
    /// file in hand.
    fn magic(input: &mut impl Read) -> Result<()> {
        let mut opening = [0_u8; 8];
        input
            .read_exact(&mut opening)
            .map_err(|_| Error::NotABackup)?;
        if &opening == LEGACY_MAGIC {
            return Ok(());
        }
        let (lead, rest) = MAGIC.split_at(opening.len());
        if opening != *lead {
            return Err(Error::NotABackup);
        }
        let mut trailing = [0_u8; 2];
        input
            .read_exact(&mut trailing)
            .map_err(|_| Error::NotABackup)?;
        if trailing != *rest {
            return Err(Error::NotABackup);
        }
        Ok(())
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

/// One frame of a backup file.
#[derive(Debug)]
enum Frame {
    /// A section opening: the log that follows, and the range of it held.
    Section(LogSpan),
    /// One log record, belonging to the section above it.
    Record {
        /// Where in that log it was.
        sequence: Sequence,
        /// Its encoded bytes, checksum already agreed.
        body: Vec<u8>,
    },
    /// The file ended inside a frame.
    Cut,
}

impl Frame {
    /// The next frame, or `None` at a clean end of the file.
    fn next(input: &mut impl Read) -> Result<Option<Self>> {
        let mut tag = [0_u8; 1];
        match fill(input, &mut tag)? {
            Filled::Empty => return Ok(None),
            Filled::Short | Filled::Whole => {}
        }
        match tag.first().copied().unwrap_or(0) {
            FRAME_SECTION => Self::section(input),
            FRAME_RECORD => Self::record(input),
            // Not a truncation and not a guess: the framing is self-describing,
            // so a tag this build does not know is a file it cannot read rather
            // than one it should skip past.
            _ => Err(Error::NotABackup),
        }
    }

    /// A section frame: the log, then the bounds that count inside it.
    fn section(input: &mut impl Read) -> Result<Option<Self>> {
        let mut raw = [0_u8; 41];
        if !matches!(fill(input, &mut raw)?, Filled::Whole) {
            return Ok(Some(Self::Cut));
        }
        let (named, bounds) = raw.split_at(25);
        let named: [u8; 25] = named.try_into().map_err(|_| Error::NotABackup)?;
        let log = log_in(named).ok_or(Error::NotABackup)?;
        let (from, tail) = bounds.split_at(8);
        Ok(Some(Self::Section(LogSpan {
            log,
            from: Sequence::new(u64::from_be_bytes(
                from.try_into().map_err(|_| Error::NotABackup)?,
            )),
            tail: Sequence::new(u64::from_be_bytes(
                tail.try_into().map_err(|_| Error::NotABackup)?,
            )),
        })))
    }

    /// A record frame: its length, its sequence, its checksum, its bytes.
    fn record(input: &mut impl Read) -> Result<Option<Self>> {
        let mut header = [0_u8; 16];
        if !matches!(fill(input, &mut header)?, Filled::Whole) {
            return Ok(Some(Self::Cut));
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
            return Ok(Some(Self::Cut));
        }
        if check::crc32(&body) != expected {
            return Err(Error::Damaged {
                sequence: sequence.get(),
            });
        }
        Ok(Some(Self::Record { sequence, body }))
    }
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
