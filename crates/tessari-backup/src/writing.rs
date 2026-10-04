//! Writing a store's logs out as a backup.

use super::*;

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
    // The home before the bounds, because the bounds mean nothing without it:
    // a position counts in one log, and a section that named a range without
    // naming which log it counted in would restore onto the wrong base with no
    // error (Q-621).
    if let Some(named) = shard_log_bytes(log) {
        out.write_all(&[FRAME_SHARD_SECTION])?;
        out.write_all(&named)?;
    } else {
        out.write_all(&[FRAME_SECTION])?;
        out.write_all(&log_bytes(log))?;
    }
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
