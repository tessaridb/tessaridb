//! The backup file's fixed values and fixed-width encodings.

use super::*;

/// What every file of this kind begins with.
pub(crate) const MAGIC: &[u8; 10] = b"TESSARILOG";

/// What a file written before the name was corrected begins with.
///
/// Read, never written. See [`Head::magic`] for why it is still read.
pub(crate) const LEGACY_MAGIC: &[u8; 8] = b"TESSALOG";

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
pub(crate) const FORMAT: u8 = 4;

/// A frame that opens a section: one log, and the range of it this file holds.
pub(crate) const FRAME_SECTION: u8 = 1;

/// A frame that carries one log record, belonging to the section above it.
pub(crate) const FRAME_RECORD: u8 = 2;

/// A frame that opens a section for one SHARD's log (G031, ADR-0080).
///
/// Its own tag rather than a wider name in [`FRAME_SECTION`], so a file holding
/// no shard log keeps the bytes it always had, and a build that predates shards
/// meets a tag it refuses (`NotABackup`) instead of reading a shard's table and
/// shard ids as the start of a writer.
pub(crate) const FRAME_SHARD_SECTION: u8 = 3;

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
pub(crate) fn log_bytes(log: LogId) -> [u8; 25] {
    let mut bytes = [0_u8; 25];
    bytes[..9].copy_from_slice(&home_bytes(log.home));
    bytes[9..].copy_from_slice(&log.writer.bytes());
    bytes
}

/// The log [`log_bytes`] wrote, or `None` for a reach variant this build does
/// not know.
pub(crate) fn log_in(bytes: [u8; 25]) -> Option<LogId> {
    let homed: [u8; 9] = bytes[..9].try_into().ok()?;
    let writer: [u8; 16] = bytes[9..].try_into().ok()?;
    Some(LogId::new(home_in(homed)?, Writer::new(writer)))
}

/// A reach as the nine fixed bytes a log key carries it in.
///
/// For a shard these are the first nine of its seventeen; the table and the
/// shard follow in [`shard_log_bytes`], which is the only writer that has them.
fn home_bytes(home: Reach) -> [u8; 9] {
    let (variant, namespace, database) = match home {
        Reach::Store => (0_u8, 0_u32, 0_u32),
        Reach::Namespace(namespace) => (1, namespace.get(), 0),
        Reach::Database(namespace, database) => (2, namespace.get(), database.get()),
        Reach::Shard(namespace, database, _, _) => (3, namespace.get(), database.get()),
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

/// A shard's log as the thirty-three fixed bytes a shard section names it with:
/// the home's nine, the table, the shard, then the writer.
pub(crate) fn shard_log_bytes(log: LogId) -> Option<[u8; 33]> {
    let Reach::Shard(_, _, table, shard) = log.home else {
        return None;
    };
    let mut bytes = [0_u8; 33];
    bytes[..9].copy_from_slice(&home_bytes(log.home));
    bytes[9..13].copy_from_slice(&table.get().to_be_bytes());
    bytes[13..17].copy_from_slice(&shard.get().to_be_bytes());
    bytes[17..].copy_from_slice(&log.writer.bytes());
    Some(bytes)
}

/// The log [`shard_log_bytes`] wrote, or `None` when the bytes do not name one.
pub(crate) fn shard_log_in(bytes: [u8; 33]) -> Option<LogId> {
    if bytes[0] != 3 {
        return None;
    }
    let namespace = NamespaceId::new(u32::from_be_bytes(bytes[1..5].try_into().ok()?));
    let database = DatabaseId::new(u32::from_be_bytes(bytes[5..9].try_into().ok()?));
    let table = TableId::new(u32::from_be_bytes(bytes[9..13].try_into().ok()?));
    let shard = u32::from_be_bytes(bytes[13..17].try_into().ok()?);
    if shard == 0 {
        return None;
    }
    let writer: [u8; 16] = bytes[17..].try_into().ok()?;
    Some(LogId::new(
        Reach::Shard(namespace, database, table, ShardId::new(shard)),
        Writer::new(writer),
    ))
}

/// How many log records are read from the store at a time.
///
/// A backup that needed the whole log in memory would fail when it is most
/// needed. Five hundred is a page, not a limit.
pub(crate) const PAGE: usize = 500;
