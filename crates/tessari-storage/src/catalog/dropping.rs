//! Dropping tables, databases and namespaces.

use super::system::Level;
use super::{Catalog, id_key, qualify, system};
use crate::error::Result;
use tessari_types::{DatabaseId, NamespaceId, RecordId, TableId};

impl<'a, 'txn> Catalog<'a, 'txn> {
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
}
