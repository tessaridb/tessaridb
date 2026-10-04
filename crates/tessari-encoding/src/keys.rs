//! Typed store keys.
//!
//! Each key type declares, in the type system, which value type it addresses. A
//! shared `get(key) -> bytes` helper cannot do that, and the resulting mistake —
//! decoding one entity's bytes as another type — surfaces far away from the code
//! that caused it.
//!
//! Keys carry identity and nothing else. Names live in the catalog, so renaming
//! a namespace, database or table rewrites one catalog entry rather than every
//! record and every index entry that mentions it.

mod node;
use tessari_kv::{Key, Keyspace};
use tessari_types::{DatabaseId, NamespaceId, Reach, RecordId, Sequence, ShardId, TableId};

use crate::error::Result;
use crate::kind::KeyKind;
use crate::log_id::{LogId, Writer};
use crate::node::NODE_ID_LEN;
use crate::order::{KeyReader, KeyWriter};
use crate::record_id;
use crate::value::StoreValue;
pub use node::{
    AppliedPositionKey, FormatVersionKey, LogRetentionKey, LogStartKey, NodeIdentityKey,
    ReclaimFloorKey, ServedReach, ServedReachKey, VersionPositionKey,
};

/// Bytes of a record key that identify the table: the kind tag plus the three
/// tenancy identifiers.
///
/// This is a fixed width on purpose. A meaningful prefix has to be a fixed
/// leading byte count for a prefix filter to be able to extract it, and that
/// constraint is the whole reason the identifiers are numbers rather than names.
pub const TABLE_PREFIX_LEN: usize = 13;

/// Bytes a [`Reach`] occupies inside a key: a variant byte, then the namespace
/// and the database, both always written.
///
/// Fixed width on purpose, for the reason [`TABLE_PREFIX_LEN`] is. A log key is
/// read by prefix, and writing the identifiers only where the variant uses them
/// would start the sequence that follows at three different offsets — a scan
/// over one home would then be a scan over whatever happened to sort into the
/// same bytes.
pub const REACH_LEN: usize = 9;

/// The variant byte for [`Reach::Store`].
const REACH_STORE: u8 = 0;
/// The variant byte for [`Reach::Namespace`].
const REACH_NAMESPACE: u8 = 1;
/// The variant byte for [`Reach::Database`].
const REACH_DATABASE: u8 = 2;
/// The variant byte for [`Reach::Shard`].
///
/// The one variant written wider than [`REACH_LEN`]: the table and the shard
/// follow the nine bytes every variant has. Every home is still one contiguous
/// prefix, because the variant byte leads and fixes the width — two homes of
/// different variants differ in their first byte, and two of one variant have
/// one width. No key written before shards existed moves.
const REACH_SHARD: u8 = 3;

/// Append a reach as [`REACH_LEN`] bytes.
///
/// The variant leads, so the three levels do not interleave and every home is
/// one contiguous range. The order *between* homes carries no meaning — they
/// are separate logs, and nothing compares a position in one against a position
/// in another — so contiguity is the whole requirement.
pub(crate) fn put_reach(writer: &mut KeyWriter, reach: Reach) {
    let (variant, namespace, database) = match reach {
        Reach::Store => (REACH_STORE, 0, 0),
        Reach::Namespace(namespace) => (REACH_NAMESPACE, namespace.get(), 0),
        Reach::Database(namespace, database) => (REACH_DATABASE, namespace.get(), database.get()),
        Reach::Shard(namespace, database, _, _) => (REACH_SHARD, namespace.get(), database.get()),
    };
    writer.put_u8(variant).put_u32(namespace).put_u32(database);
    if let Reach::Shard(_, _, table, shard) = reach {
        writer.put_u32(table.get()).put_u32(shard.get());
    }
}

/// Read a reach written by [`put_reach`].
///
/// A variant this build does not know is refused rather than widened to the
/// store: a record filed under a home this binary cannot name is a record it
/// cannot decide the destination of, and answering `Store` would hand it to
/// every subscriber.
///
/// # Errors
///
/// Returns [`Error::UnknownReach`] for an unknown variant byte, and whatever
/// the reader returns when the bytes are short.
///
/// [`Error::UnknownReach`]: crate::error::Error::UnknownReach
pub(crate) fn take_reach(reader: &mut KeyReader<'_>) -> Result<Reach> {
    let offset = reader.position();
    let variant = reader.take_u8()?;
    let namespace = NamespaceId::new(reader.take_u32()?);
    let database = DatabaseId::new(reader.take_u32()?);
    match variant {
        REACH_STORE => Ok(Reach::Store),
        REACH_NAMESPACE => Ok(Reach::Namespace(namespace)),
        REACH_DATABASE => Ok(Reach::Database(namespace, database)),
        REACH_SHARD => Ok(Reach::Shard(
            namespace,
            database,
            TableId::new(reader.take_u32()?),
            ShardId::new(reader.take_u32()?),
        )),
        found => Err(crate::error::Error::UnknownReach {
            kind: reader.kind(),
            found,
            offset,
        }),
    }
}

/// Append a log's name: its home, then its writer.
///
/// The home leads so that every log of one range is one contiguous span, which
/// is what lets a reader ask *which logs does this range have* with a single
/// bound. The writer follows at a fixed width so the sequence after it starts at
/// one offset and a per-log prefix stays exact.
fn put_log(writer: &mut KeyWriter, log: LogId) {
    put_reach(writer, log.home);
    writer.put_fixed(&log.writer.bytes());
}

/// Read a log name written by [`put_log`].
///
/// # Errors
///
/// Returns whatever [`take_reach`] returns, and whatever the reader returns when
/// the bytes are short.
fn take_log(reader: &mut KeyReader<'_>) -> Result<LogId> {
    let home = take_reach(reader)?;
    let writer = Writer::new(reader.take_fixed::<NODE_ID_LEN>()?);
    Ok(LogId::new(home, writer))
}

/// A key that addresses one kind of stored value.
pub trait StoreKey: Sized {
    /// The value type stored under this key.
    type Value: StoreValue;

    /// The kind tag this key carries.
    const KIND: KeyKind;

    /// Encode to the bytes used in the substrate.
    fn encode(&self) -> Key;

    /// Decode from bytes read out of the substrate.
    ///
    /// # Errors
    ///
    /// Returns an error when the bytes carry a different kind tag, are
    /// truncated, hold an unterminated component, or have trailing bytes.
    fn decode(bytes: &[u8]) -> Result<Self>;

    /// The keyspace this key lives in.
    #[must_use]
    fn keyspace() -> Keyspace {
        Self::KIND.keyspace()
    }
}

/// Addresses one version of one record.
///
/// ```text
/// <0x01> <namespace:u32> <database:u32> <table:u32> <record-id> <!version:u64>
/// ```
///
/// The version suffix is the complement of the sequence, so the versions of one
/// record sort newest-first and a snapshot read is a seek followed by taking the
/// first entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordKey {
    /// The namespace the record belongs to.
    pub namespace: NamespaceId,
    /// The database within that namespace.
    pub database: DatabaseId,
    /// The table within that database.
    pub table: TableId,
    /// The record's identity within the table.
    pub id: RecordId,
    /// The sequence at which this version was written.
    pub version: Sequence,
}

impl RecordKey {
    /// Build a record key.
    #[must_use]
    pub const fn new(
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
        id: RecordId,
        version: Sequence,
    ) -> Self {
        Self {
            namespace,
            database,
            table,
            id,
            version,
        }
    }

    /// The prefix shared by every record in one table.
    ///
    /// Always [`TABLE_PREFIX_LEN`] bytes long.
    #[must_use]
    pub fn table_prefix(namespace: NamespaceId, database: DatabaseId, table: TableId) -> Vec<u8> {
        let mut writer = KeyWriter::with_capacity(TABLE_PREFIX_LEN);
        writer
            .put_u8(KeyKind::Record.tag())
            .put_u32(namespace.get())
            .put_u32(database.get())
            .put_u32(table.get());
        writer.finish()
    }

    /// The prefix shared by every version of one record.
    ///
    /// Bounding a scan with this is what keeps a snapshot read inside the record
    /// it asked for instead of running on into the next one.
    #[must_use]
    pub fn versions_prefix(
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
        id: &RecordId,
    ) -> Vec<u8> {
        let mut writer = KeyWriter::with_capacity(TABLE_PREFIX_LEN.saturating_add(24));
        writer
            .put_u8(KeyKind::Record.tag())
            .put_u32(namespace.get())
            .put_u32(database.get())
            .put_u32(table.get());
        record_id::put(&mut writer, id);
        writer.finish()
    }
}

impl StoreKey for RecordKey {
    type Value = crate::value::RecordValue;

    const KIND: KeyKind = KeyKind::Record;

    fn encode(&self) -> Key {
        let mut bytes = Self::versions_prefix(self.namespace, self.database, self.table, &self.id);
        bytes.extend_from_slice(&(!self.version.get()).to_be_bytes());
        Key::from(bytes)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut reader = KeyReader::new(Self::KIND, bytes);
        reader.expect_kind()?;
        let namespace = NamespaceId::new(reader.take_u32()?);
        let database = DatabaseId::new(reader.take_u32()?);
        let table = TableId::new(reader.take_u32()?);
        let id = record_id::take(&mut reader)?;
        let version = Sequence::new(reader.take_u64_descending()?);
        reader.finish()?;
        Ok(Self {
            namespace,
            database,
            table,
            id,
            version,
        })
    }
}

/// Addresses one entry in the ordered log.
///
/// ```text
/// <0x20> <home:9> <writer:16> <sequence:u64>
/// ```
///
/// # The writer is part of the log's name, at a fixed width
///
/// A home used to name a log on its own, and did so exactly while one leader
/// decided every write into a range. A range that admits two writers has two
/// counters, and a position means nothing without the counter it came from — so
/// the log is named by the pair (see [`LogId`]).
///
/// The writer is written **always**, not only when a range has two of them.
/// Encoding it conditionally would leave two key shapes under one tag, told
/// apart by their length on the replication read path, and this store has
/// already met that choice and refused it: the migration that gave the log its
/// home rewrote the keys rather than read them through a second decoder,
/// because a choice made by length there is a choice made on every record
/// forever. An older log is therefore rewritten once at open, to
/// [`Writer::UNATTRIBUTED`].
///
/// # The home comes first, and that is what makes the log per-range
///
/// A position is a position *in a log*, and once two leaders allocate positions
/// from independent counters there is no longer one log to be in. The home —
/// the reach a record belongs to, decided by the partition function above this
/// layer — leads the key, so each home's entries are one contiguous range and a
/// scan resumes inside the log it names rather than inside whatever sorted
/// nearby.
///
/// Two homes may therefore hold the same sequence, and must: that is the point.
/// Nothing compares a position in one home against a position in another, and
/// a reader that did would be comparing two unrelated counters.
///
/// The sequence is written **ascending**, which is the opposite of the version
/// suffix on a record key. That asymmetry is deliberate and both halves of it are
/// correct for their reader: a snapshot read wants the newest version at or
/// before a point, so record versions sort newest-first; a log reader resumes at
/// a position and walks forward, so log entries sort oldest-first. Writing them
/// the same way would make one of the two scans run backwards through its own
/// data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct LogKey {
    /// The log this entry belongs to.
    pub log: LogId,
    /// The position of this entry in that log.
    pub sequence: Sequence,
}

impl LogKey {
    /// Address a log entry in one log.
    #[must_use]
    pub const fn new(log: LogId, sequence: Sequence) -> Self {
        Self { log, sequence }
    }

    /// The prefix shared by every log entry, whatever its log.
    ///
    /// Still answers what it answered before the home existed — the whole
    /// keyspace — because the callers that hold it are asking about the log as a
    /// keyspace rather than about one log. A scan over a single log uses
    /// [`Self::prefix_for`].
    #[must_use]
    pub fn prefix() -> Vec<u8> {
        vec![KeyKind::LogEntry.tag()]
    }

    /// The prefix shared by every entry of one log.
    ///
    /// Exact: [`REACH_LEN`] and [`NODE_ID_LEN`] are both fixed, so these bytes
    /// lead an entry's key exactly when the entry belongs to this log. Nothing
    /// else can sort into the range. That exactness is why the writer is written
    /// at a fixed width and always — a conditionally-present field would make
    /// this prefix cover *some* of another log as well, and the failure would be
    /// a scan that silently read a writer it was never asked about.
    #[must_use]
    pub fn prefix_for(log: LogId) -> Vec<u8> {
        let mut writer =
            KeyWriter::with_capacity(1usize.saturating_add(REACH_LEN).saturating_add(NODE_ID_LEN));
        writer.put_u8(KeyKind::LogEntry.tag());
        put_log(&mut writer, log);
        writer.finish()
    }

    /// The prefix shared by every log of one home, whichever writer holds it.
    ///
    /// The answer to *which logs does this range have* is asked of the store
    /// rather than reasoned from the type (Q-632), and this is the bound that
    /// asks it.
    #[must_use]
    pub fn prefix_for_home(home: Reach) -> Vec<u8> {
        let mut writer = KeyWriter::with_capacity(1usize.saturating_add(REACH_LEN));
        writer.put_u8(KeyKind::LogEntry.tag());
        put_reach(&mut writer, home);
        writer.finish()
    }
}

impl StoreKey for LogKey {
    type Value = crate::value::LogRecord;

    const KIND: KeyKind = KeyKind::LogEntry;

    fn encode(&self) -> Key {
        let mut writer =
            KeyWriter::with_capacity(REACH_LEN.saturating_add(NODE_ID_LEN).saturating_add(9));
        writer.put_u8(Self::KIND.tag());
        put_log(&mut writer, self.log);
        writer.put_u64(self.sequence.get());
        Key::from(writer.finish())
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut reader = KeyReader::new(Self::KIND, bytes);
        reader.expect_kind()?;
        let log = take_log(&mut reader)?;
        let sequence = Sequence::new(reader.take_u64()?);
        reader.finish()?;
        Ok(Self { log, sequence })
    }
}

#[cfg(test)]
mod tests;
