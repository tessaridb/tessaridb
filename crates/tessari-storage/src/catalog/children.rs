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
    fn claimed(&self, level: Level, parents: &[u32]) -> Result<Vec<u32>> {
        let prefix = qualify(level, parents, "");
        // The prefix ends in `/`, so the first text past every name that begins
        // with it ends in the next byte, `0` — and a parent whose id merely
        // begins with this one's digits (`…/30/`) starts at or after it.
        let mut past = prefix.clone();
        past.pop();
        past.push('0');
        let (from, to) = (RecordId::from(prefix), RecordId::from(past));
        let mut ids = Vec::new();
        for (_, payload) in self.transaction.records_between(
            SYSTEM_NAMESPACE,
            SYSTEM_DATABASE,
            system::NAMES,
            Window {
                from: Some(&from),
                to: Some((&to, false)),
            },
            None,
            usize::MAX,
        )? {
            ids.push(definition::id_of(&decode_payload(&payload)?, "name", "id")?);
        }
        ids.sort_unstable();
        Ok(ids)
    }

    /// The ids `level`'s names claim under one table, or `None` when the table
    /// has no definition: a dropped table's children are found by the whole
    /// read, which is the only thing that still reaches them.
    fn claimed_under(&self, level: Level, table: TableId) -> Result<Option<Vec<u32>>> {
        let Some(parent) = self.table(table)? else {
            return Ok(None);
        };
        let parents = [
            parent.namespace.get(),
            parent.database.get(),
            parent.id.get(),
        ];
        self.claimed(level, &parents).map(Some)
    }

    /// Every database in one namespace.
    ///
    /// # Errors
    ///
    /// Returns an error when a stored definition cannot be read.
    pub fn databases_in(&self, namespace: NamespaceId) -> Result<Vec<DatabaseDefinition>> {
        let mut found = Vec::new();
        for id in self.claimed(Level::Database, &[namespace.get()])? {
            if let Some(database) = self.database(DatabaseId::new(id))?
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
    pub fn tables_in(
        &self,
        namespace: NamespaceId,
        database: DatabaseId,
    ) -> Result<Vec<TableDefinition>> {
        let mut found = Vec::new();
        for id in self.claimed(Level::Table, &[namespace.get(), database.get()])? {
            if let Some(table) = self.table(TableId::new(id))?
                && table.namespace == namespace
                && table.database == database
            {
                found.push(table);
            }
        }
        Ok(found)
    }

    /// Every index on one table.
    ///
    /// This is what a write path consults, once per table a commit touches, so
    /// it reads that table's own definitions and nothing else (module note).
    ///
    /// # Errors
    ///
    /// Returns an error when a stored definition cannot be read.
    pub fn indexes_on(&self, table: TableId) -> Result<Vec<IndexDefinition>> {
        let Some(ids) = self.claimed_under(Level::Index, table)? else {
            return self.every_index_on(table);
        };
        let mut found = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(index) = self.index(IndexId::new(id))?
                && index.table == table
            {
                found.push(index);
            }
        }
        Ok(found)
    }

    /// Every field declared on one table, found the way [`Self::indexes_on`]
    /// finds its indexes.
    ///
    /// # Errors
    ///
    /// Returns an error when a stored definition cannot be read.
    pub fn fields_on(&self, table: TableId) -> Result<Vec<FieldDefinition>> {
        let Some(ids) = self.claimed_under(Level::Field, table)? else {
            let mut found = self.fields()?;
            found.retain(|definition| definition.table == table);
            return Ok(found);
        };
        let mut found = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(field) = self.field(FieldId::new(id))?
                && field.table == table
            {
                found.push(field);
            }
        }
        Ok(found)
    }
}
