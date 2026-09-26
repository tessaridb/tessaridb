//! The reserved tenancy the catalog lives in.
//!
//! The catalog is records, not a keyspace of its own (ADR-0009), so it needs
//! somewhere to be. Namespace zero and database zero are reserved for it, and
//! user ids start at one.
//!
//! Nothing collides, and the reason is structural rather than lucky: a record
//! key carries all three identifiers, so the system table `(0, 0, 1)` and a user
//! table `(1, 1, 1)` are different keys even though both tables have id one.

use tessari_types::{DatabaseId, NamespaceId, RecordId, TableId};

use crate::transaction::RecordAddress;

/// The namespace the catalog itself lives in.
pub const SYSTEM_NAMESPACE: NamespaceId = NamespaceId::new(0);

/// The database the catalog itself lives in.
pub const SYSTEM_DATABASE: DatabaseId = DatabaseId::new(0);

/// Namespace definitions, keyed by namespace id.
pub const NAMESPACES: TableId = TableId::new(1);

/// Database definitions, keyed by database id.
pub const DATABASES: TableId = TableId::new(2);

/// Table definitions, keyed by table id.
pub const TABLES: TableId = TableId::new(3);

/// Qualified names, keyed by the name, holding the id it resolves to.
///
/// This table is what makes a name unique. Conflict detection is over what a
/// transaction wrote rather than what it read, so two creations that each read
/// an empty catalog and each write a *different* definition would both commit.
/// Both also write this one key, and that is what makes one of them lose.
pub const NAMES: TableId = TableId::new(4);

/// The id counters, keyed by the level they hand out ids for.
pub const ALLOCATORS: TableId = TableId::new(5);

/// Index definitions, keyed by index id.
pub const INDEXES: TableId = TableId::new(6);

/// Field definitions, keyed by field id.
pub const FIELDS: TableId = TableId::new(7);

/// Declared analyzers.
pub const ANALYZERS: TableId = TableId::new(8);

/// Declared users.
pub const USERS: TableId = TableId::new(9);

/// Which tables a user may reach, and for what.
pub const GRANTS: TableId = TableId::new(10);

/// The peers this store knows about, keyed by replica id.
///
/// A catalog record rather than a `META` key, and the distinction is the whole
/// of ADR-0018: who *else* is here must reach every node, so it travels in the
/// log; who *this node* is must not, so it does not (see `crate::node`).
pub const REPLICAS: TableId = TableId::new(11);

/// Declared stream consumers, keyed by consumer id.
///
/// The **declaration** only. Whether this process is running one, and where it
/// had reached when it last committed, are facts about this machine and live in
/// `META` beside the node's own identity — the same split ADR-0018 makes for a
/// replica, applied to the two halves of one object.
pub const CONSUMERS: TableId = TableId::new(12);

/// The next identity each table will give a record it is not given a name for,
/// keyed by the table id.
///
/// One counter per table rather than one per store: two tables numbering their
/// records independently is the point, and a shared counter would leave both of
/// them full of gaps for no reason anyone could read.
///
/// A catalog record rather than a `META` key, and for the same reason as
/// [`REPLICAS`]: this number must reach every node. A replica that derived its
/// own would re-issue an identity that already names a record on the leader,
/// after which the next write there replaces a record instead of adding one,
/// with nothing anywhere in an error state.
pub const RECORD_SEQUENCES: TableId = TableId::new(13);

/// Declared graphs.
pub const GRAPHS: TableId = TableId::new(14);

/// Declared edge kinds, keyed by edge-kind id.
///
/// A level of its own rather than a flag on a table, because an edge kind is not
/// a table: its entries are adjacency keys beside the node, not records behind an
/// index, so nothing about it fits the shape [`TABLES`] describes.
pub const EDGE_KINDS: TableId = TableId::new(15);

/// The store's vault root record: the salt and the master key sealed under the
/// operator's passphrase.
///
/// One record, at [`VAULT_ROOT_ID`]. Catalog state rather than a `META` key, on
/// the same reading as [`REPLICAS`]: it must reach every node and survive a
/// restore, because a follower promoted to leader that could not be unsealed
/// would hold every secret and open none of them.
///
/// Nothing in it is a secret. The salt is public by design and the wrapped key
/// is ciphertext under a key nobody has stored, so replicating it and backing it
/// up gives an attacker holding the backup an offline guessing problem against
/// Argon2id and nothing else.
pub const VAULT_ROOT: TableId = TableId::new(16);

/// Reads of a vault, one record per read.
///
/// Catalog state rather than a `META` key, and the reading is the same as
/// [`REPLICAS`]: an audit trail that stayed on the node that wrote it would be
/// lost with that node, and a follower promoted to leader would carry no record
/// of what the old one served. It travels in the log and survives a restore.
///
/// Nothing in it is a secret. It holds who asked, which record, which field
/// **names**, and whether the read was served — never a value, a fragment or a
/// length that discloses one.
pub const VAULT_AUDIT: TableId = TableId::new(17);

/// How many records each table holds, keyed by table id.
///
/// The planner reads this to decide whether an index is worth using, and there
/// was nothing to read before it. [`RECORD_SEQUENCES`] is the only other
/// per-table number the catalog keeps and it answers a different question: it
/// is an identity allocator, so it never decreases when a record is deleted and
/// it is never touched when the caller supplies its own id. A churned table
/// would read far too large under it and a table written with explicit ids
/// would read zero.
///
/// Catalog state rather than a `META` key, on the same reading as
/// [`RECORD_SEQUENCES`]: it is derived from the log record inside the commit,
/// so a replica replaying that record reaches the same number. A count derived
/// only on the leader would make a follower's planner choose a different access
/// path for the same query — the same records, by a slower route, with nothing
/// anywhere in an error state.
pub const RECORD_COUNTS: TableId = TableId::new(18);

/// Which node the log last showed leading a range, and under which leadership.
///
/// Reads like [`REPLICAS`] and is written for the opposite reason. A peer row is
/// an operator's statement of what a node *should* be; this is the winner's own
/// record of what it *became*, written at the moment a majority granted it and
/// ordered by the log like any other record — which is what lets a partitioned
/// node still answer *who leads this range* from what it had already applied.
pub const LEADERSHIPS: TableId = TableId::new(19);

/// The failover policy this cluster runs under, and the pair that orders it.
///
/// One row, because the periods are the cluster's and not a range's: how long a
/// leader's grant lasts and how often a node learns about its peers are the same
/// questions everywhere in one cluster, and a per-range answer would let two
/// halves of one election disagree about when it had timed out.
///
/// Here and not in a configuration file for the reason [`LEADERSHIPS`] is here:
/// a file is an unreplicated claim about a cluster-wide fact, so two nodes
/// holding different files is not a conflict anything detects. A row is a log
/// record, so it reaches every follower by the path every other record takes and
/// arrives ordered against the leadership that wrote it.
pub const FAILOVER: TableId = TableId::new(20);

/// Each topic reader's stored position (G037).
///
/// A row per topic and reader name, written by the reader's own `READ … FOR
/// CONSUMER` inside the reader's transaction — so the position moves exactly
/// when the reader's other writes commit, and two readers of one name write the
/// same row, which is what makes them take turns.
pub const TOPIC_POSITIONS: TableId = TableId::new(21);

/// The one record [`VAULT_ROOT`] holds.
pub const VAULT_ROOT_ID: u32 = 1;

/// The first id handed out at any level. Zero belongs to the system.
pub const FIRST_ID: u32 = 1;

/// The first identity a table gives a record it names itself.
///
/// One rather than zero, for the reason [`FIRST_ID`] is one: zero reads as
/// "unset" to everyone who has ever seen a counter, and a record legitimately
/// called `users:0` would spend the rest of its life being taken for one.
pub const FIRST_RECORD_NUMBER: u64 = 1;

/// Address a record in a system table.
#[must_use]
pub fn address(table: TableId, id: RecordId) -> RecordAddress {
    RecordAddress::new(SYSTEM_NAMESPACE, SYSTEM_DATABASE, table, id)
}

/// The levels that allocate ids, and the key each counter is stored under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Namespaces.
    Namespace,
    /// Databases.
    Database,
    /// Tables.
    Table,
    /// Indexes.
    Index,
    /// Declared fields on a table.
    Field,
    /// Declared analyzers.
    Analyzer,
    /// Declared users.
    User,
    /// Known peers.
    Replica,
    /// Declared stream consumers.
    Consumer,
    /// Declared graphs.
    Graph,
    /// Declared edge kinds.
    EdgeKind,
}

impl Level {
    /// The counter key for this level.
    #[must_use]
    pub const fn counter(self) -> &'static str {
        match self {
            Self::Namespace => "namespace",
            Self::Database => "database",
            Self::Table => "table",
            Self::Index => "index",
            Self::Field => "field",
            Self::Analyzer => "analyzer",
            Self::User => "user",
            Self::Replica => "replica",
            Self::Consumer => "consumer",
            Self::Graph => "graph",
            Self::EdgeKind => "edge-kind",
        }
    }

    /// The prefix a qualified name carries at this level.
    ///
    /// Without it a namespace named `5/orders` and the database `orders` inside
    /// namespace `5` would qualify to the same string, and one of the two would
    /// be refused as a duplicate of something it has nothing to do with.
    #[must_use]
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Namespace => "ns",
            Self::Database => "db",
            Self::Table => "tb",
            Self::Index => "ix",
            Self::Field => "fd",
            Self::Analyzer => "an",
            Self::User => "us",
            Self::Replica => "rp",
            Self::Consumer => "cs",
            Self::Graph => "gr",
            Self::EdgeKind => "ek",
        }
    }

    /// The level a tag names, the inverse of [`Self::tag`].
    ///
    /// Paired with it so the two directions cannot drift, which is the reason
    /// `Reach::of` and `Reach::parts` are written as a pair: a reader of a
    /// qualified name that rebuilt this mapping by hand would be a second place
    /// for one fact, and the one that drifts is the one nobody is reading.
    #[must_use]
    pub fn from_tag(tag: &str) -> Option<Self> {
        Some(match tag {
            "ns" => Self::Namespace,
            "db" => Self::Database,
            "tb" => Self::Table,
            "ix" => Self::Index,
            "fd" => Self::Field,
            "an" => Self::Analyzer,
            "us" => Self::User,
            "rp" => Self::Replica,
            "cs" => Self::Consumer,
            "gr" => Self::Graph,
            "ek" => Self::EdgeKind,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_system_tenancy_is_zero_so_user_ids_never_reach_it() {
        assert_eq!(SYSTEM_NAMESPACE.get(), 0);
        assert_eq!(SYSTEM_DATABASE.get(), 0);
        assert_ne!(FIRST_ID, SYSTEM_NAMESPACE.get());
    }

    #[test]
    fn every_system_table_has_a_distinct_id() {
        // Every system table, not a sample: an id is only proved distinct if it
        // is compared against all of them, and a name left out of this list
        // cannot be found duplicated however wrong it is. `GRANTS` was missing
        // from here until it was noticed while adding `REPLICAS`, and the two
        // vault tables were missing until `RECORD_COUNTS` was added — a list
        // three short of the constants it guards would have let a new id
        // collide with one of them and said nothing.
        let ids = [
            NAMESPACES,
            DATABASES,
            TABLES,
            NAMES,
            ALLOCATORS,
            INDEXES,
            FIELDS,
            ANALYZERS,
            USERS,
            GRANTS,
            REPLICAS,
            CONSUMERS,
            RECORD_SEQUENCES,
            GRAPHS,
            EDGE_KINDS,
            VAULT_ROOT,
            VAULT_AUDIT,
            RECORD_COUNTS,
            LEADERSHIPS,
        ];
        for (index, table) in ids.iter().enumerate() {
            assert!(
                !ids[index.saturating_add(1)..].contains(table),
                "{table} is used twice"
            );
        }
    }

    #[test]
    fn levels_have_distinct_counters_and_tags() {
        // Every level, not a sample: a new one is only proved distinct if it is
        // compared against all of them, and this list was short of three when
        // `Replica` was added.
        let levels = [
            Level::Namespace,
            Level::Database,
            Level::Table,
            Level::Index,
            Level::Field,
            Level::Analyzer,
            Level::User,
            Level::Replica,
            Level::Consumer,
            Level::Graph,
            Level::EdgeKind,
        ];
        for (index, level) in levels.iter().enumerate() {
            for other in &levels[index.saturating_add(1)..] {
                assert_ne!(level.counter(), other.counter());
                assert_ne!(level.tag(), other.tag());
            }
        }
    }
}
