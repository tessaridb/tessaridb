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
pub(crate) mod definition;
mod edge_kind;
mod field;
mod grant;
mod graph;
mod leadership;
mod replica;
pub(crate) mod system;
mod user;
mod vault;

use tessari_encoding::{decode_payload, encode_payload};
use tessari_types::{
    DatabaseId, FieldKind, IndexId, NamespaceId, Path, RecordId, Replication, TableId, Value,
};

pub use analyzer::AnalyzerDefinition;
pub use authority::{Authority, Held, Kind, Reach};
pub(crate) use carried::carried_to;
pub(crate) use change::{CatalogChange, catalog_change, defined_index};
pub use consumer::{ConsumerDefinition, Mapped, OnFailure};
pub use definition::{
    CLAIMED_BY_CONSUMER, CLAIMED_BY_INSTANCE, DatabaseDefinition, EdgeDeclaration, EdgeOrder,
    GEO_FIELD, IndexDefinition, IndexShape, NamespaceDefinition, QUEUE_ATTEMPTS, QUEUE_CLAIMED_BY,
    QUEUE_CLAIMED_UNTIL, QueueDeclaration, RECORD_LEVEL, SeriesDeclaration, StoredKind,
    TableDefinition, TableKind, TableShape, VECTOR_FIELD, VaultDeclaration, VectorDeclaration,
    VectorDistance, ViewDeclaration,
};
pub use edge_kind::EdgeKindDefinition;
pub use field::{FieldDefinition, FieldShape};
pub use grant::GrantDefinition;
pub use graph::GraphDefinition;
pub use leadership::LeadershipDefinition;
pub(crate) use leadership::covering;
pub use replica::{ReplicaDefinition, names_a_peer};
pub use system::{SYSTEM_DATABASE, SYSTEM_NAMESPACE};
pub use user::{Role, UserDefinition, Verb};
pub use vault::VaultRoot;

use crate::error::{Error, Result};
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

    /// Create a namespace.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NameTaken`] when the name is already in use, and a
    /// substrate or decoding failure otherwise.
    pub fn create_namespace(&mut self, name: &str) -> Result<NamespaceDefinition> {
        let qualified = qualify(Level::Namespace, &[], name);
        self.reserve_name(&qualified)?;
        let id = NamespaceId::new(self.allocate(Level::Namespace)?);
        let definition = NamespaceDefinition {
            id,
            name: name.to_owned(),
            // A namespace is created having said nothing about replication, and
            // the clause is applied by [`Self::set_replication`] whether it
            // arrived with the `DEFINE` or with a later `ALTER`. One write path
            // rather than two: the two statements set the same field, and a
            // second route for the creating case is a route that can disagree
            // with the altering one. Both run inside the caller's transaction,
            // whose pending writes are keyed by address, so a definition
            // written and then amended still reaches the log as one mutation.
            replication: None,
        };
        self.write(system::NAMESPACES, id.get(), &definition.to_value());
        self.claim_name(&qualified, id.get());
        Ok(definition)
    }

    /// Set how many copies of a namespace the cluster is asked to keep.
    ///
    /// Moves between **stated** values in both directions (owner requirement
    /// D12) and never back to never-stated: a namespace that was once asked has
    /// been asked, and silence is a fact about its history rather than a
    /// setting to restore.
    ///
    /// Nothing is redistributed here, and nothing needs to be. The log already
    /// holds every write the namespace ever took, so a follower that begins
    /// subscribing replays it, and this statement has nothing to do but record
    /// the policy. See `Session::alter_namespace` for why that is a property of
    /// the design rather than a step left out.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoSuchParent`] when the namespace does not exist, and a
    /// substrate or decoding failure otherwise.
    pub fn set_replication(
        &mut self,
        namespace: NamespaceId,
        replication: Replication,
    ) -> Result<NamespaceDefinition> {
        let Some(mut definition) = self.namespace(namespace)? else {
            return Err(Error::NoSuchParent {
                entity: "namespace",
                id: namespace.get(),
            });
        };
        definition.replication = Some(replication);
        self.write(system::NAMESPACES, namespace.get(), &definition.to_value());
        Ok(definition)
    }

    /// Create a database inside an existing namespace.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoSuchParent`] when the namespace does not exist,
    /// [`Error::NameTaken`] when the name is in use within it.
    pub fn create_database(
        &mut self,
        namespace: NamespaceId,
        name: &str,
    ) -> Result<DatabaseDefinition> {
        if self.namespace(namespace)?.is_none() {
            return Err(Error::NoSuchParent {
                entity: "namespace",
                id: namespace.get(),
            });
        }
        let qualified = qualify(Level::Database, &[namespace.get()], name);
        self.reserve_name(&qualified)?;
        let id = DatabaseId::new(self.allocate(Level::Database)?);
        let definition = DatabaseDefinition {
            id,
            namespace,
            name: name.to_owned(),
        };
        self.write(system::DATABASES, id.get(), &definition.to_value());
        self.claim_name(&qualified, id.get());
        Ok(definition)
    }

    /// Create a table inside an existing database.
    ///
    /// The shape is fixed at creation except for `schemafull`, which
    /// [`Self::set_schemafull`] rewrites in place. That one moves because a
    /// schema is a rule about what may be *written*, so changing it binds the
    /// writes that follow and leaves the stored rows alone; the `kind` does not
    /// move, because it describes what the records already **are**.
    ///
    /// An edge table additionally gets an index on `out` and one on `in`, in
    /// this same commit, so that traversal is an index read without the caller
    /// having had to know to declare them.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoSuchParent`] when the database does not exist or does
    /// not belong to `namespace`, and [`Error::NameTaken`] when the name is in
    /// use within it.
    pub fn create_table(
        &mut self,
        namespace: NamespaceId,
        database: DatabaseId,
        name: &str,
        shape: TableShape,
    ) -> Result<TableDefinition> {
        let parent = self.database(database)?;
        if parent.is_none_or(|found| found.namespace != namespace) {
            return Err(Error::NoSuchParent {
                entity: "database",
                id: database.get(),
            });
        }
        let qualified = qualify(Level::Table, &[namespace.get(), database.get()], name);
        self.reserve_name(&qualified)?;
        let id = TableId::new(self.allocate(Level::Table)?);
        let definition = TableDefinition {
            id,
            namespace,
            database,
            name: name.to_owned(),
            schemafull: shape.schemafull,
            kind: shape.kind,
            identity: shape.identity,
            graph: shape.graph,
        };
        self.write(system::TABLES, id.get(), &definition.to_value());
        self.claim_name(&qualified, id.get());
        // Learned here rather than on the first read, so a table declared in
        // this process never costs a catalog round trip to recognise. Recorded
        // before the commit, which is deliberate and harmless: a rolled-back
        // creation leaves an entry for a table id nothing can address, and ids
        // are never reused.
        self.transaction
            .store()
            .series()
            .learn(id, &definition.kind);
        if matches!(definition.kind, TableKind::Edge(_)) {
            // Every edge table gets the endpoint machinery, declared pair or
            // not: what the pair adds is a refusal at the write and an order on
            // the key, not a different way of being reachable.
            //
            // Each endpoint gets both an index and a declaration. The index is
            // what makes traversal a range read; the declaration is what lets an
            // edge table also be `SCHEMAFULL`, since nobody writes `out` and `in`
            // by hand and a caller should not have to declare fields the store
            // itself fills in.
            for endpoint in [EDGE_OUT, EDGE_IN] {
                self.create_index(
                    id,
                    &format!("{endpoint}_edges"),
                    vec![Path::field(endpoint)],
                    IndexShape::default(),
                )?;
                self.create_field(id, endpoint, FieldKind::Record, FieldShape::default())?;
            }
        }
        if matches!(definition.kind, TableKind::Bucket(_)) {
            // The companion table the bytes live in. Its name carries a byte an
            // identifier cannot hold, so no statement can name it — the same
            // mechanism the catalog itself uses to be unreachable rather than
            // merely undocumented, and the reason `SELECT * FROM media` answers
            // with files and never with chunks (ADR-0011 §2).
            self.create_table(
                namespace,
                database,
                &Self::chunks_named(name),
                TableShape::default(),
            )?;
        }
        Ok(definition)
    }

    /// The name of the table a bucket's chunks live in.
    ///
    /// Derived rather than stored: the name carries the fact, so a second field
    /// in the catalog holding the same id would be a fact that can disagree with
    /// itself. The `\u{1}` is what makes it unnameable — an identifier is
    /// letters, digits and underscores, so nothing a caller can write reaches it.
    #[must_use]
    pub fn chunks_named(bucket: &str) -> String {
        format!("{bucket}\u{1}chunks")
    }

    /// The name of the table an edge kind's edges live in.
    ///
    /// Derived rather than stored, and unnameable for the same reason a bucket's
    /// chunk table is: an identifier is letters, digits and underscores, so the
    /// `\u{1}` puts it out of reach of anything a caller can write. An edge kind
    /// is not a table in the language, and this is what keeps that true while
    /// still letting an edge be an ordinary record mutation — which is what
    /// carries it, and the adjacency derived from it, to every replica.
    #[must_use]
    pub fn edges_named(kind: &str) -> String {
        format!("{kind}\u{1}edges")
    }

    /// Create an index on an existing table.
    ///
    /// The projection list is the index's identity as much as its name is: an
    /// index on `(a, b)` answers a query about `a` and one on `(b, a)` does not,
    /// so the order given here is the order values are encoded in. Each entry is
    /// a path, so an index may project a value nested inside the record.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoSuchParent`] when the table does not exist,
    /// [`Error::EmptyIndex`] when no field is named, and [`Error::NameTaken`]
    /// when the name is in use on that table.
    pub fn create_index(
        &mut self,
        table: TableId,
        name: &str,
        fields: Vec<Path>,
        shape: IndexShape,
    ) -> Result<IndexDefinition> {
        if fields.is_empty() {
            return Err(Error::EmptyIndex {
                name: name.to_owned(),
            });
        }
        let Some(parent) = self.table(table)? else {
            return Err(Error::NoSuchParent {
                entity: "table",
                id: table.get(),
            });
        };
        let qualified = qualify(
            Level::Index,
            &[
                parent.namespace.get(),
                parent.database.get(),
                parent.id.get(),
            ],
            name,
        );
        self.reserve_name(&qualified)?;
        let id = IndexId::new(self.allocate(Level::Index)?);
        let definition = IndexDefinition {
            id,
            namespace: parent.namespace,
            database: parent.database,
            table,
            name: name.to_owned(),
            fields,
            unique: shape.unique,
            search: shape.search,
            vector: shape.vector,
            spatial: shape.spatial,
        };
        self.write(system::INDEXES, id.get(), &definition.to_value());
        self.claim_name(&qualified, id.get());
        Ok(definition)
    }

    /// Look an index up by id.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored definition cannot be read.
    pub fn index(&self, id: IndexId) -> Result<Option<IndexDefinition>> {
        self.read(system::INDEXES, id.get())?
            .as_ref()
            .map(IndexDefinition::from_value)
            .transpose()
    }

    /// Every index on one table.
    ///
    /// This is what a write path consults, so it reads the whole index catalog
    /// and filters. That is honest at catalog scale and would not be at table
    /// scale; when the write path is called hot, the answer is a cache keyed by
    /// the catalog's own version, not a cleverer scan.
    ///
    /// # Errors
    ///
    /// Returns an error when a stored definition cannot be read.
    pub fn indexes_on(&self, table: TableId) -> Result<Vec<IndexDefinition>> {
        let mut found = Vec::new();
        for (_, bytes) in self.transaction.scan_table(
            system::SYSTEM_NAMESPACE,
            system::SYSTEM_DATABASE,
            system::INDEXES,
        )? {
            let definition = IndexDefinition::from_value(&decode_payload(&bytes)?)?;
            if definition.table == table {
                found.push(definition);
            }
        }
        Ok(found)
    }

    /// Write an index's definition again, unchanged, so its entries are built
    /// from the table's rows as they now stand.
    ///
    /// The whole of `REBUILD INDEX`. A catalog entry is an ordinary record
    /// (ADR-0009), so writing this one puts a mutation in the log that index
    /// maintenance already knows how to answer — by building every entry the
    /// definition implies. Nothing about the definition changes, and nothing
    /// needs to: the *rows* changed, and the entries are a function of them.
    ///
    /// Two properties come from doing it this way rather than with a command of
    /// its own. Every replica rebuilds at the same sequence, because each one
    /// applies the same record. And the rebuild is atomic with whatever else the
    /// transaction does, because it is the same batch.
    ///
    pub fn rebuild_index(&mut self, definition: &IndexDefinition) {
        self.write(system::INDEXES, definition.id.get(), &definition.to_value());
    }

    /// Drop an index's definition and release its name.
    ///
    /// The entries themselves are **not** removed here, for the same reason a
    /// dropped table keeps its records: it is bulk work whose cost belongs at
    /// the call site rather than hidden inside a catalog call.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored definition cannot be read.
    pub fn drop_index(&mut self, id: IndexId) -> Result<bool> {
        let Some(definition) = self.index(id)? else {
            return Ok(false);
        };
        let qualified = qualify(
            Level::Index,
            &[
                definition.namespace.get(),
                definition.database.get(),
                definition.table.get(),
            ],
            &definition.name,
        );
        self.transaction.delete(system::address(
            system::INDEXES,
            RecordId::Int(id_key(id.get())),
        ));
        self.transaction
            .delete(system::address(system::NAMES, RecordId::from(qualified)));
        Ok(true)
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
        self.read(system::TABLES, id.get())?
            .as_ref()
            .map(TableDefinition::from_value)
            .transpose()
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

    /// Drop a table's definition and release its name.
    ///
    /// The table's records are **not** removed here — that is a bulk operation
    /// with its own cost and its own decisions, and doing it silently inside a
    /// catalog call would hide it. [`Self::drop_database`] and
    /// [`Self::drop_namespace`] take the same stance one and two levels up:
    /// each removes its own definition and nothing beneath it, and whether
    /// anything is still down there is a question the statement asks, where the
    /// span to refuse with lives.
    ///
    /// The id is not released. A reused id would let a stale key or an in-flight
    /// reference resolve against a different table, and nothing in the store
    /// could detect it.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored definition cannot be read.
    pub fn drop_table(&mut self, id: TableId) -> Result<bool> {
        let Some(definition) = self.table(id)? else {
            return Ok(false);
        };
        self.transaction.store().series().forget(id);
        let qualified = qualify(
            Level::Table,
            &[definition.namespace.get(), definition.database.get()],
            &definition.name,
        );
        self.transaction.delete(system::address(
            system::TABLES,
            RecordId::Int(id_key(id.get())),
        ));
        self.transaction
            .delete(system::address(system::NAMES, RecordId::from(qualified)));
        Ok(true)
    }

    /// Remove a database's definition and release its name.
    ///
    /// Answers `false` when there was nothing under that id.
    ///
    /// Nothing inside is touched, for the reason [`Self::drop_table`] leaves the
    /// records: cascading here would be unbounded work hidden inside a catalog
    /// call. Whether the database still holds anything is asked by the
    /// statement, which has the span to say so with.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored definition cannot be read.
    pub fn drop_database(&mut self, id: DatabaseId) -> Result<bool> {
        let Some(definition) = self.database(id)? else {
            return Ok(false);
        };
        let qualified = qualify(
            Level::Database,
            &[definition.namespace.get()],
            &definition.name,
        );
        self.transaction.delete(system::address(
            system::DATABASES,
            RecordId::Int(id_key(id.get())),
        ));
        self.transaction
            .delete(system::address(system::NAMES, RecordId::from(qualified)));
        Ok(true)
    }

    /// Remove a namespace's definition and release its name.
    ///
    /// Answers `false` when there was nothing under that id. Nothing inside is
    /// touched — see [`Self::drop_database`].
    ///
    /// # Errors
    ///
    /// Returns an error when the stored definition cannot be read.
    pub fn drop_namespace(&mut self, id: NamespaceId) -> Result<bool> {
        let Some(definition) = self.namespace(id)? else {
            return Ok(false);
        };
        let qualified = qualify(Level::Namespace, &[], &definition.name);
        self.transaction.delete(system::address(
            system::NAMESPACES,
            RecordId::Int(id_key(id.get())),
        ));
        self.transaction
            .delete(system::address(system::NAMES, RecordId::from(qualified)));
        Ok(true)
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

    /// Hand out the next id at `level`, and record that it was handed out.
    ///
    /// The counter is written in this transaction, so two concurrent creations
    /// write the same record and one of them loses — the same mechanism that
    /// keeps names unique, and no second lock.
    fn allocate(&mut self, level: Level) -> Result<u32> {
        let address = system::address(system::ALLOCATORS, RecordId::from(level.counter()));
        let next = match self.transaction.get(&address)? {
            Some(bytes) => definition::id_of(&decode_payload(&bytes)?, "allocator", "next")?,
            None => system::FIRST_ID,
        };
        let following = next.checked_add(1).ok_or(Error::IdSpaceExhausted {
            level: level.counter(),
        })?;
        self.transaction.put(
            address,
            encode_payload(&definition::number(following)).into_bytes(),
        );
        Ok(next)
    }

    /// Hand out the next identity for a record in `table`, and record that it
    /// was handed out.
    ///
    /// Written in the caller's transaction for the reason [`Self::allocate`]
    /// gives: two writers that each read the same counter also both write it,
    /// and that shared key is what makes one of them lose. So no number reaches
    /// two records, and no second lock is needed to say so.
    ///
    /// The counter is a catalog record like every other, so it rides the log,
    /// takes the snapshot and reaches a replica — which is the whole point. A
    /// counter a replica derived for itself, or one restored from a backup taken
    /// before the writes it counts, would re-issue an identity that already
    /// names a record, and the next write under it would replace that record
    /// rather than add one, with nothing anywhere in an error state.
    ///
    /// The number answered always fits an `i64`, so the caller can build a
    /// [`RecordId::Int`] from it without a second refusal to invent.
    ///
    /// # Errors
    ///
    /// Returns [`Error::IdSpaceExhausted`] when the table has spent every
    /// identity the key grammar can express, and a substrate or decoding
    /// failure otherwise.
    pub fn next_record_number(&mut self, table: TableId) -> Result<u64> {
        let address = system::address(system::RECORD_SEQUENCES, RecordId::Int(id_key(table.get())));
        let next = match self.transaction.get(&address)? {
            Some(bytes) => {
                definition::count_of(&decode_payload(&bytes)?, "record sequence", "next")?
            }
            None => system::FIRST_RECORD_NUMBER,
        };
        let following = next.checked_add(1).ok_or(Error::IdSpaceExhausted {
            level: definition::RECORD_LEVEL,
        })?;
        // Stored before it is answered, so a count this store could hold but
        // could never spend is refused while the caller still has no identity to
        // do anything with.
        let held = definition::count(following)?;
        self.transaction
            .put(address, encode_payload(&held).into_bytes());
        Ok(next)
    }

    /// How many records a table holds, when the store has a count for it.
    ///
    /// `None` means no estimate rather than an empty table: the counter is
    /// written by [`crate::cardinality`] when a record arrives or leaves, so a
    /// table nothing has written since the store was created has no record
    /// here. The two are worth telling apart because a planner told "no
    /// estimate" must fall back to the behaviour it had before counts existed,
    /// while a planner told "zero" would conclude that every index beats a scan
    /// of nothing.
    ///
    /// It is an **estimate for choosing an access path** and never an answer.
    /// Nothing that decides which records a statement returns may read it.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot be read or the stored count
    /// cannot be decoded.
    pub fn record_count(&mut self, table: TableId) -> Result<Option<u64>> {
        let address = system::address(system::RECORD_COUNTS, RecordId::Int(id_key(table.get())));
        let Some(bytes) = self.transaction.get(&address)? else {
            return Ok(None);
        };
        Ok(Some(definition::count_of(
            &decode_payload(&bytes)?,
            "record count",
            "held",
        )?))
    }

    /// Refuse early if the name is already resolvable.
    fn reserve_name(&self, qualified: &str) -> Result<()> {
        if self.resolve(qualified)?.is_some() {
            return Err(Error::NameTaken {
                qualified: qualified.to_owned(),
            });
        }
        Ok(())
    }

    fn claim_name(&mut self, qualified: &str, id: u32) {
        let address = system::address(system::NAMES, RecordId::from(qualified));
        let value = definition::number(id);
        self.transaction
            .put(address, encode_payload(&value).into_bytes());
    }

    fn resolve(&self, qualified: &str) -> Result<Option<u32>> {
        let address = system::address(system::NAMES, RecordId::from(qualified));
        let Some(bytes) = self.transaction.get(&address)? else {
            return Ok(None);
        };
        definition::id_of(&decode_payload(&bytes)?, "name", "id").map(Some)
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
