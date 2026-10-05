//! Names for what the store holds by id: tables, their kinds, and their tenancy.

use super::*;

impl Db {
    /// The names of the tables an answer's record references point at.
    ///
    /// # Why an answer cannot be rendered without this
    ///
    /// A record reference holds a table **id** and a record id, because that is
    /// what the key grammar stores and what a reference has to be to survive a
    /// rename. The name the language writes — `users:1` — lives in the catalog.
    /// So a renderer handed a `Value` alone cannot produce a reference anybody
    /// can use: the console would print `1:2` and a JSON client would receive
    /// `"1:2"`, which is indistinguishable from a value it could follow and is
    /// not one.
    ///
    /// # Why it is here rather than in each surface
    ///
    /// Because there are two of them. The console and the HTTP endpoint both
    /// need this, and two implementations of "what is this table called" would
    /// eventually disagree about a renamed table — with one surface answering
    /// the old name and the other the new, which is worse than neither
    /// answering.
    ///
    /// # Why it walks the answer first
    ///
    /// The catalog is only read when the answer actually holds a reference, so a
    /// read of records that carry none costs a walk over values already in
    /// memory rather than a catalog scan. Most answers hold none.
    ///
    /// # Errors
    ///
    /// Returns an error when the catalog cannot be read.
    pub fn names_in(&self, records: &[(RecordId, Value)]) -> Result<BTreeMap<TableId, String>> {
        let mut wanted = BTreeSet::new();
        for (_, held) in records {
            tessari_types::json::referenced_tables(held, &mut wanted);
        }
        let mut named = BTreeMap::new();
        if wanted.is_empty() {
            return Ok(named);
        }
        let mut transaction = self.store.begin()?;
        let catalog = Catalog::new(&mut transaction);
        for table in wanted {
            // A reference to a table that has been dropped keeps its id and
            // gains no name, the same way a reference to a deleted record keeps
            // its id: the field still says what it says.
            if let Some(held) = catalog.table(table)? {
                named.insert(table, held.name);
            }
        }
        Ok(named)
    }

    /// What a table is called, for a caller holding only its id.
    ///
    /// The other direction of [`Db::names_in`], and here for the same reason: a
    /// change on the feed names its table by id, and an id means nothing to
    /// anybody outside this process. A table that has been dropped has no name
    /// and keeps its id, the way a reference to a deleted record does.
    ///
    /// # Errors
    ///
    /// Returns an error when the catalog cannot be read.
    pub fn table_name(&self, table: TableId) -> Result<Option<String>> {
        let mut transaction = self.store.begin()?;
        Ok(Catalog::new(&mut transaction)
            .table(table)?
            .map(|held| held.name))
    }

    /// Whether `namespace.database.table` names a series — what the HTTP
    /// append route asks before it writes a batch (G044 C12), so a batch for a
    /// table of another kind is refused rather than written.
    ///
    /// # Errors
    ///
    /// Returns an error when the catalog cannot be read.
    pub fn is_series(&self, namespace: &str, database: &str, table: &str) -> Result<bool> {
        let mut transaction = self.store.begin()?;
        let catalog = Catalog::new(&mut transaction);
        let Some(namespace) = catalog.namespace_id(namespace)? else {
            return Ok(false);
        };
        let Some(database) = catalog.database_id(namespace, database)? else {
            return Ok(false);
        };
        let Some(table) = catalog.table_id(namespace, database, table)? else {
            return Ok(false);
        };
        Ok(matches!(
            catalog.table(table)?.map(|held| held.kind),
            Some(tessari_storage::TableKind::Series(_))
        ))
    }

    /// Whether `namespace.database.table` names a space — what the HTTP
    /// key-value routes ask before they run (ADR-0090), so a route aimed at a
    /// table of another kind answers `404` rather than reading it as keys.
    ///
    /// # Errors
    ///
    /// Returns an error when the catalog cannot be read.
    pub fn is_space(&self, namespace: &str, database: &str, table: &str) -> Result<bool> {
        let mut transaction = self.store.begin()?;
        let catalog = Catalog::new(&mut transaction);
        let Some(namespace) = catalog.namespace_id(namespace)? else {
            return Ok(false);
        };
        let Some(database) = catalog.database_id(namespace, database)? else {
            return Ok(false);
        };
        let Some(table) = catalog.table_id(namespace, database, table)? else {
            return Ok(false);
        };
        Ok(matches!(
            catalog.table(table)?.map(|held| held.kind),
            Some(tessari_storage::TableKind::Space(_))
        ))
    }

    /// Resolve the namespace and database a session has selected.
    ///
    /// A caller that reads the log needs these, because the log is every
    /// tenancy's and a reader confined to one has to know which ids that is.
    ///
    /// # Errors
    ///
    /// Returns an error when the catalog cannot be read. A name that is not
    /// there is `None`: not existing is an answer.
    pub fn tenancy_in(
        &self,
        namespace: &str,
        database: &str,
    ) -> Result<Option<(NamespaceId, DatabaseId)>> {
        let mut transaction = self.store.begin()?;
        let catalog = Catalog::new(&mut transaction);
        let Some(namespace) = catalog.namespace_id(namespace)? else {
            return Ok(None);
        };
        Ok(catalog
            .database_id(namespace, database)?
            .map(|database| (namespace, database)))
    }

    /// Resolve a table by the three names a session selects it with.
    ///
    /// One method rather than three, because a caller that resolved a namespace
    /// and a database itself would be holding two ids it has no other use for,
    /// and every step of the walk is the same catalog read.
    ///
    /// # Errors
    ///
    /// Returns an error when the catalog cannot be read. A name that is not
    /// there is `None` rather than an error: not existing is an answer.
    pub fn table_in(
        &self,
        namespace: &str,
        database: &str,
        table: &str,
    ) -> Result<Option<TableId>> {
        let mut transaction = self.store.begin()?;
        let catalog = Catalog::new(&mut transaction);
        let Some(namespace) = catalog.namespace_id(namespace)? else {
            return Ok(None);
        };
        let Some(database) = catalog.database_id(namespace, database)? else {
            return Ok(None);
        };
        Ok(catalog.table_id(namespace, database, table)?)
    }

    /// The split tables in one database, each with its shards (G031, ADR-0080).
    ///
    /// What a reader of the database's log has to know before it starts: those
    /// tables' single-shard writes are in their shards' logs.
    ///
    /// # Errors
    ///
    /// Returns an error when the catalog cannot be read.
    pub(crate) fn split_tables_in(
        &self,
        namespace: NamespaceId,
        database: DatabaseId,
    ) -> Result<Vec<(String, TableId, Vec<ShardId>)>> {
        let mut transaction = self.store.begin()?;
        Ok(Catalog::new(&mut transaction)
            .tables_in(namespace, database)?
            .into_iter()
            .filter_map(|table| {
                // Every log that may hold its records — a retired shard's
                // included, since a split stops writes to it and not reads.
                let shards = table.shards?.logs().collect();
                Some((table.name, table.id, shards))
            })
            .collect())
    }
}
