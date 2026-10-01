//! Declaring, finding, rebuilding and dropping indexes.

use super::system::Level;
use super::{Catalog, IndexDefinition, IndexShape, id_key, qualify, system};
use crate::error::{Error, Result};
use tessari_encoding::decode_payload;
use tessari_types::{IndexId, Path, RecordId, TableId};

impl<'a, 'txn> Catalog<'a, 'txn> {
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
    /// [`Error::EmptyIndex`] when no field is named, [`Error::IndexReadsOneField`]
    /// when a search, spatial or vector index names more than one, and [`Error::NameTaken`]
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
        // Each of these reads the first field and no other, so a second one
        // named would be a declaration the index does not keep.
        let one_field = if shape.search {
            Some("SEARCH")
        } else if shape.spatial {
            Some("SPATIAL")
        } else if shape.vector.is_some() {
            Some("VECTOR")
        } else {
            None
        };
        if let Some(kind) = one_field
            && fields.len() > 1
        {
            return Err(Error::IndexReadsOneField {
                name: name.to_owned(),
                kind,
                fields: fields.len(),
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
            costs: shape.costs,
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
}
