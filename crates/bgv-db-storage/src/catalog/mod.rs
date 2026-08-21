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

mod definition;
mod system;

use bgv_db_encoding::{decode_payload, encode_payload};
use bgv_db_types::{DatabaseId, NamespaceId, RecordId, TableId, Value};

pub use definition::{DatabaseDefinition, NamespaceDefinition, TableDefinition};
pub use system::{SYSTEM_DATABASE, SYSTEM_NAMESPACE};

use crate::error::{Error, Result};
use crate::transaction::{RecordAddress, Transaction};
use system::Level;

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
        };
        self.write(system::NAMESPACES, id.get(), &definition.to_value());
        self.claim_name(&qualified, id.get());
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
        };
        self.write(system::TABLES, id.get(), &definition.to_value());
        self.claim_name(&qualified, id.get());
        Ok(definition)
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
    /// catalog call would hide it. Dropping a namespace or a database cascades
    /// into everything below it and is not built yet.
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
    use super::*;

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
