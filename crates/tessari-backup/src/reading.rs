//! Reading a backup back: into a store, into a new node, or only to say what it holds.

use super::format::{Frame, Head};
use super::{Bootstrapped, Error, LogSpan, Restored, Verified, VerifiedLog};
use crate::Result;
use std::io::Read;
use tessari_encoding::{LogId, LogRecord, StoreValue};
use tessari_storage::Store;
use tessari_types::Sequence;

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
                // The log the SECTION named, home and writer both: a restore
                // files a record where the backup read it from and never where
                // the restoring node happens to write. Deriving the home from
                // the record instead refused every store whose records were
                // filed before each database had a log of its own — they sit in
                // the store log and derive a database home today.
                store.apply_record_in(section.log, sequence, &record)?;
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

/// How much of a buffer a read managed to fill.
pub(crate) enum Filled {
    /// All of it.
    Whole,
    /// Some of it, and then the stream ended — a cut record.
    Short,
    /// None of it, which is the clean end of the file.
    Empty,
}
