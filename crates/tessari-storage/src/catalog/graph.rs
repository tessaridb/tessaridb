//! A graph — the object a node table belongs to.
//!
//! A catalog entry like any other: an ordinary record in the system tenancy
//! (ADR-0009), so declaring one takes part in the transaction that issued it and
//! replicates through the same apply path.
//!
//! What the entry buys is **identity**. Before it, "the social graph" was a fact
//! in somebody's head about which tables were related: nothing could enumerate
//! it, nothing could drop it, and nothing could be asked a question *about* it.
//! A graph is now a thing the store holds, which is the precondition for
//! everything the engine adds later — a bounded walk needs a boundary, and a
//! question about the whole needs a whole to name.
//!
//! It is **scoped to a database**, like a table and unlike an analyzer: two
//! tenants may each keep a `social` and neither shadows the other, because a
//! graph describes one tenant's data rather than a property of the language.

use std::collections::BTreeMap;

use tessari_types::{DatabaseId, GraphId, NamespaceId, RecordId, Value};

use super::definition::{field_id, field_name, number, object};
use super::{Catalog, Level, id_key, qualify, system};
use crate::error::{Error, Result};

const FIELD_ID: &str = "id";
const FIELD_NAME: &str = "name";
const FIELD_NAMESPACE: &str = "namespace";
const FIELD_DATABASE: &str = "database";

const ENTITY: &str = "graph";

/// A declared graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphDefinition {
    /// Its id.
    ///
    /// Carried into the leading bytes of every adjacency key above the node it
    /// belongs to, which is what makes the whole structure one prefix.
    pub id: GraphId,
    /// The namespace it belongs to.
    pub namespace: NamespaceId,
    /// The database it belongs to.
    pub database: DatabaseId,
    /// Its name, unique within its database.
    pub name: String,
}

impl GraphDefinition {
    /// The value written to the catalog.
    #[must_use]
    pub fn to_value(&self) -> Value {
        Value::Object(BTreeMap::from([
            (FIELD_ID.to_owned(), number(self.id.get())),
            (FIELD_NAMESPACE.to_owned(), number(self.namespace.get())),
            (FIELD_DATABASE.to_owned(), number(self.database.get())),
            (FIELD_NAME.to_owned(), Value::from(self.name.as_str())),
        ]))
    }

    /// Read a definition back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when a field is missing or holds the
    /// wrong type.
    pub fn from_value(value: &Value) -> Result<Self> {
        let fields = object(value, ENTITY)?;
        Ok(Self {
            id: GraphId::new(field_id(fields, FIELD_ID, ENTITY)?),
            namespace: NamespaceId::new(field_id(fields, FIELD_NAMESPACE, ENTITY)?),
            database: DatabaseId::new(field_id(fields, FIELD_DATABASE, ENTITY)?),
            name: field_name(fields, ENTITY)?,
        })
    }
}

impl Catalog<'_, '_> {
    /// Declare a graph in a database.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoSuchParent`] when the database does not belong to the
    /// namespace, and [`Error::NameTaken`] when the name is already declared in
    /// that database.
    pub fn create_graph(
        &mut self,
        namespace: NamespaceId,
        database: DatabaseId,
        name: &str,
    ) -> Result<GraphDefinition> {
        let parent = self.database(database)?;
        if parent.is_none_or(|found| found.namespace != namespace) {
            return Err(Error::NoSuchParent {
                entity: "database",
                id: database.get(),
            });
        }
        let qualified = qualify(Level::Graph, &[namespace.get(), database.get()], name);
        self.reserve_name(&qualified)?;
        let id = GraphId::new(self.allocate(Level::Graph)?);
        let definition = GraphDefinition {
            id,
            namespace,
            database,
            name: name.to_owned(),
        };
        self.write(system::GRAPHS, id.get(), &definition.to_value());
        self.claim_name(&qualified, id.get());
        Ok(definition)
    }

    /// Look a graph up by id.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored definition cannot be read.
    pub fn graph(&self, id: GraphId) -> Result<Option<GraphDefinition>> {
        self.read(system::GRAPHS, id.get())?
            .as_ref()
            .map(GraphDefinition::from_value)
            .transpose()
    }

    /// Resolve a graph name within a database.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored entry cannot be read.
    pub fn graph_id(
        &self,
        namespace: NamespaceId,
        database: DatabaseId,
        name: &str,
    ) -> Result<Option<GraphId>> {
        Ok(self
            .resolve(&qualify(
                Level::Graph,
                &[namespace.get(), database.get()],
                name,
            ))?
            .map(GraphId::new))
    }

    /// Every graph declared in one database.
    ///
    /// # Errors
    ///
    /// Returns an error when a stored definition cannot be read.
    pub fn graphs_in(
        &self,
        namespace: NamespaceId,
        database: DatabaseId,
    ) -> Result<Vec<GraphDefinition>> {
        Ok(self
            .all(system::GRAPHS, GraphDefinition::from_value)?
            .into_iter()
            .filter(|found| found.namespace == namespace && found.database == database)
            .collect())
    }

    /// Drop a graph's definition and release its name.
    ///
    /// Answers `false` when there was nothing under that id.
    ///
    /// **Whether any table still belongs to the graph is not asked here**, and
    /// that is the same division [`Self::drop_table`] and [`Self::drop_database`]
    /// keep: a catalog call removes its own definition, and the statement — which
    /// holds the span to refuse with — decides whether anything beneath it makes
    /// the drop wrong.
    ///
    /// The id is not released, for the reason a table id is not: a reused id
    /// would let a stale membership resolve against a different graph, and
    /// nothing in the store could detect it.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored definition cannot be read.
    pub fn drop_graph(&mut self, id: GraphId) -> Result<bool> {
        let Some(definition) = self.graph(id)? else {
            return Ok(false);
        };
        let qualified = qualify(
            Level::Graph,
            &[definition.namespace.get(), definition.database.get()],
            &definition.name,
        );
        self.transaction.delete(system::address(
            system::GRAPHS,
            RecordId::Int(id_key(id.get())),
        ));
        self.transaction
            .delete(system::address(system::NAMES, RecordId::from(qualified)));
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used)]

    use super::*;

    fn definition() -> GraphDefinition {
        GraphDefinition {
            id: GraphId::new(7),
            namespace: NamespaceId::new(1),
            database: DatabaseId::new(2),
            name: "social".to_owned(),
        }
    }

    #[test]
    fn a_definition_survives_the_round_trip_it_is_stored_through() {
        let original = definition();
        let read_back = GraphDefinition::from_value(&original.to_value()).unwrap();
        assert_eq!(read_back, original);
    }

    #[test]
    fn a_stored_graph_missing_its_database_is_refused_rather_than_defaulted() {
        // A graph whose database decoded to zero would land in the system
        // tenancy, where `graphs_in` for a real database would never find it and
        // nothing would report it as lost. Refusing names the corruption at the
        // read that met it.
        let Value::Object(mut fields) = definition().to_value() else {
            panic!("a graph encodes to an object");
        };
        fields.remove(FIELD_DATABASE);
        let error = GraphDefinition::from_value(&Value::Object(fields)).unwrap_err();
        assert!(
            matches!(error, Error::CatalogMalformed { entity: ENTITY, .. }),
            "{error}"
        );
    }
}
