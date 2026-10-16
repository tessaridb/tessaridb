//! What one parent holds — a namespace's databases, a database's tables, a
//! table's indexes and fields — found through the names its children claim
//! rather than by reading every definition in the store.
//!
//! Each of them claims `<tag>:<parent ids>/<name>` in `NAMES` when it is
//! declared and releases it when it is dropped, in the same transaction as its
//! definition (`db:<ns>/`, `tb:<ns>/<db>/`, `ix:` and `fd:<ns>/<db>/<table>/`).
//! So one parent's children are one span of `NAMES`, and finding them costs that
//! parent's own definitions. Reading the whole definition table instead cost
//! every definition in every namespace, the ones left behind by dropped tables
//! and the deleted versions still retained — once per table a commit wrote and
//! once per database a housekeeping pass visited (DB-18, DB-11).

use tessari_encoding::decode_payload;
use tessari_types::{DatabaseId, FieldId, IndexId, NamespaceId, RecordId, TableId};

use super::rows::Lookup;
use super::system::{Level, SYSTEM_DATABASE, SYSTEM_NAMESPACE};
use super::{
    Catalog, DatabaseDefinition, FieldDefinition, IndexDefinition, TableDefinition, definition,
    qualify, system,
};
use crate::error::Result;
use crate::transaction::Window;

impl Catalog<'_, '_> {
    /// The ids `level`'s names claim under `parents`, in id order — the order a
    /// read of the whole definition table answers in.
    ///
    /// Held between statements as a row is (`rows`): a write asks this for its
    /// table's indexes and fields at every commit and every statement.
    async fn claimed(&self, level: Level, parents: &[u32]) -> Result<Vec<u32>> {
        let prefix = qualify(level, parents, "");
        // This transaction's own names are read through it, never from what is
        // held for everybody's snapshot.
        if self
            .transaction
            .has_written_in(SYSTEM_NAMESPACE, SYSTEM_DATABASE, system::NAMES)
        {
            return self.read_claimed(prefix).await;
        }
        let store = self.transaction.store();
        let fill = match store
            .catalog_rows()
            .lookup_span(&prefix, self.transaction.snapshot())
        {
            Lookup::Held(ids) => return Ok(ids),
            Lookup::Missing(fill) => fill,
        };
        let reads_landed = self.reads_only_landed();
        let ids = self.read_claimed(prefix.clone()).await?;
        if let Some(generation) = fill
            && reads_landed
        {
            store
                .catalog_rows()
                .fill_span(prefix, generation, ids.clone());
        }
        Ok(ids)
    }

    /// [`Self::claimed`] read from `NAMES` at this transaction's snapshot.
    async fn read_claimed(&self, prefix: String) -> Result<Vec<u32>> {
        // The prefix ends in `/`, so the first text past every name that begins
        // with it ends in the next byte, `0` — and a parent whose id merely
        // begins with this one's digits (`…/30/`) starts at or after it.
        let mut past = prefix.clone();
        past.pop();
        past.push('0');
        let (from, to) = (RecordId::from(prefix), RecordId::from(past));
        let mut ids = Vec::new();
        for (_, payload) in self
            .transaction
            .records_between(
                SYSTEM_NAMESPACE,
                SYSTEM_DATABASE,
                system::NAMES,
                Window {
                    from: Some(&from),
                    to: Some((&to, false)),
                },
                None,
                usize::MAX,
            )
            .await?
        {
            ids.push(definition::id_of(&decode_payload(&payload)?, "name", "id")?);
        }
        ids.sort_unstable();
        Ok(ids)
    }

    /// Every database in one namespace.
    ///
    /// # Errors
    ///
    /// Returns an error when a stored definition cannot be read.
    pub async fn databases_in(&self, namespace: NamespaceId) -> Result<Vec<DatabaseDefinition>> {
        let mut found = Vec::new();
        for id in self.claimed(Level::Database, &[namespace.get()]).await? {
            if let Some(database) = self.database(DatabaseId::new(id)).await?
                && database.namespace == namespace
            {
                found.push(database);
            }
        }
        Ok(found)
    }

    /// Every table in one database.
    ///
    /// # Errors
    ///
    /// Returns an error when a stored definition cannot be read.
    pub async fn tables_in(
        &self,
        namespace: NamespaceId,
        database: DatabaseId,
    ) -> Result<Vec<TableDefinition>> {
        let mut found = Vec::new();
        for id in self
            .claimed(Level::Table, &[namespace.get(), database.get()])
            .await?
        {
            if let Some(table) = self.table(TableId::new(id)).await?
                && table.namespace == namespace
                && table.database == database
            {
                found.push(table);
            }
        }
        Ok(found)
    }

    /// Every index on one table, named by its whole tenancy.
    ///
    /// This is what a write path consults, once per table a commit touches, so
    /// it reads that table's own definitions and nothing else (module note). The
    /// tenancy is part of the question because a table id is not a key on its
    /// own: the catalog's own tables share numbers with declared ones. With it
    /// the table's definition is not needed to find the span — so a table that
    /// has none (the catalog's own, or a dropped one, whose index names stay
    /// claimed) costs its own span too, and the catalog's answers empty.
    ///
    /// # Errors
    ///
    /// Returns an error when a stored definition cannot be read.
    pub async fn indexes_on(
        &self,
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
    ) -> Result<Vec<IndexDefinition>> {
        let parents = [namespace.get(), database.get(), table.get()];
        let mut found = Vec::new();
        for id in self.claimed(Level::Index, &parents).await? {
            if let Some(index) = self.index(IndexId::new(id)).await?
                && index.namespace == namespace
                && index.database == database
                && index.table == table
            {
                found.push(index);
            }
        }
        Ok(found)
    }

    /// Every field declared on a table a reader names, its tenancy read from the
    /// table's own definition; a table with none declares nothing a reader may
    /// use (as [`Self::field_indexes_on`] for indexes).
    ///
    /// # Errors
    ///
    /// Returns an error when a stored definition cannot be read.
    pub async fn declared_fields(&self, table: TableId) -> Result<Vec<FieldDefinition>> {
        match self.table(table).await? {
            Some(defined) => {
                self.fields_on(defined.namespace, defined.database, table)
                    .await
            }
            None => Ok(Vec::new()),
        }
    }

    /// Every field declared on one table, found the way [`Self::indexes_on`]
    /// finds its indexes.
    ///
    /// # Errors
    ///
    /// Returns an error when a stored definition cannot be read.
    pub async fn fields_on(
        &self,
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
    ) -> Result<Vec<FieldDefinition>> {
        let parents = [namespace.get(), database.get(), table.get()];
        let mut found = Vec::new();
        for id in self.claimed(Level::Field, &parents).await? {
            if let Some(field) = self.field(FieldId::new(id)).await?
                && field.namespace == namespace
                && field.database == database
                && field.table == table
            {
                found.push(field);
            }
        }
        Ok(found)
    }
}
