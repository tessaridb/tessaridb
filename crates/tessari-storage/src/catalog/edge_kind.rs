//! An edge kind — the join a graph's adjacency is written under.
//!
//! A catalog entry like any other: an ordinary record in the system tenancy
//! (ADR-0009), so declaring one takes part in the transaction that issued it and
//! replicates through the same apply path.
//!
//! **It is not a table**, and that is the whole reason it has a level of its own
//! rather than a flag on [`super::TableDefinition`]. A node kind *is* a table —
//! selected from, inserted into, indexed, granted on — differing from an ordinary
//! one by exactly one fact, which is why membership of a graph is a clause on
//! `DEFINE TABLE`. An edge kind is never selected from: its entries are adjacency
//! keys beside the node, not records behind an index. Nothing about it fits the
//! shape a table definition describes, so it does not borrow that shape.
//!
//! Both endpoint tables must belong to the same graph as the kind. That is what
//! bounds a walk: a traversal cannot leave the graph through a join whose far
//! side was never part of it.

use std::collections::BTreeMap;

use tessari_types::{DatabaseId, EdgeKindId, GraphId, NamespaceId, RecordId, TableId, Value};

use super::definition::{field_id, field_name, number, object};
use super::{Catalog, Level, id_key, qualify, system};
use crate::error::Result;

const FIELD_ID: &str = "id";
const FIELD_NAME: &str = "name";
const FIELD_NAMESPACE: &str = "namespace";
const FIELD_DATABASE: &str = "database";
const FIELD_GRAPH: &str = "graph";
const FIELD_FROM: &str = "from";
const FIELD_TO: &str = "to";
const FIELD_EDGES: &str = "edges";

const ENTITY: &str = "edge kind";

/// A declared edge kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeKindDefinition {
    /// Its id, carried between the node and the neighbour in every adjacency key.
    pub id: EdgeKindId,
    /// The namespace it belongs to.
    pub namespace: NamespaceId,
    /// The database it belongs to.
    pub database: DatabaseId,
    /// The graph that bounds it.
    pub graph: GraphId,
    /// Its name, unique within its database.
    pub name: String,
    /// The table an edge of this kind leaves.
    pub from: TableId,
    /// The table an edge of this kind reaches.
    pub to: TableId,
    /// The companion table an edge of this kind is stored in.
    ///
    /// Invisible in the language: its name carries a byte no identifier can
    /// hold, so nothing can select from it, drop it or index it by naming it.
    /// It exists so that an edge is an ordinary record mutation, which is what
    /// carries it through the log to every replica — adjacency is *derived* from
    /// that mutation on each node, exactly as an index entry is. A walk never
    /// reads it: the neighbours and their properties are in the adjacency
    /// entries.
    pub edges: TableId,
}

impl EdgeKindDefinition {
    /// The value written to the catalog.
    #[must_use]
    pub fn to_value(&self) -> Value {
        Value::Object(BTreeMap::from([
            (FIELD_ID.to_owned(), number(self.id.get())),
            (FIELD_NAMESPACE.to_owned(), number(self.namespace.get())),
            (FIELD_DATABASE.to_owned(), number(self.database.get())),
            (FIELD_GRAPH.to_owned(), number(self.graph.get())),
            (FIELD_NAME.to_owned(), Value::from(self.name.as_str())),
            (FIELD_FROM.to_owned(), number(self.from.get())),
            (FIELD_TO.to_owned(), number(self.to.get())),
            (FIELD_EDGES.to_owned(), number(self.edges.get())),
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
            id: EdgeKindId::new(field_id(fields, FIELD_ID, ENTITY)?),
            namespace: NamespaceId::new(field_id(fields, FIELD_NAMESPACE, ENTITY)?),
            database: DatabaseId::new(field_id(fields, FIELD_DATABASE, ENTITY)?),
            graph: GraphId::new(field_id(fields, FIELD_GRAPH, ENTITY)?),
            name: field_name(fields, ENTITY)?,
            from: TableId::new(field_id(fields, FIELD_FROM, ENTITY)?),
            to: TableId::new(field_id(fields, FIELD_TO, ENTITY)?),
            edges: TableId::new(field_id(fields, FIELD_EDGES, ENTITY)?),
        })
    }
}

impl Catalog<'_, '_> {
    /// Declare an edge kind in a graph.
    ///
    /// The caller has already resolved the graph and both endpoint tables, and
    /// has already checked that they belong to it — that refusal carries a span
    /// and so lives with the statement, exactly as the endpoint refusal on an
    /// edge table does.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NameTaken`] when the name is already declared in that
    /// database.
    pub fn create_edge_kind(
        &mut self,
        graph: &super::GraphDefinition,
        name: &str,
        from: TableId,
        to: TableId,
        edges: TableId,
    ) -> Result<EdgeKindDefinition> {
        let qualified = qualify(
            Level::EdgeKind,
            &[graph.namespace.get(), graph.database.get()],
            name,
        );
        self.reserve_name(&qualified)?;
        let id = EdgeKindId::new(self.allocate(Level::EdgeKind)?);
        let definition = EdgeKindDefinition {
            id,
            namespace: graph.namespace,
            database: graph.database,
            graph: graph.id,
            name: name.to_owned(),
            from,
            to,
            edges,
        };
        self.write(system::EDGE_KINDS, id.get(), &definition.to_value());
        self.claim_name(&qualified, id.get());
        Ok(definition)
    }

    /// Look an edge kind up by id.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored definition cannot be read.
    pub fn edge_kind(&self, id: EdgeKindId) -> Result<Option<EdgeKindDefinition>> {
        self.read(system::EDGE_KINDS, id.get())?
            .as_ref()
            .map(EdgeKindDefinition::from_value)
            .transpose()
    }

    /// Resolve an edge-kind name within a database.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored entry cannot be read.
    pub fn edge_kind_id(
        &self,
        namespace: NamespaceId,
        database: DatabaseId,
        name: &str,
    ) -> Result<Option<EdgeKindId>> {
        Ok(self
            .resolve(&qualify(
                Level::EdgeKind,
                &[namespace.get(), database.get()],
                name,
            ))?
            .map(EdgeKindId::new))
    }

    /// Every edge kind declared in one database.
    ///
    /// # Errors
    ///
    /// Returns an error when a stored definition cannot be read.
    pub fn edge_kinds_in(
        &self,
        namespace: NamespaceId,
        database: DatabaseId,
    ) -> Result<Vec<EdgeKindDefinition>> {
        Ok(self
            .all(system::EDGE_KINDS, EdgeKindDefinition::from_value)?
            .into_iter()
            .filter(|found| found.namespace == namespace && found.database == database)
            .collect())
    }

    /// Drop an edge kind's definition and release its name.
    ///
    /// Answers `false` when there was nothing under that id.
    ///
    /// **The adjacency it named is not deleted here**, for the division every
    /// other drop in this module keeps: a catalog call removes its own
    /// definition, and the caller — which holds the transaction the entries must
    /// disappear in — removes what hangs off it. The id is not released, because
    /// a reused id would let a stale adjacency key resolve against a different
    /// kind and nothing in the store could detect it.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored definition cannot be read.
    pub fn drop_edge_kind(&mut self, id: EdgeKindId) -> Result<bool> {
        let Some(definition) = self.edge_kind(id)? else {
            return Ok(false);
        };
        let qualified = qualify(
            Level::EdgeKind,
            &[definition.namespace.get(), definition.database.get()],
            &definition.name,
        );
        self.transaction.delete(system::address(
            system::EDGE_KINDS,
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
    use crate::error::Error;

    fn definition() -> EdgeKindDefinition {
        EdgeKindDefinition {
            id: EdgeKindId::new(7),
            namespace: NamespaceId::new(1),
            database: DatabaseId::new(2),
            graph: GraphId::new(3),
            name: "works_at".to_owned(),
            from: TableId::new(4),
            to: TableId::new(5),
            edges: TableId::new(6),
        }
    }

    #[test]
    fn a_definition_survives_the_round_trip_it_is_stored_through() {
        let original = definition();
        let read_back = EdgeKindDefinition::from_value(&original.to_value()).unwrap();
        assert_eq!(read_back, original);
    }

    #[test]
    fn the_two_endpoints_are_stored_separately_and_do_not_swap() {
        // `from` and `to` are both table ids, so a decoder that read them in the
        // wrong order would produce a definition that is valid, plausible, and
        // reverses every edge of this kind with nothing in an error state.
        let original = definition();
        let read_back = EdgeKindDefinition::from_value(&original.to_value()).unwrap();
        assert_eq!(read_back.from, TableId::new(4));
        assert_eq!(read_back.to, TableId::new(5));
        assert_ne!(read_back.from, read_back.to);
    }

    #[test]
    fn a_stored_kind_missing_its_graph_is_refused_rather_than_defaulted() {
        // A graph that decoded to zero would put the kind's adjacency under a
        // graph nothing resolves, where no walk would find it and nothing would
        // report it as lost.
        let Value::Object(mut fields) = definition().to_value() else {
            panic!("an edge kind encodes to an object");
        };
        fields.remove(FIELD_GRAPH);
        let error = EdgeKindDefinition::from_value(&Value::Object(fields)).unwrap_err();
        assert!(
            matches!(error, Error::CatalogMalformed { entity: ENTITY, .. }),
            "{error}"
        );
    }
}
