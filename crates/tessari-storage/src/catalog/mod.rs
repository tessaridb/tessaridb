//! The catalog: which namespaces, databases and tables exist.
//!
//! Entries are ordinary records in a reserved system tenancy (ADR-0009), so a
//! catalog change rides the same log, takes the same snapshot and replicates
//! through the same apply path as any other write. Two consequences are worth
//! naming, because both would otherwise have to be built:
//!
//! - A transaction that creates a table and writes to it commits atomically, or
//!   neither part lands.
//! - A read at an old snapshot sees the schema **as of that snapshot**, so a
//!   long transaction never decodes its records against a definition written
//!   after it began.
//!
//! # Names are unique because a record says so
//!
//! Conflict detection is over what a transaction wrote, not what it read, so two
//! creations that each read an empty catalog and each write a different
//! definition would both commit. Both also write the qualified name into
//! [`system::NAMES`], and that shared key is what makes one of them lose. The
//! read below it is an early, friendlier refusal — not the enforcement.

mod analyzer;
mod authority;
mod carried;
mod change;
mod consumer;
// `pub(crate)` for the counter helpers: `crate::cardinality` stores a record
// count with the same `count`/`count_of` pair the record sequence uses, so the
// two per-table numbers are written and read one way rather than two.
mod allocation;
mod creating;
mod decoded;
pub(crate) mod definition;
mod dropping;
mod edge_kind;
mod failover;
mod field;
mod grant;
mod graph;
mod group;
mod indexes;
mod leadership;
mod position;
mod replica;
mod rows;
mod shard;
mod space;
mod splitting;
pub(crate) mod system;
mod topic;
mod user;
mod vault;

use tessari_encoding::{decode_payload, encode_payload};
use tessari_types::{DatabaseId, NamespaceId, RecordId, TableId, Value};

pub use analyzer::AnalyzerDefinition;
pub use authority::{Authority, Held, Kind, Reach};
pub(crate) use carried::{carried_to, home_of};
pub(crate) use change::{CatalogChange, catalog_change, defined_index};
pub use consumer::{ConsumerDefinition, Feed, Mapped, OnFailure};
pub(crate) use decoded::DecodedTables;
pub use definition::{
    CLAIMED_BY_CONSUMER, CLAIMED_BY_INSTANCE, DatabaseDefinition, EdgeDeclaration, EdgeOrder,
    GEO_FIELD, IndexDefinition, IndexShape, NamespaceDefinition, QUEUE_ATTEMPTS, QUEUE_CLAIMED_BY,
    QUEUE_CLAIMED_UNTIL, QueueDeclaration, RECORD_LEVEL, RollupCompute, RollupDeclaration,
    RollupFold, SearchCosts, SeriesDeclaration, StoredKind, TableDefinition, TableKind, TableShape,
    VECTOR_FIELD, VaultCustody, VaultDeclaration, VectorDeclaration, VectorDistance,
    ViewDeclaration,
};
pub use edge_kind::EdgeKindDefinition;
pub use failover::{FailoverDefinition, FailoverStamp};
pub use field::{FieldDefinition, FieldShape};
pub use grant::GrantDefinition;
pub use graph::GraphDefinition;
pub use group::{GroupDeclaration, GroupState, InFlight};
pub use leadership::LeadershipDefinition;
pub(crate) use leadership::covering;
pub use leadership::governing;
pub use replica::{
    ReplicaDefinition, another_node_may_write, names_a_peer, the_row_a_greeting_binds,
};
pub(crate) use rows::CatalogRows;
pub use shard::{ShardMap, ShardSpan};
pub use space::{Eviction, SpaceDeclaration, SpaceLimit};
pub use system::{SYSTEM_DATABASE, SYSTEM_NAMESPACE};
pub use topic::{PublicAppend, TopicDeclaration};
pub use user::{Role, UserDefinition, Verb};
pub use vault::VaultRoot;

use crate::error::Result;
use crate::transaction::{RecordAddress, Transaction};
use system::Level;

/// The field an edge's source endpoint is written to.
pub const EDGE_OUT: &str = "out";

/// The field an edge's target endpoint is written to.
pub const EDGE_IN: &str = "in";

/// Reads and writes the catalog through one transaction.
#[derive(Debug)]
pub struct Catalog<'a, 'txn> {
    transaction: &'a mut Transaction<'txn>,
}

impl<'a, 'txn> Catalog<'a, 'txn> {
    /// Work on the catalog inside `transaction`.
    pub fn new(transaction: &'a mut Transaction<'txn>) -> Self {
        Self { transaction }
    }

    /// Look a namespace up by id.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored definition cannot be read.
    pub fn namespace(&self, id: NamespaceId) -> Result<Option<NamespaceDefinition>> {
        self.read(system::NAMESPACES, id.get())?
            .as_ref()
            .map(NamespaceDefinition::from_value)
            .transpose()
    }

    /// Look a database up by id.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored definition cannot be read.
    pub fn database(&self, id: DatabaseId) -> Result<Option<DatabaseDefinition>> {
        self.read(system::DATABASES, id.get())?
            .as_ref()
            .map(DatabaseDefinition::from_value)
            .transpose()
    }

    /// Look a table up by id.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored definition cannot be read.
    pub fn table(&self, id: TableId) -> Result<Option<TableDefinition>> {
        // The row is read here, at this transaction's snapshot, as every catalog
        // read is — held between statements when the snapshot allows (`rows`) —
        // and decoding it is shared (`decoded`).
        let address: RecordAddress =
            system::address(system::TABLES, RecordId::Int(id_key(id.get())));
        let Some(stored) = self.row(&address)? else {
            return Ok(None);
        };
        self.transaction
            .store()
            .decoded_tables()
            .definition(&stored)
            .map(Some)
    }

    /// Every namespace that has been created.
    ///
    /// # The system tenancy is not in here, and not because it is filtered out
    ///
    /// Namespace zero holds the catalog itself and has **no definition record**:
    /// it was never created through [`Catalog::create_namespace`], so nothing
    /// wrote a row for it and this scan cannot produce one. That is the same
    /// property that makes it unaddressable by name — it is absent rather than
    /// hidden, and a listing that had to remember to exclude it would be one
    /// somebody could forget to.
    ///
    /// # Errors
    ///
    /// Returns an error when a stored definition cannot be read.
    pub fn namespaces(&self) -> Result<Vec<NamespaceDefinition>> {
        self.all(system::NAMESPACES, NamespaceDefinition::from_value)
    }

    /// Every database in one namespace.
    ///
    /// # Errors
    ///
    /// Returns an error when a stored definition cannot be read.
    pub fn databases_in(&self, namespace: NamespaceId) -> Result<Vec<DatabaseDefinition>> {
        Ok(self
            .all(system::DATABASES, DatabaseDefinition::from_value)?
            .into_iter()
            .filter(|found| found.namespace == namespace)
            .collect())
    }

    /// Every table in one database.
    ///
    /// # Errors
    ///
    /// Returns an error when a stored definition cannot be read.
    pub fn tables_in(
        &self,
        namespace: NamespaceId,
        database: DatabaseId,
    ) -> Result<Vec<TableDefinition>> {
        Ok(self
            .all(system::TABLES, TableDefinition::from_value)?
            .into_iter()
            .filter(|found| found.namespace == namespace && found.database == database)
            .collect())
    }

    /// Every definition in one system table, decoded.
    ///
    /// Reads the whole table and lets the caller filter, which is what
    /// [`Catalog::indexes_on`] does and is honest at catalog scale — the
    /// alternative is an index over the catalog, and the catalog is what indexes
    /// are declared in. If a caller ever needs this hot, the answer is a cache
    /// keyed by the catalog's own version rather than a cleverer scan.
    fn all<T>(&self, table: TableId, decode: fn(&Value) -> Result<T>) -> Result<Vec<T>> {
        let mut found = Vec::new();
        for (_, payload) in
            self.transaction
                .scan_table(system::SYSTEM_NAMESPACE, system::SYSTEM_DATABASE, table)?
        {
            found.push(decode(&decode_payload(&payload)?)?);
        }
        Ok(found)
    }

    /// Resolve a namespace name.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored entry cannot be read.
    pub fn namespace_id(&self, name: &str) -> Result<Option<NamespaceId>> {
        Ok(self
            .resolve(&qualify(Level::Namespace, &[], name))?
            .map(NamespaceId::new))
    }

    /// Resolve a database name within a namespace.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored entry cannot be read.
    pub fn database_id(&self, namespace: NamespaceId, name: &str) -> Result<Option<DatabaseId>> {
        Ok(self
            .resolve(&qualify(Level::Database, &[namespace.get()], name))?
            .map(DatabaseId::new))
    }

    /// Resolve a table name within a database.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored entry cannot be read.
    pub fn table_id(
        &self,
        namespace: NamespaceId,
        database: DatabaseId,
        name: &str,
    ) -> Result<Option<TableId>> {
        Ok(self
            .resolve(&qualify(
                Level::Table,
                &[namespace.get(), database.get()],
                name,
            ))?
            .map(TableId::new))
    }

    /// Rewrite a table's `schemafull` flag, leaving everything else as it is.
    ///
    /// Answers `false` when there was nothing under that id.
    ///
    /// The name is not touched, so the definition keeps its identity and every
    /// index, field and record already pointing at this id stays pointed at it.
    /// The rows are not visited: a schema is a rule about what may be written,
    /// and this writes the rule.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored definition cannot be read.
    pub fn set_schemafull(&mut self, id: TableId, schemafull: bool) -> Result<bool> {
        let Some(mut definition) = self.table(id)? else {
            return Ok(false);
        };
        definition.schemafull = schemafull;
        self.write(system::TABLES, id.get(), &definition.to_value());
        Ok(true)
    }

    /// Replace a series' declaration — its rollup list — keeping everything
    /// else about the table (ADR-0088 §6).
    ///
    /// Answers `false` when there is no table under that id. The per-process
    /// series registry is deliberately not told: it answers floors and time
    /// fields, which this cannot change, and a rollup list is read from here.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored definition cannot be read.
    pub fn set_series(&mut self, id: TableId, declared: SeriesDeclaration) -> Result<bool> {
        let Some(mut definition) = self.table(id)? else {
            return Ok(false);
        };
        definition.kind = TableKind::Series(declared);
        self.write(system::TABLES, id.get(), &definition.to_value());
        Ok(true)
    }

    /// Replace a vault's declaration — its custody after a passphrase change
    /// (ADR-0093 D3) — keeping everything else about the table.
    ///
    /// Answers `false` when there is no table under that id.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored definition cannot be read.
    pub fn set_vault(&mut self, id: TableId, declared: VaultDeclaration) -> Result<bool> {
        let Some(mut definition) = self.table(id)? else {
            return Ok(false);
        };
        definition.kind = TableKind::Vault(declared);
        self.write(system::TABLES, id.get(), &definition.to_value());
        Ok(true)
    }

    /// A name or table row at this transaction's snapshot, from the rows held
    /// between statements when the snapshot allows it (`rows`).
    fn row(&self, address: &RecordAddress) -> Result<Option<Vec<u8>>> {
        if !CatalogRows::holds(address) || self.transaction.has_written(address) {
            return self.transaction.get(address);
        }
        let store = self.transaction.store();
        let fill = match store
            .catalog_rows()
            .lookup(address, self.transaction.snapshot())
        {
            rows::Lookup::Held(row) => return Ok(row),
            rows::Lookup::Missing(fill) => fill,
        };
        let row = self.transaction.get(address)?;
        if let Some(generation) = fill
            && !store.write_gate().holding()
        {
            store
                .catalog_rows()
                .fill(address.clone(), generation, row.clone());
        }
        Ok(row)
    }

    /// The store's vault root record, when one has been created.
    ///
    /// `None` is a store that has never had a vault, which is every store until
    /// somebody unseals one for the first time.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when the record is present but does
    /// not decode. Refused rather than treated as absent: a store whose root
    /// record is unreadable still holds sealed records, and reporting "no vault
    /// here" would invite an operator to initialise a second one over the top
    /// and lose every secret behind the first.
    pub fn vault_root(&self) -> Result<Option<VaultRoot>> {
        match self.read(system::VAULT_ROOT, system::VAULT_ROOT_ID)? {
            Some(value) => VaultRoot::from_value(&value).map(Some),
            None => Ok(None),
        }
    }

    /// Write the store's vault root record.
    ///
    /// Called once, when the first vault is created. There is deliberately no
    /// path that replaces one: replacing the root record would leave every
    /// wrapped key in the store sealed under a master key nobody can recover,
    /// which reads as an empty vault rather than as an error.
    pub fn set_vault_root(&mut self, root: &VaultRoot) {
        self.write(system::VAULT_ROOT, system::VAULT_ROOT_ID, &root.to_value());
    }

    fn write(&mut self, table: TableId, id: u32, value: &Value) {
        self.transaction.put(
            system::address(table, RecordId::Int(id_key(id))),
            encode_payload(value).into_bytes(),
        );
    }

    fn read(&self, table: TableId, id: u32) -> Result<Option<Value>> {
        let address: RecordAddress = system::address(table, RecordId::Int(id_key(id)));
        let Some(bytes) = self.transaction.get(&address)? else {
            return Ok(None);
        };
        Ok(Some(decode_payload(&bytes)?))
    }
}

/// An identifier as a record id: widened, never cast.
fn id_key(id: u32) -> i64 {
    i64::from(id)
}

/// The string a name is unique against.
///
/// The level tag is what keeps the levels apart: without it a namespace named
/// `5/orders` and the database `orders` inside namespace `5` would qualify to
/// the same string, and one would be refused as a duplicate of something
/// unrelated. Parent ids are numeric, so a `/` inside a name can never be
/// mistaken for a separator that precedes it.
/// Read a qualified name back into the level and parent ids it was built from.
///
/// The inverse of [`qualify`], and it lives beside it for the reason
/// `Reach::of` and `Reach::parts` live beside each other: one format written in
/// two places drifts, and the copy that drifts is the one nobody is reading.
///
/// `None` for anything this build did not write — an unknown tag, a missing
/// separator, a parent that is not a number. A caller gets *cannot tell* rather
/// than a guess, because the caller asking is the replication filter and its
/// answer to *cannot tell* is to withhold.
///
/// The name itself is deliberately not returned. The one caller needs the
/// tenancy and nothing else, and handing back a borrowed name would invite a
/// second caller to compare strings the catalog compares by id.
fn parse_qualified(qualified: &str) -> Option<(Level, Vec<u32>)> {
    let (tag, rest) = qualified.split_once(':')?;
    let level = Level::from_tag(tag)?;
    let mut parents = Vec::new();
    let mut rest = rest;
    // A name may itself contain '/', which is why the parents are counted from
    // the left rather than split from the right: every parent is a number, and
    // the first segment that is not one is where the name begins.
    while let Some((head, tail)) = rest.split_once('/') {
        let Ok(parent) = head.parse::<u32>() else {
            break;
        };
        parents.push(parent);
        rest = tail;
    }
    Some((level, parents))
}

fn qualify(level: Level, parents: &[u32], name: &str) -> String {
    let mut qualified = String::from(level.tag());
    qualified.push(':');
    for parent in parents {
        qualified.push_str(&parent.to_string());
        qualified.push('/');
    }
    qualified.push_str(name);
    qualified
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// The two directions of one format, asserted against each other.
    ///
    /// Written as a round trip rather than against literals because a literal
    /// pins what somebody typed and a round trip pins what `qualify` produces —
    /// and the reader exists to read exactly that.
    #[test]
    fn a_qualified_name_reads_back_as_the_level_and_parents_it_was_built_from() {
        for (level, parents) in [
            (Level::Namespace, vec![]),
            (Level::Database, vec![7]),
            (Level::Table, vec![7, 3]),
            (Level::Index, vec![7, 3, 12]),
            (Level::Field, vec![7, 3, 12]),
            (Level::Graph, vec![7, 3]),
            (Level::EdgeKind, vec![7, 3]),
            (Level::Analyzer, vec![]),
            (Level::User, vec![]),
            (Level::Replica, vec![]),
            (Level::Consumer, vec![]),
        ] {
            let qualified = qualify(level, &parents, "orders");
            assert_eq!(
                parse_qualified(&qualified),
                Some((level, parents.clone())),
                "{qualified} must read back as what built it"
            );
        }
    }

    /// A name that itself begins with a number and a slash is the case the level
    /// tag was introduced for, and the reader must not mistake it for a parent
    /// it does not have. Over-reading is harmless because the true parents are
    /// always leftmost — but that is an argument, and this is the evidence.
    #[test]
    fn a_name_that_looks_like_a_parent_does_not_move_the_real_ones() {
        let qualified = qualify(Level::Table, &[7, 3], "9/orders");
        let (level, parents) = parse_qualified(&qualified).unwrap();
        assert_eq!(level, Level::Table);
        assert_eq!(parents.first(), Some(&7));
        assert_eq!(parents.get(1), Some(&3));
    }

    /// Anything this build did not write reads as *cannot tell*, never as a
    /// guess — the caller is the replication filter and its answer to that is to
    /// withhold.
    #[test]
    fn an_unknown_qualified_name_reads_as_cannot_tell() {
        assert_eq!(parse_qualified("orders"), None);
        assert_eq!(parse_qualified("zz:7/orders"), None);
    }

    #[test]
    fn the_level_tag_keeps_a_namespace_name_from_colliding_with_a_database_name() {
        let namespace = qualify(Level::Namespace, &[], "5/orders");
        let database = qualify(Level::Database, &[5], "orders");
        assert_ne!(namespace, database);
    }

    #[test]
    fn the_same_name_in_two_databases_qualifies_differently() {
        assert_ne!(
            qualify(Level::Table, &[1, 2], "users"),
            qualify(Level::Table, &[1, 3], "users")
        );
    }
}
