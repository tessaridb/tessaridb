//! The store's format version, and the sequence stored as a value.

use super::{HEADER_LEN, StoreValue, split_header, with_header};
use crate::error::{Error, Result};
use tessari_kv::Value;
use tessari_types::Sequence;

/// The store's own on-disk format version.
///
/// Independent of any storage engine's internal versioning: this one describes
/// the key grammar and the value codec, which are ours.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct FormatVersion(u32);

impl FormatVersion {
    /// The format this build writes.
    ///
    /// Moved to 2 when the log record gained its epoch (ADR-0059). A store
    /// already on version 1 is still opened and is not rewritten — its records
    /// read as the first leadership, which is what they are — so the bump buys
    /// one thing: a store *created* by this build is refused by an older one at
    /// `open`, rather than at whichever read first meets a flag bit it does not
    /// know.
    ///
    /// Moved to 3 when the log became per-range and its keys gained a home. A
    /// store below 3 **is** rewritten at open, which is the difference from the
    /// bump before it: version 2 changed what a value means, and an old value
    /// still decoded; version 3 changed the shape of a key, and an old key does
    /// not decode at all. Keeping two key decoders live would put the choice
    /// between them on the replication read path forever, for a format nothing
    /// has released.
    ///
    /// Moved to 4 when a stored version gained its causal stamp. This is the
    /// version-2 shape and not the version-3 one: the bump changes what a value
    /// *means* while every value already written still decodes, because a clear
    /// stamp bit reads as an empty stamp. So a store below 4 is opened and is
    /// **not** rewritten, and the bump buys the same single thing the first one
    /// did — a store created by this build is refused by an older one at `open`
    /// rather than at whichever read first meets a flag bit it does not know.
    ///
    /// Moved to 5 when a log key gained its writer (G027 S2.2). Unlike the three
    /// bumps before it, a store below 5 **is** rewritten at open: the writer is
    /// a fixed-width field in the key rather than a flag bit in a value, so an
    /// old key and a new one differ in length, and telling two key shapes apart
    /// by their length on the replication read path is a choice made on every
    /// record forever rather than once. `give_an_older_log_its_home` already
    /// refused that trade for the home; this is the same refusal for the writer.
    ///
    /// Moved to 6 when a containment index's entries arrived under their own key
    /// kind, `0x44` (ADR-0116 D4). The version-2 shape again: no key or value
    /// already written changes, so a store below 6 is opened and **not**
    /// rewritten, and the bump buys the one thing — a build that does not know
    /// the kind refuses a store this build created at `open`, rather than reading
    /// the catalog without its `contains` flag and serving an equality from
    /// entries it cannot read.
    ///
    /// Moved to 7 when a table could declare that its records expire (ADR-0122
    /// A8). The version-2 shape once more: an expiring version has been readable
    /// by every build since the key-value verbs arrived, so nothing on disk
    /// changes. What an older build cannot do is **write** such a table by its
    /// rules — it would clear a record's instant on a plain write and stamp no
    /// default — so the declaration waits for this format, and once a store
    /// holds it no older build opens it at all.
    pub const CURRENT: Self = Self(7);

    /// The first format whose tables may declare that their records expire
    /// (ADR-0122 A8). Named for the reason [`Self::HOMED_LOG`] is.
    pub const TABLE_EXPIRY: Self = Self(7);

    /// The first format whose rollups may keep a sketch (ADR-0122 C5): an
    /// older build reading the declaration would refuse a fold it does not
    /// know, and could not keep the state beside the row. Format 7 was not yet
    /// released when this joined it, so one bump carries both.
    pub const SKETCH_ROLLUP: Self = Self(7);

    /// The first format whose stores may hold a containment index's entries
    /// (ADR-0116 D4).
    ///
    /// A store below it is opened **as is** and keeps its stamp — so a build
    /// before it can still open a store this one merely read — and is stamped
    /// here when its first containment index is built, the moment a build that
    /// cannot read those entries must start refusing it. Named for the reason
    /// [`Self::HOMED_LOG`] is.
    pub const CONTAINMENT_INDEX: Self = Self(6);

    /// The first format whose log keys carry a home.
    ///
    /// Named rather than written as a literal at the one place that compares
    /// against it: a later bump moves `CURRENT` and must leave this where it is,
    /// and a literal `3` sitting in the storage layer would move with whichever
    /// of the two the next author happened to be reading.
    pub const HOMED_LOG: Self = Self(3);

    /// The first format whose log keys carry the writer that allocated them.
    ///
    /// Named for the reason [`Self::HOMED_LOG`] is named.
    pub const WRITER_QUALIFIED_LOG: Self = Self(5);

    /// The first release that writes this format: what every peer must run
    /// before a store is finalized to it (ADR-0118 D3).
    ///
    /// `None` for a format no release has been recorded against. Every format
    /// up to 5 is held by `0.22.0-beta`, the oldest release whose stores a newer
    /// build promises to open; a new format adds its row when `CURRENT` moves,
    /// and a test holds `CURRENT` to having one.
    #[must_use]
    pub const fn first_written_by(self) -> Option<crate::NodeVersion> {
        let (major, minor) = match self.0 {
            1..=5 => (0, 22),
            6 => (0, 28),
            7 => (0, 33),
            _ => return None,
        };
        Some(crate::NodeVersion {
            major,
            minor,
            patch: 0,
        })
    }

    /// Wrap a raw format version.
    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// The raw format version.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// Refuse a store written by a newer build.
    ///
    /// Opening forward-compatibly would write this build's format into a store
    /// that already holds a newer one, which is not recoverable afterwards.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnsupportedFormatVersion`] when the store is newer than
    /// this build.
    pub fn check_supported(self) -> Result<()> {
        if self.0 > Self::CURRENT.0 {
            return Err(Error::UnsupportedFormatVersion {
                found: self.0,
                supported: Self::CURRENT.0,
            });
        }
        Ok(())
    }
}

impl StoreValue for FormatVersion {
    fn encode(&self) -> Value {
        let mut buffer = with_header(0, 4);
        buffer.extend_from_slice(&self.0.to_be_bytes());
        Value::from(buffer)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let (_, payload) = split_header(bytes, 0)?;
        let raw: [u8; 4] = payload.try_into().map_err(|_| Error::ValueTruncated {
            len: bytes.len(),
            needed: HEADER_LEN.saturating_add(4),
        })?;
        Ok(Self(u32::from_be_bytes(raw)))
    }
}

impl StoreValue for Sequence {
    fn encode(&self) -> Value {
        let mut buffer = with_header(0, 8);
        buffer.extend_from_slice(&self.get().to_be_bytes());
        Value::from(buffer)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let (_, payload) = split_header(bytes, 0)?;
        let raw: [u8; 8] = payload.try_into().map_err(|_| Error::ValueTruncated {
            len: bytes.len(),
            needed: HEADER_LEN.saturating_add(8),
        })?;
        Ok(Self::new(u64::from_be_bytes(raw)))
    }
}
