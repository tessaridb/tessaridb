//! Keys for the facts a store holds once: format, positions, retention, served reach and identity.

use super::{REACH_LEN, StoreKey, put_log, put_reach, take_log, take_reach};
use crate::error::Result;
use crate::kind::KeyKind;
use crate::log_id::LogId;
use crate::node::{NODE_ID_LEN, NodeIdentity};
use crate::order::{KeyReader, KeyWriter};
use crate::value::{FormatVersion, StoreValue};
use tessari_kv::Key;
use tessari_types::{Reach, Sequence};

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
/// The oldest sequence one log still holds.
///
/// Absent means nothing has ever been pruned from that log, which is every log
/// until a retention policy runs — so absence is *the log is whole*, not *the log
/// is empty*.
///
/// # Why it is not derived from the first surviving key
///
/// A scan for the lowest key of one log would answer the same number while the
/// log has records in it, and would answer nothing at all once it has none —
/// which is exactly the state a fully-pruned log is in, and exactly when a reader
/// most needs to be told that its position is below the horizon rather than that
/// the log does not exist. A recorded start says *this log begins here* whether
/// or not anything is left in it.
///
/// It is also the only durable evidence that pruning happened. A start lost on
/// restart makes every refusal below it wrong in the dangerous direction: the
/// store would go back to answering reads for records it no longer holds, which
/// is a gap reported as a divergence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogStartKey {
    /// The log this start belongs to.
    pub log: LogId,
}

impl LogStartKey {
    /// Address one log's start.
    #[must_use]
    pub const fn new(log: LogId) -> Self {
        Self { log }
    }
}

impl StoreKey for LogStartKey {
    type Value = Sequence;

    const KIND: KeyKind = KeyKind::LogStart;

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

/// How many log records this node keeps, when it keeps a bounded number.
///
/// Absent means **unbounded**, which is what every store held before retention
/// existed and what every store holds until an operator says otherwise. Absence
/// is therefore the setting that changes nothing, which is the only safe default
/// for an irreversible operation.
///
/// A singleton, like [`NodeIdentityKey`]: the number is about this node's disk.
/// A retention that travelled in the log would be inherited by whoever restored a
/// backup, and a follower would silently adopt its leader's disk budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogRetentionKey;

impl StoreKey for LogRetentionKey {
    type Value = Sequence;

    const KIND: KeyKind = KeyKind::LogRetention;

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

/// The reach this node's upstream last served it under (G031, ADR-0081).
///
/// Absent on a node that has never been served — a leader, a store standing
/// alone, a follower that has not yet collected — and absent means *holds
/// everything it has*, which is what every node was before shards existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServedReachKey;

impl StoreKey for ServedReachKey {
    type Value = ServedReach;

    const KIND: KeyKind = KeyKind::ServedReach;

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

/// The value under [`ServedReachKey`]: one reach, in the form a log key holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServedReach(pub Reach);

impl StoreValue for ServedReach {
    fn encode(&self) -> tessari_kv::Value {
        let mut writer = KeyWriter::with_capacity(REACH_LEN.saturating_add(8));
        put_reach(&mut writer, self.0);
        let payload = writer.finish();
        let mut buffer = crate::value::with_header(0, payload.len());
        buffer.extend_from_slice(&payload);
        tessari_kv::Value::from(buffer)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let (_, payload) = crate::value::split_header(bytes, 0)?;
        let mut reader = KeyReader::new(KeyKind::ServedReach, payload);
        let reach = take_reach(&mut reader)?;
        reader.finish()?;
        Ok(Self(reach))
    }
}

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
