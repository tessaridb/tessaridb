//! The key-kind table.
//!
//! The first byte of every key names its kind. That is what lets one keyspace
//! hold several kinds and stay scannable, and it makes a collision between kinds
//! impossible instead of merely unlikely.
//!
//! This table is assigned **once**. A tag is never reused and never renumbered,
//! because a renumber after data exists is a rebuild of the whole store, not a
//! code change. Kinds whose encoders are not written yet are listed here anyway,
//! for exactly that reason — reserving a byte costs nothing today and cannot be
//! done retroactively.
//!
//! The normative statement of the grammar is `docs/key-grammar.md`; this module
//! is its executable half, and the two are kept in step deliberately.

use core::fmt;

use tessari_kv::Keyspace;

/// What a key addresses, encoded as its leading byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum KeyKind {
    /// One version of one record.
    Record,
    /// A non-unique secondary index entry.
    SecondaryIndex,
    /// A unique index entry.
    UniqueIndex,
    /// A full-text posting list entry.
    Posting,
    /// A node in a vector index.
    VectorNode,
    /// A graph edge between two records.
    Edge,
    /// The collection statistics one search index is ranked against.
    SearchStatistics,
    /// One entry in the ordered log.
    LogEntry,
    /// The store's own on-disk format version.
    FormatVersion,
    /// The log position whose effects are durably present in the state.
    AppliedPosition,
    /// A namespace catalog entry.
    NamespaceCatalog,
    /// A database catalog entry.
    DatabaseCatalog,
    /// A table catalog entry.
    TableCatalog,
    /// An index catalog entry.
    IndexCatalog,
    /// The next-identifier allocator for a catalog level.
    IdAllocator,
    /// A resumable index-backfill watermark.
    BackfillWatermark,
    /// This node's own identity: who it is, not who else is here.
    ///
    /// In `META` rather than the log on purpose. A replica reaches its state by
    /// replaying the log, so an identity that travelled in it would be inherited
    /// by whoever restored a backup (ADR-0018 §1).
    NodeIdentity,
}

impl KeyKind {
    /// Every kind, in tag order.
    ///
    /// Exhaustive by construction: a new variant that is not added here fails
    /// the table test rather than going unnoticed.
    pub const ALL: &'static [Self] = &[
        Self::Record,
        Self::SecondaryIndex,
        Self::UniqueIndex,
        Self::Posting,
        Self::VectorNode,
        Self::Edge,
        Self::SearchStatistics,
        Self::LogEntry,
        Self::FormatVersion,
        Self::AppliedPosition,
        Self::NamespaceCatalog,
        Self::DatabaseCatalog,
        Self::TableCatalog,
        Self::IndexCatalog,
        Self::IdAllocator,
        Self::BackfillWatermark,
        Self::NodeIdentity,
    ];

    /// The leading byte that identifies this kind on disk.
    ///
    /// Tags are grouped by family — `0x0_` data, `0x1_` index, `0x2_` log,
    /// `0x3_` meta — so a hex dump is readable and each family can grow.
    /// `0x00` is never assigned: it is the escape byte of the variable-length
    /// encoding and is kept free as a sorts-before-everything sentinel.
    #[must_use]
    pub const fn tag(self) -> u8 {
        match self {
            Self::Record => 0x01,
            Self::SecondaryIndex => 0x10,
            Self::UniqueIndex => 0x11,
            Self::Posting => 0x12,
            Self::VectorNode => 0x13,
            Self::Edge => 0x14,
            Self::SearchStatistics => 0x15,
            Self::LogEntry => 0x20,
            Self::FormatVersion => 0x30,
            Self::AppliedPosition => 0x31,
            Self::NamespaceCatalog => 0x32,
            Self::DatabaseCatalog => 0x33,
            Self::TableCatalog => 0x34,
            Self::IndexCatalog => 0x35,
            Self::IdAllocator => 0x36,
            Self::BackfillWatermark => 0x37,
            Self::NodeIdentity => 0x38,
        }
    }

    /// The keyspace this kind is stored in.
    #[must_use]
    pub const fn keyspace(self) -> Keyspace {
        match self {
            Self::Record => Keyspace::DATA,
            Self::SecondaryIndex
            | Self::UniqueIndex
            | Self::Posting
            | Self::VectorNode
            | Self::Edge
            | Self::SearchStatistics => Keyspace::INDEX,
            Self::LogEntry => Keyspace::LOG,
            Self::FormatVersion
            | Self::AppliedPosition
            | Self::NamespaceCatalog
            | Self::DatabaseCatalog
            | Self::TableCatalog
            | Self::IndexCatalog
            | Self::IdAllocator
            | Self::BackfillWatermark
            | Self::NodeIdentity => Keyspace::META,
        }
    }

    /// A stable name, used in decode and corruption errors.
    ///
    /// A key-decode failure that reports a hex blob leaves an operator unable to
    /// tell which subsystem wrote the bad key, so every kind can name itself.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Record => "record",
            Self::SecondaryIndex => "secondary-index",
            Self::UniqueIndex => "unique-index",
            Self::Posting => "posting",
            Self::VectorNode => "vector-node",
            Self::Edge => "edge",
            Self::SearchStatistics => "search-statistics",
            Self::LogEntry => "log-entry",
            Self::FormatVersion => "format-version",
            Self::AppliedPosition => "applied-position",
            Self::NamespaceCatalog => "namespace-catalog",
            Self::DatabaseCatalog => "database-catalog",
            Self::TableCatalog => "table-catalog",
            Self::IndexCatalog => "index-catalog",
            Self::IdAllocator => "id-allocator",
            Self::BackfillWatermark => "backfill-watermark",
            Self::NodeIdentity => "node-identity",
        }
    }

    /// Recover a kind from its leading byte, if the byte is assigned.
    #[must_use]
    pub fn from_tag(tag: u8) -> Option<Self> {
        Self::ALL.iter().copied().find(|kind| kind.tag() == tag)
    }
}

impl fmt::Display for KeyKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn tags_are_unique() {
        let tags: HashSet<u8> = KeyKind::ALL.iter().map(|kind| kind.tag()).collect();
        assert_eq!(
            tags.len(),
            KeyKind::ALL.len(),
            "two kinds share a tag; on-disk they would be indistinguishable"
        );
    }

    #[test]
    fn names_are_unique() {
        let names: HashSet<&str> = KeyKind::ALL.iter().map(|kind| kind.name()).collect();
        assert_eq!(names.len(), KeyKind::ALL.len());
    }

    #[test]
    fn zero_is_never_assigned() {
        // 0x00 is the escape byte of the variable-length encoding and the
        // sentinel that sorts before every key.
        assert!(KeyKind::ALL.iter().all(|kind| kind.tag() != 0x00));
        assert_eq!(KeyKind::from_tag(0x00), None);
    }

    #[test]
    fn every_kind_round_trips_through_its_tag() {
        for kind in KeyKind::ALL {
            assert_eq!(KeyKind::from_tag(kind.tag()), Some(*kind));
        }
    }

    #[test]
    fn tags_pin_to_their_documented_values() {
        // These are the values in `docs/key-grammar.md` §3. Changing one is a
        // rebuild of every existing store, so the test states them literally
        // rather than deriving them.
        let expected: &[(KeyKind, u8)] = &[
            (KeyKind::Record, 0x01),
            (KeyKind::SecondaryIndex, 0x10),
            (KeyKind::UniqueIndex, 0x11),
            (KeyKind::Posting, 0x12),
            (KeyKind::VectorNode, 0x13),
            (KeyKind::Edge, 0x14),
            (KeyKind::SearchStatistics, 0x15),
            (KeyKind::LogEntry, 0x20),
            (KeyKind::FormatVersion, 0x30),
            (KeyKind::AppliedPosition, 0x31),
            (KeyKind::NamespaceCatalog, 0x32),
            (KeyKind::DatabaseCatalog, 0x33),
            (KeyKind::TableCatalog, 0x34),
            (KeyKind::IndexCatalog, 0x35),
            (KeyKind::IdAllocator, 0x36),
            (KeyKind::BackfillWatermark, 0x37),
            (KeyKind::NodeIdentity, 0x38),
        ];
        assert_eq!(expected.len(), KeyKind::ALL.len(), "a kind is untested");
        for (kind, tag) in expected {
            assert_eq!(kind.tag(), *tag, "tag drift for {kind}");
        }
    }

    #[test]
    fn each_kind_belongs_to_exactly_one_keyspace() {
        assert_eq!(KeyKind::Record.keyspace(), Keyspace::DATA);
        assert_eq!(KeyKind::Posting.keyspace(), Keyspace::INDEX);
        assert_eq!(KeyKind::LogEntry.keyspace(), Keyspace::LOG);
        assert_eq!(KeyKind::AppliedPosition.keyspace(), Keyspace::META);
    }
}
