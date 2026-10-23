//! Renaming a table: a catalog move, the id kept (ADR-0134 D9).

use super::system::Level;
use super::{Catalog, qualify, system};
use crate::error::Result;
use tessari_types::{RecordId, TableId};

impl<'a, 'txn> Catalog<'a, 'txn> {
    /// Give a table a new name in its database, keeping its id.
    ///
    /// Everything that holds the table by id — its records, indexes, fields,
    /// grants, edges, consumers — keeps holding it; only the name is moved, and
    /// the new one is reserved first, so a name already taken refuses with
    /// what holds it. Answers `false` when there is no table under that id.
    ///
    /// # Errors
    ///
    /// The new name is taken, or the stored definition cannot be read.
    pub async fn rename_table(&mut self, id: TableId, name: &str) -> Result<bool> {
        let Some(mut definition) = self.table(id).await? else {
            return Ok(false);
        };
        let parents = [definition.namespace.get(), definition.database.get()];
        let wanted = qualify(Level::Table, &parents, name);
        self.reserve_name(&wanted).await?;
        let held = qualify(Level::Table, &parents, &definition.name);
        self.transaction
            .delete(system::address(system::NAMES, RecordId::from(held)));
        self.claim_name(&wanted, id.get());
        name.clone_into(&mut definition.name);
        self.write(system::TABLES, id.get(), &definition.to_value());
        Ok(true)
    }
}

impl Catalog<'_, '_> {
    /// Give a declared field a new name on its table, keeping its id and its
    /// declaration. Answers `false` when there is no field under that id.
    ///
    /// # Errors
    ///
    /// The new name is taken on the table, or the definition cannot be read.
    pub async fn rename_field(&mut self, id: tessari_types::FieldId, name: &str) -> Result<bool> {
        let Some(mut definition) = self.field(id).await? else {
            return Ok(false);
        };
        let parents = [
            definition.namespace.get(),
            definition.database.get(),
            definition.table.get(),
        ];
        let wanted = qualify(Level::Field, &parents, name);
        self.reserve_name(&wanted).await?;
        let held = qualify(Level::Field, &parents, &definition.name);
        self.transaction
            .delete(system::address(system::NAMES, RecordId::from(held)));
        self.claim_name(&wanted, id.get());
        name.clone_into(&mut definition.name);
        self.write(system::FIELDS, id.get(), &definition.to_value());
        Ok(true)
    }
}
