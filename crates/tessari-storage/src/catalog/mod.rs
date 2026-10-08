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
mod format;
mod grant;
mod graph;
mod group;
mod indexes;
mod leadership;
mod params;
mod position;
mod qualified;
mod replica;
mod revocation;
mod rows;
mod shard;
mod space;
mod splitting;
pub(crate) mod system;
mod tombstone;
mod topic;
mod user;
mod vault;
mod words;

use tessari_encoding::{decode_payload, encode_payload};
use tessari_types::{DatabaseId, NamespaceId, RecordId, TableId, Value};

pub use analyzer::AnalyzerDefinition;
pub use authority::{Authority, Held, Kind, Reach};
pub(crate) use carried::{carried_to, home_of};
pub(crate) use change::{CatalogChange, catalog_change, defined_index};
pub use consumer::{ConsumerDefinition, Feed, Mapped, OnFailure};
pub(crate) use decoded::DecodedTables;
pub use definition::{
    AutoSplit, CLAIMED_BY_CONSUMER, CLAIMED_BY_INSTANCE, DatabaseDefinition, EdgeDeclaration,
    EdgeOrder, EngineField, EngineMember, EventDeclaration, GEO_FIELD, IndexDefinition, IndexShape,
    NamespaceDefinition, QUEUE_ATTEMPTS, QUEUE_CLAIMED_BY, QUEUE_CLAIMED_UNTIL, QueueDeclaration,
    RECORD_LEVEL, RollupCompute, RollupDeclaration, RollupFold, SearchCosts, SeriesDeclaration,
    StoredKind, TableDefinition, TableExpiry, TableKind, TableShape, UNIT_WEIGHT, VECTOR_FIELD,
    VaultCustody, VaultDeclaration, VectorDeclaration, VectorDistance, ViewDeclaration,
};
pub use edge_kind::EdgeKindDefinition;
pub use failover::{FailoverDefinition, FailoverStamp};
pub use field::{FieldDefinition, FieldShape};
pub(crate) use format::finalized_in;
pub use grant::GrantDefinition;
pub use graph::GraphDefinition;
pub use group::{GroupDeclaration, GroupState, InFlight};
pub use leadership::LeadershipDefinition;
pub(crate) use leadership::covering;
pub use leadership::governing;
pub use replica::{
    Greeter, JoinTicket, ReplicaDefinition, another_node_may_write, names_a_peer,
    the_row_a_greeting_binds,
};
pub(crate) use rows::CatalogRows;
pub use shard::{ShardMap, ShardSpan};
pub use space::{Eviction, SpaceDeclaration, SpaceLimit};
pub use system::{SYSTEM_DATABASE, SYSTEM_NAMESPACE};
pub use topic::{PublicAppend, TopicDeclaration};
pub use user::{Role, UserDefinition, Verb};
pub use vault::VaultRoot;
pub use words::{WordSet, WordSetKind};

use crate::error::{Error, Result};
use crate::transaction::{RecordAddress, Transaction};
use qualified::parse_qualified;
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

    /// Set or clear when the table's shards split and merge without being
    /// asked (ADR-0113 D2), keeping everything else about it.
    ///
    /// Answers `false` when there is no table under that id.
    ///
    /// # Errors
    ///
    /// [`Error::AutoSplitOnAnUnsplitTable`] for a table with no shard map,
    /// whose shards nothing could split; [`Error::AutoSplitWouldOscillate`]
    /// when a split's two halves would merge straight back.
    pub fn set_auto_split(&mut self, id: TableId, policy: Option<AutoSplit>) -> Result<bool> {
        let Some(mut definition) = self.table(id)? else {
            return Ok(false);
        };
        if let Some(policy) = policy {
            if definition.shards.is_none() {
                return Err(Error::AutoSplitOnAnUnsplitTable {
                    table: definition.name,
                });
            }
            // A shard just above the bound splits into halves of about half
            // of it each; if those two together were under the merge bound
            // the next pass would merge them again, and the table would
            // change shape every pass.
            if policy.merge_below.saturating_mul(2) >= policy.above {
                return Err(Error::AutoSplitWouldOscillate {
                    table: definition.name,
                    above: policy.above,
                    merge_below: policy.merge_below,
                });
            }
        }
        definition.auto_split = policy;
        self.write(system::TABLES, id.get(), &definition.to_value());
        Ok(true)
    }

    /// Set the table's expiry declaration (ADR-0122 A1, A6), keeping everything
    /// else about it. Changes the declaration and no stored record.
    ///
    /// Answers `false` when there is no table under that id.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored definition cannot be read.
    pub fn set_expiry(&mut self, id: TableId, expire: TableExpiry) -> Result<bool> {
        let Some(mut definition) = self.table(id)? else {
            return Ok(false);
        };
        definition.expire = Some(expire);
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

    /// Replace a table's events (ADR-0110), keeping everything else about it.
    ///
    /// Answers `false` when there is no table under that id.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored definition cannot be read.
    pub fn set_events(&mut self, id: TableId, events: Vec<EventDeclaration>) -> Result<bool> {
        let Some(mut definition) = self.table(id)? else {
            return Ok(false);
        };
        definition.events = events;
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
mod tests;
