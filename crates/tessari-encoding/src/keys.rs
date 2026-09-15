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

use tessari_kv::{Key, Keyspace};
use tessari_types::{DatabaseId, NamespaceId, Reach, RecordId, Sequence, TableId};

use crate::error::Result;
use crate::kind::KeyKind;
use crate::log_id::{LogId, Writer};
use crate::node::{NODE_ID_LEN, NodeIdentity};
use crate::order::{KeyReader, KeyWriter};
use crate::record_id;
use crate::value::{FormatVersion, StoreValue};

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

/// Append a reach as [`REACH_LEN`] bytes.
///
/// The variant leads, so the three levels do not interleave and every home is
/// one contiguous range. The order *between* homes carries no meaning — they
/// are separate logs, and nothing compares a position in one against a position
/// in another — so contiguity is the whole requirement.
fn put_reach(writer: &mut KeyWriter, reach: Reach) {
    let (variant, namespace, database) = match reach {
        Reach::Store => (REACH_STORE, 0, 0),
        Reach::Namespace(namespace) => (REACH_NAMESPACE, namespace.get(), 0),
        Reach::Database(namespace, database) => (REACH_DATABASE, namespace.get(), database.get()),
    };
    writer.put_u8(variant).put_u32(namespace).put_u32(database);
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
fn take_reach(reader: &mut KeyReader<'_>) -> Result<Reach> {
    let offset = reader.position();
    let variant = reader.take_u8()?;
    let namespace = NamespaceId::new(reader.take_u32()?);
    let database = DatabaseId::new(reader.take_u32()?);
    match variant {
        REACH_STORE => Ok(Reach::Store),
        REACH_NAMESPACE => Ok(Reach::Namespace(namespace)),
        REACH_DATABASE => Ok(Reach::Database(namespace, database)),
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

/// Addresses the store's own on-disk format version.
///
/// A singleton, written at creation and read at open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FormatVersionKey;

impl StoreKey for FormatVersionKey {
    type Value = FormatVersion;

    const KIND: KeyKind = KeyKind::FormatVersion;

    fn encode(&self) -> Key {
        Key::from(vec![Self::KIND.tag()])
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut reader = KeyReader::new(Self::KIND, bytes);
        reader.expect_kind()?;
        reader.finish()?;
        Ok(Self)
    }
}

/// Addresses the log position whose effects are durably present in the state,
/// for one home's log.
///
/// ```text
/// <0x31> <home:9> <writer:16>
/// ```
///
/// Written in the same batch as the state it describes, which is what turns
/// recovery into a resumable replay instead of a guess.
///
/// One per log, because the position it records counts in that log and nowhere
/// else. A single store-wide value would be the counter two leaders both
/// allocate from, which is the thing the per-range log exists to stop — and one
/// per *home* would be that same counter again as soon as a home has two
/// writers.
///
/// This keyspace is also the register of which logs exist: a log exists exactly
/// when something has been written into it, which is exactly when its position
/// key exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppliedPositionKey {
    /// The log this position belongs to.
    pub log: LogId,
}

impl AppliedPositionKey {
    /// Address one log's applied position.
    #[must_use]
    pub const fn new(log: LogId) -> Self {
        Self { log }
    }

    /// The prefix shared by every applied position of one home.
    #[must_use]
    pub fn prefix_for_home(home: Reach) -> Vec<u8> {
        let mut writer = KeyWriter::with_capacity(1usize.saturating_add(REACH_LEN));
        writer.put_u8(KeyKind::AppliedPosition.tag());
        put_reach(&mut writer, home);
        writer.finish()
    }
}

impl StoreKey for AppliedPositionKey {
    type Value = Sequence;

    const KIND: KeyKind = KeyKind::AppliedPosition;

    fn encode(&self) -> Key {
        let mut writer =
            KeyWriter::with_capacity(1usize.saturating_add(REACH_LEN).saturating_add(NODE_ID_LEN));
        writer.put_u8(Self::KIND.tag());
        put_log(&mut writer, self.log);
        Key::from(writer.finish())
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut reader = KeyReader::new(Self::KIND, bytes);
        reader.expect_kind()?;
        let log = take_log(&mut reader)?;
        reader.finish()?;
        Ok(Self { log })
    }
}

/// Addresses the newest record version this store has written.
///
/// A singleton, advanced in the same batch as the versions it accounts for.
///
/// # Why this is not the applied position
///
/// It held the same number for as long as one leader decided every write, and
/// that is the only reason the two were ever one key. They answer different
/// questions. The applied position is the log's — a fact several nodes must
/// agree on, because a replica resumes at it and a divergence is detected by
/// comparing it. A record version is a fact about one store's own visible
/// history: it orders that store's records against each other and against the
/// snapshot a reader holds, and nobody else reads it.
///
/// Once two leaders allocate log positions from independent counters, one
/// number cannot be both. A transaction opened at a store-wide "5" would read
/// one range as of its fifth record and another as of its fifth — two unrelated
/// moments presented as one, with no error and plausible data (Q-614).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VersionPositionKey;

impl StoreKey for VersionPositionKey {
    type Value = Sequence;

    const KIND: KeyKind = KeyKind::VersionPosition;

    fn encode(&self) -> Key {
        Key::from(vec![Self::KIND.tag()])
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut reader = KeyReader::new(Self::KIND, bytes);
        reader.expect_kind()?;
        reader.finish()?;
        Ok(Self)
    }
}

/// Addresses the oldest sequence a read can still be answered at exactly.
///
/// Reclamation keeps, for each record, the newest version at or below the floor
/// it ran at, and removes what is strictly older. So a reader **at** that floor
/// still resolves correctly and a reader **below** it may not — it can find an
/// older value than it should, or none, and nothing anywhere reports that.
///
/// This is the only durable record of that boundary. Without it a historical
/// read is unfalsifiable: the store has no way to distinguish "this record did
/// not exist then" from "the version that said so has been removed".
///
/// A singleton, absent until the first pass removes something. Absent means
/// nothing has ever been reclaimed, which is the store's state until reclamation
/// is scheduled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReclaimFloorKey;

impl StoreKey for ReclaimFloorKey {
    type Value = Sequence;

    const KIND: KeyKind = KeyKind::ReclaimFloor;

    fn encode(&self) -> Key {
        Key::from(vec![Self::KIND.tag()])
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut reader = KeyReader::new(Self::KIND, bytes);
        reader.expect_kind()?;
        reader.finish()?;
        Ok(Self)
    }
}

/// Addresses this node's own identity.
///
/// A singleton, generated once when absent and read at every open. It is in
/// `META` and not in the log because a replica reaches its state by replaying
/// the log: an identity that travelled there would be inherited by whoever
/// restored a backup, and two processes would then claim to be the same node
/// (ADR-0018 §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NodeIdentityKey;

impl StoreKey for NodeIdentityKey {
    type Value = NodeIdentity;

    const KIND: KeyKind = KeyKind::NodeIdentity;

    fn encode(&self) -> Key {
        Key::from(vec![Self::KIND.tag()])
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut reader = KeyReader::new(Self::KIND, bytes);
        reader.expect_kind()?;
        reader.finish()?;
        Ok(Self)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::error::Error;

    fn key(id: RecordId, version: u64) -> RecordKey {
        RecordKey::new(
            NamespaceId::new(1),
            DatabaseId::new(2),
            TableId::new(3),
            id,
            Sequence::new(version),
        )
    }

    /// A log of `home` whose writer nobody named — the migrated shape.
    fn logged(home: Reach) -> LogId {
        LogId::unattributed(home)
    }

    /// A log of `home` held by a distinguishable writer.
    fn by(home: Reach, writer: u8) -> LogId {
        LogId::new(home, Writer::new([writer; NODE_ID_LEN]))
    }

    #[test]
    fn a_record_key_round_trips_every_id_variant() {
        let ids = [
            RecordId::Int(-42),
            RecordId::Int(i64::MAX),
            RecordId::from("user"),
            RecordId::from(""),
            RecordId::Uuid([0x5a; 16]),
            RecordId::Bytes(vec![0x00, 0xff, 0x00]),
        ];
        for id in ids {
            let original = key(id, 7);
            let encoded = original.encode();
            assert_eq!(RecordKey::decode(encoded.as_slice()).unwrap(), original);
        }
    }

    #[test]
    fn the_table_prefix_is_fixed_width_and_leads_every_record_key() {
        let prefix =
            RecordKey::table_prefix(NamespaceId::new(1), DatabaseId::new(2), TableId::new(3));
        assert_eq!(prefix.len(), TABLE_PREFIX_LEN);
        let encoded = key(RecordId::from("x"), 1).encode();
        assert!(encoded.as_slice().starts_with(&prefix));
    }

    #[test]
    fn every_version_of_a_record_shares_the_versions_prefix() {
        let id = RecordId::from("same");
        let prefix = RecordKey::versions_prefix(
            NamespaceId::new(1),
            DatabaseId::new(2),
            TableId::new(3),
            &id,
        );
        for version in [0, 1, u64::MAX] {
            let encoded = key(id.clone(), version).encode();
            assert!(encoded.as_slice().starts_with(&prefix));
            assert_eq!(encoded.len(), prefix.len().saturating_add(8));
        }
    }

    #[test]
    fn newer_versions_of_a_record_sort_first() {
        let older = key(RecordId::from("r"), 5).encode();
        let newer = key(RecordId::from("r"), 9).encode();
        assert!(newer.as_slice() < older.as_slice());
    }

    #[test]
    fn distinct_records_stay_ordered_despite_the_version_suffix() {
        // Without a terminator on the record id, "a" followed by a version whose
        // first byte exceeds 'b' would sort after "ab". The version chosen here
        // is the one that would trigger it.
        let short = key(RecordId::from("a"), !0x6200_0000_0000_0000_u64).encode();
        let long = key(RecordId::from("ab"), 0).encode();
        assert!(short.as_slice() < long.as_slice());
    }

    #[test]
    fn decoding_a_record_key_as_a_meta_key_is_refused() {
        let encoded = key(RecordId::Int(1), 1).encode();
        let error = FormatVersionKey::decode(encoded.as_slice()).unwrap_err();
        assert!(matches!(error, Error::UnexpectedKind { .. }));
    }

    #[test]
    fn the_format_version_key_is_one_byte_and_round_trips() {
        let format = FormatVersionKey.encode();
        assert_eq!(format.len(), 1);
        assert_eq!(
            FormatVersionKey::decode(format.as_slice()).unwrap(),
            FormatVersionKey
        );
        assert_ne!(
            format.as_slice(),
            AppliedPositionKey::new(logged(Reach::Store))
                .encode()
                .as_slice()
        );
    }

    #[test]
    fn each_log_has_its_own_applied_position() {
        // Not a singleton any more, and that is the whole of the per-range log:
        // a position counts in one log, so the record of how far it has been
        // applied is one per log. A single value would be the counter two
        // leaders both allocate from — and one per HOME would be that same
        // counter again the moment a home admits two writers, which is why the
        // last two entries here share a home and differ only by writer.
        let logs = [
            logged(Reach::Store),
            logged(Reach::Namespace(NamespaceId::new(1))),
            logged(Reach::Database(NamespaceId::new(1), DatabaseId::new(2))),
            logged(Reach::Database(NamespaceId::new(1), DatabaseId::new(3))),
            by(Reach::Database(NamespaceId::new(1), DatabaseId::new(3)), 1),
            by(Reach::Database(NamespaceId::new(1), DatabaseId::new(3)), 2),
        ];
        let mut seen = Vec::new();
        for log in logs {
            let key = AppliedPositionKey::new(log);
            let encoded = key.encode();
            assert_eq!(
                encoded.len(),
                26,
                "the kind tag, the fixed reach and the fixed writer"
            );
            assert_eq!(AppliedPositionKey::decode(encoded.as_slice()).unwrap(), key);
            assert!(!seen.contains(&encoded), "two logs share a position key");
            seen.push(encoded);
        }
    }

    #[test]
    fn a_homes_applied_position_prefix_covers_its_writers_and_no_other_home() {
        let home = Reach::Database(NamespaceId::new(1), DatabaseId::new(2));
        let prefix = AppliedPositionKey::prefix_for_home(home);
        assert_eq!(prefix.len(), 10, "the kind tag plus the fixed reach");
        for writer in [0, 1, 255] {
            let mine = AppliedPositionKey::new(by(home, writer)).encode();
            assert!(mine.as_slice().starts_with(&prefix));
        }
        let sibling = AppliedPositionKey::new(by(
            Reach::Database(NamespaceId::new(1), DatabaseId::new(3)),
            1,
        ))
        .encode();
        assert!(!sibling.as_slice().starts_with(&prefix));
    }

    #[test]
    fn a_singleton_key_with_trailing_bytes_is_refused() {
        let bytes = [KeyKind::FormatVersion.tag(), 0x00];
        assert!(matches!(
            FormatVersionKey::decode(&bytes).unwrap_err(),
            Error::TrailingBytes { extra: 1, .. }
        ));
    }

    #[test]
    fn each_key_type_reports_its_keyspace() {
        assert_eq!(RecordKey::keyspace(), Keyspace::DATA);
        assert_eq!(FormatVersionKey::keyspace(), Keyspace::META);
        assert_eq!(AppliedPositionKey::keyspace(), Keyspace::META);
        assert_eq!(LogKey::keyspace(), Keyspace::LOG);
    }

    #[test]
    fn log_entries_sort_oldest_first_which_is_the_opposite_of_record_versions() {
        let older = LogKey::new(logged(Reach::Store), Sequence::new(5)).encode();
        let newer = LogKey::new(logged(Reach::Store), Sequence::new(9)).encode();
        assert!(
            older.as_slice() < newer.as_slice(),
            "a log reader resumes at a position and walks forward"
        );

        // The same two sequences, as versions of one record, sort the other way.
        let older_version = key(RecordId::from("r"), 5).encode();
        let newer_version = key(RecordId::from("r"), 9).encode();
        assert!(newer_version.as_slice() < older_version.as_slice());
    }

    #[test]
    fn a_log_key_round_trips_and_is_fixed_width() {
        let homes = [
            Reach::Store,
            Reach::Namespace(NamespaceId::new(3)),
            Reach::Database(NamespaceId::new(3), DatabaseId::new(4)),
        ];
        for home in homes {
            for sequence in [0, 1, u64::MAX] {
                let original = LogKey::new(by(home, 9), Sequence::new(sequence));
                let encoded = original.encode();
                assert_eq!(
                    encoded.len(),
                    34,
                    "tag, nine reach bytes, sixteen writer bytes, eight sequence"
                );
                assert_eq!(LogKey::decode(encoded.as_slice()).unwrap(), original);
            }
        }
    }

    #[test]
    fn two_homes_hold_the_same_position_without_colliding() {
        // The whole of what the per-range log buys: two leaders allocate from
        // independent counters, so the same number arrives twice and must land
        // in two places. Before the home was part of the key these two were one
        // key, and the second write silently replaced the first.
        let position = Sequence::new(7);
        let one = LogKey::new(
            logged(Reach::Database(NamespaceId::new(1), DatabaseId::new(2))),
            position,
        );
        let other = LogKey::new(
            logged(Reach::Database(NamespaceId::new(1), DatabaseId::new(3))),
            position,
        );
        assert_ne!(one.encode(), other.encode());
        assert_eq!(LogKey::decode(one.encode().as_slice()).unwrap(), one);
        assert_eq!(LogKey::decode(other.encode().as_slice()).unwrap(), other);
    }

    #[test]
    fn every_log_key_carries_the_log_prefix() {
        let prefix = LogKey::prefix();
        assert_eq!(prefix.len(), 1);
        for sequence in [0, 42, u64::MAX] {
            let encoded = LogKey::new(logged(Reach::Store), Sequence::new(sequence)).encode();
            assert!(encoded.as_slice().starts_with(&prefix));
        }
    }

    #[test]
    fn a_logs_prefix_leads_its_own_entries_and_no_others() {
        let home = Reach::Database(NamespaceId::new(1), DatabaseId::new(2));
        let prefix = LogKey::prefix_for(logged(home));
        assert_eq!(
            prefix.len(),
            26,
            "the kind tag, the fixed reach and the fixed writer"
        );
        for sequence in [0, 42, u64::MAX] {
            let mine = LogKey::new(logged(home), Sequence::new(sequence)).encode();
            assert!(mine.as_slice().starts_with(&prefix));
        }
        // The neighbours a scan over that prefix must not reach: the namespace
        // above it, a sibling database, and the store.
        let strangers = [
            logged(Reach::Namespace(NamespaceId::new(1))),
            logged(Reach::Database(NamespaceId::new(1), DatabaseId::new(3))),
            logged(Reach::Store),
            // And the one the writer adds: the same home, another writer. This
            // is the neighbour a home-wide prefix would have swept in.
            by(home, 1),
        ];
        for stranger in strangers {
            let theirs = LogKey::new(stranger, Sequence::new(42)).encode();
            assert!(!theirs.as_slice().starts_with(&prefix));
        }
    }

    #[test]
    fn two_writers_in_one_home_hold_the_same_position_without_colliding() {
        // The per-range log stopped two RANGES sharing a counter. This is the
        // same failure one level in: two masters on ONE range allocate the same
        // number, and before the writer was part of the key the second write
        // replaced the first with nothing in an error state.
        let home = Reach::Database(NamespaceId::new(1), DatabaseId::new(2));
        let position = Sequence::new(7);
        let one = LogKey::new(by(home, 1), position);
        let other = LogKey::new(by(home, 2), position);
        assert_ne!(one.encode(), other.encode());
        assert_eq!(LogKey::decode(one.encode().as_slice()).unwrap(), one);
        assert_eq!(LogKey::decode(other.encode().as_slice()).unwrap(), other);
    }

    #[test]
    fn a_homes_prefix_covers_every_writer_of_that_home_and_no_other_home() {
        // Which logs a range has is asked of the store, and this is the bound
        // that asks it (Q-632).
        let home = Reach::Database(NamespaceId::new(1), DatabaseId::new(2));
        let prefix = LogKey::prefix_for_home(home);
        assert_eq!(prefix.len(), 10, "the kind tag plus the fixed reach");
        for writer in [0, 1, 255] {
            let mine = LogKey::new(by(home, writer), Sequence::new(3)).encode();
            assert!(mine.as_slice().starts_with(&prefix));
        }
        let sibling = LogKey::new(
            by(Reach::Database(NamespaceId::new(1), DatabaseId::new(3)), 1),
            Sequence::new(3),
        )
        .encode();
        assert!(!sibling.as_slice().starts_with(&prefix));
    }

    #[test]
    fn one_writers_entries_stay_contiguous_and_ascending() {
        // Two properties in one assertion because they are one requirement: a
        // log reader resumes at a position and walks forward, and it must not
        // walk into another writer's numbering on the way.
        let home = Reach::Namespace(NamespaceId::new(4));
        let mine_early = LogKey::new(by(home, 1), Sequence::new(1)).encode();
        let mine_late = LogKey::new(by(home, 1), Sequence::new(u64::MAX)).encode();
        let theirs_early = LogKey::new(by(home, 2), Sequence::new(1)).encode();
        assert!(mine_early.as_slice() < mine_late.as_slice());
        assert!(
            mine_late.as_slice() < theirs_early.as_slice(),
            "a writer's whole log sorts before the next writer's first entry"
        );
    }

    #[test]
    fn an_unknown_reach_variant_is_refused_rather_than_read_as_the_store() {
        // Widening it to the store would file a record this build cannot place
        // into the one log every subscriber reads.
        let mut bytes = LogKey::new(logged(Reach::Store), Sequence::new(1))
            .encode()
            .into_bytes();
        bytes[1] = 0x7f;
        assert!(matches!(
            LogKey::decode(&bytes).unwrap_err(),
            Error::UnknownReach { found: 0x7f, .. }
        ));
    }

    #[test]
    fn a_record_key_is_never_decodable_as_a_log_key() {
        let encoded = key(RecordId::Int(1), 1).encode();
        assert!(matches!(
            LogKey::decode(encoded.as_slice()).unwrap_err(),
            Error::UnexpectedKind { .. }
        ));
    }
}
