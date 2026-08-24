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

/// The first id handed out at any level. Zero belongs to the system.
pub const FIRST_ID: u32 = 1;

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
        }
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
        // from here until it was noticed while adding `REPLICAS`.
        let ids = [
            NAMESPACES, DATABASES, TABLES, NAMES, ALLOCATORS, INDEXES, FIELDS, ANALYZERS, USERS,
            GRANTS, REPLICAS,
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
        ];
        for (index, level) in levels.iter().enumerate() {
            for other in &levels[index.saturating_add(1)..] {
                assert_ne!(level.counter(), other.counter());
                assert_ne!(level.tag(), other.tag());
            }
        }
    }
}
