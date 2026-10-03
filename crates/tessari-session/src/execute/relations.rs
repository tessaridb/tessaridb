//! Relating records: edges, and the graphs that type them.

use std::collections::BTreeMap;
use tessari_encoding::decode_payload;
use tessari_ql::{Answer, ColumnDeclaration, EdgeClause, Name, RecordTarget, TableRef};
use tessari_storage::{
    Catalog, EDGE_IN, EDGE_OUT, EdgeDeclaration, EdgeOrder, RecordAddress, TableKind, Transaction,
};

use tessari_types::{GraphId, RecordRef, Value};

use crate::error::{Error, Result};
use crate::outcome::Outcome;
use crate::session::Session;

use super::{answered, edge_identity};

impl Session<'_> {
    /// The kind a `DEFINE TABLE` produces, with any declared pair resolved.
    ///
    /// Resolution happens here rather than inside the table's own creation
    /// because an endpoint that does not exist has to refuse before anything is
    /// written: a table carrying a dangling endpoint id could refuse nothing,
    /// and the clause exists only to refuse.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unknown`] when an endpoint names no table, or when the
    /// order names a field the statement does not declare.
    /// The graph a `DEFINE TABLE … IN social` clause names, resolved to its id.
    ///
    /// The graph is looked up in the tenancy the statement is running in, which
    /// is where `DEFINE GRAPH` put it. A name that resolves to nothing refuses
    /// here, before the table exists, so a refusal leaves the store exactly as
    /// it found it.
    pub(super) fn resolve_graph(
        &self,
        transaction: &mut Transaction<'_>,
        graph: Option<&Name>,
    ) -> Result<Option<GraphId>> {
        let Some(graph) = graph else {
            return Ok(None);
        };
        let context = self.context(transaction, None, graph.span)?;
        let id = Catalog::new(transaction)
            .graph_id(context.namespace, context.database, &graph.text)?
            .ok_or_else(|| Error::Unknown {
                entity: "graph",
                name: graph.text.clone(),
                span: graph.span,
            })?;
        Ok(Some(id))
    }

    pub(super) fn edge_kind(
        &self,
        transaction: &mut Transaction<'_>,
        edge: Option<&EdgeClause>,
        columns: &[ColumnDeclaration],
    ) -> Result<TableKind> {
        let Some(edge) = edge else {
            return Ok(TableKind::Table);
        };
        let EdgeClause::Between(declared) = edge else {
            return Ok(TableKind::Edge(None));
        };
        let (_, from_id) = self.resolve_table(transaction, &declared.from)?;
        let (_, to_id) = self.resolve_table(transaction, &declared.to)?;
        // The ordering field has to be one the table declares. Nothing else can
        // guarantee an edge carries it, and the order is the endpoint index's
        // key suffix rather than a sort applied afterwards: an edge missing the
        // field has no place to be written, and the failure would surface much
        // later as neighbours arriving in roughly the right sequence.
        let order = match &declared.order {
            Some(ordering) => {
                if !columns
                    .iter()
                    .any(|column| column.name.text == ordering.field.text)
                {
                    return Err(Error::Unknown {
                        entity: "field",
                        name: ordering.field.text.clone(),
                        span: ordering.field.span,
                    });
                }
                Some(EdgeOrder {
                    field: ordering.field.text.clone(),
                    descending: ordering.descending,
                })
            }
            None => None,
        };
        Ok(TableKind::Edge(Some(EdgeDeclaration {
            from: from_id,
            to: to_id,
            order,
        })))
    }

    /// Write an edge between two records.
    ///
    /// The edge is an ordinary record in the edge table, carrying `out` and `in`
    /// as record references — so it takes part in the transaction, replicates
    /// through the same path, and is found by the endpoint indexes the table was
    /// given when it was declared. Nothing about a graph needed its own keyspace.
    ///
    /// **The edge's identity is derived from its endpoints**, which makes
    /// `RELATE` idempotent: re-asserting a link that is already there replaces it
    /// rather than adding a second copy. That is the right default for a caller
    /// that re-states what it knows, and it is why two edges between the same
    /// pair in the same table are one edge with properties rather than two
    /// records (Q-41).
    pub(super) fn relate(
        &self,
        transaction: &mut Transaction<'_>,
        from: &RecordTarget,
        edges: &TableRef,
        to: &RecordTarget,
        value: Option<&tessari_ql::Expr>,
    ) -> Result<Outcome> {
        let context = self.context(transaction, None, edges.span)?;
        // An edge *kind* is looked for first, because it is the narrower word: a
        // kind and an edge table cannot share a name (both claim it in the same
        // catalog), so finding one settles which path this is.
        if let Some(kind) = Catalog::new(transaction).edge_kind_id(
            context.namespace,
            context.database,
            &edges.name.text,
        )? {
            return self.relate_in_graph(transaction, kind, from, edges, to, value);
        }

        let (context, edge_table) = self.resolve_table(transaction, edges)?;
        let Some(definition) = Catalog::new(transaction).table(edge_table)? else {
            return Err(Error::NotAnEdgeTable {
                table: edges.name.text.clone(),
                span: edges.span,
            });
        };
        if !definition.is_edge() {
            return Err(Error::NotAnEdgeTable {
                table: edges.name.text.clone(),
                span: edges.span,
            });
        }
        let declared = definition.edge_endpoints().cloned();
        let (_, out) = self.address(transaction, from)?;
        let (_, into) = self.address(transaction, to)?;
        // A table that declared its pair refuses a link between any other, and
        // that refusal is the whole of what the clause buys. It is checked after
        // both endpoints resolve so that a link naming a record that is not
        // there fails as the missing record it is, rather than as a pair the
        // table does not join.
        if let Some(declared) = declared
            && (out.table != declared.from || into.table != declared.to)
        {
            return Err(Error::EndpointsNotDeclared {
                table: edges.name.text.clone(),
                span: edges.span,
            });
        }

        let mut fields = match value {
            Some(expression) => match self.evaluate(transaction, expression)? {
                Value::Object(given) => given,
                // A non-object edge property has nowhere to live beside the two
                // endpoints, so it is refused where it is written rather than
                // silently dropped.
                other => {
                    return Err(Error::EdgePropertiesNotAnObject {
                        found: other.type_name(),
                        span: edges.span,
                    });
                }
            },
            None => BTreeMap::new(),
        };
        fields.insert(
            EDGE_OUT.to_owned(),
            Value::Record(RecordRef::new(out.table, out.id.clone())),
        );
        fields.insert(
            EDGE_IN.to_owned(),
            Value::Record(RecordRef::new(into.table, into.id.clone())),
        );

        let address = RecordAddress::new(
            context.namespace,
            context.database,
            edge_table,
            edge_identity(&out, &into),
        );
        // An edge is an ordinary record, so an edge table's declarations apply
        // to it — including their defaults.
        let payload = self.with_defaults(transaction, edge_table, Value::Object(fields))?;
        self.put_record(transaction, address, payload, edges.span)?;
        Ok(Outcome::Done)
    }

    /// `RELATE person:1->works_at->company:1` — an edge of a declared kind.
    ///
    /// The record written here is never read by a walk. It exists so that the
    /// edge is an ordinary mutation, which is what carries it and the adjacency
    /// derived from it through the log to every replica; the neighbours and their
    /// properties are read from the adjacency entries instead.
    pub(super) fn relate_in_graph(
        &self,
        transaction: &mut Transaction<'_>,
        kind: tessari_types::EdgeKindId,
        from: &RecordTarget,
        edges: &TableRef,
        to: &RecordTarget,
        value: Option<&tessari_ql::Expr>,
    ) -> Result<Outcome> {
        let declared =
            Catalog::new(transaction)
                .edge_kind(kind)?
                .ok_or_else(|| Error::Unknown {
                    entity: "edge kind",
                    name: edges.name.text.clone(),
                    span: edges.span,
                })?;
        let (_, out) = self.address(transaction, from)?;
        let (_, into) = self.address(transaction, to)?;
        // Checked after both endpoints resolve, so a relation naming a record
        // that is not there fails as the missing record rather than as a pair the
        // kind does not join. The order matters too: an unordered check would
        // accept `company:1->works_at->person:1`.
        if out.table != declared.from || into.table != declared.to {
            return Err(Error::EndpointsNotDeclared {
                table: edges.name.text.clone(),
                span: edges.span,
            });
        }

        let mut fields = match value {
            Some(expression) => match self.evaluate(transaction, expression)? {
                Value::Object(given) => given,
                other => {
                    return Err(Error::EdgePropertiesNotAnObject {
                        found: other.type_name(),
                        span: edges.span,
                    });
                }
            },
            None => BTreeMap::new(),
        };
        fields.insert(
            EDGE_OUT.to_owned(),
            Value::Record(RecordRef::new(out.table, out.id.clone())),
        );
        fields.insert(
            EDGE_IN.to_owned(),
            Value::Record(RecordRef::new(into.table, into.id.clone())),
        );

        // Identified by its endpoints, as an edge-table edge is: relating the
        // same pair twice replaces one record rather than adding a second, which
        // is what makes `RELATE` idempotent and keeps the adjacency a set.
        let address = RecordAddress::new(
            declared.namespace,
            declared.database,
            declared.edges,
            edge_identity(&out, &into),
        );
        self.put_record(transaction, address, Value::Object(fields), edges.span)?;
        Ok(Outcome::Done)
    }

    /// `DELETE person:1->works_at->company:1` — one edge, by what it joins.
    ///
    /// The caller writes the two endpoints and the edge, exactly as they wrote
    /// them to create it, and the identity is derived here by the same rule that
    /// derived it there. That is the whole statement: without it, removing an
    /// edge means reconstructing `"person:1->company:1"` by hand, which is a
    /// caller depending on an internal encoding to undo what `RELATE` did — and
    /// a caller who derives it slightly differently deletes nothing and is told
    /// it worked.
    ///
    /// **The adjacency needs no code here.** The entries are derived from this
    /// record's own mutation in `adjacency::maintain`, so the tombstone written
    /// below removes both of them in the batch that carries it — the same reason
    /// `DROP EDGE` needed no sweep.
    pub(super) fn delete_edge(
        &self,
        transaction: &mut Transaction<'_>,
        from: &RecordTarget,
        edges: &TableRef,
        to: &RecordTarget,
        answer: Answer,
    ) -> Result<Outcome> {
        let context = self.context(transaction, None, edges.span)?;
        let (_, out) = self.address(transaction, from)?;
        let (_, into) = self.address(transaction, to)?;

        // An edge kind is looked for first, for the reason `relate` looks for it
        // first: a kind and an edge table cannot share a name, so finding one
        // settles which path this is.
        let address = if let Some(kind) = Catalog::new(transaction).edge_kind_id(
            context.namespace,
            context.database,
            &edges.name.text,
        )? {
            let declared =
                Catalog::new(transaction)
                    .edge_kind(kind)?
                    .ok_or_else(|| Error::Unknown {
                        entity: "edge kind",
                        name: edges.name.text.clone(),
                        span: edges.span,
                    })?;
            // Refused rather than answered with a no-op. The derived identity
            // for a pair the kind does not join cannot exist, so deleting it
            // would succeed and remove nothing — and a caller who wrote the
            // endpoints the wrong way round would be told their edge is gone.
            // `RELATE` refuses the same two shapes, and an asymmetry between the
            // statement that writes an edge and the one that removes it is the
            // surprising thing, not the refusal.
            if out.table != declared.from || into.table != declared.to {
                return Err(Error::EndpointsNotDeclared {
                    table: edges.name.text.clone(),
                    span: edges.span,
                });
            }
            RecordAddress::new(
                declared.namespace,
                declared.database,
                declared.edges,
                edge_identity(&out, &into),
            )
        } else {
            let (context, edge_table) = self.resolve_table(transaction, edges)?;
            let declared = Catalog::new(transaction)
                .table(edge_table)?
                .filter(|found| found.is_edge())
                .ok_or_else(|| Error::NotAnEdgeTable {
                    table: edges.name.text.clone(),
                    span: edges.span,
                })?;
            if let Some(pair) = declared.edge_endpoints()
                && (out.table != pair.from || into.table != pair.to)
            {
                return Err(Error::EndpointsNotDeclared {
                    table: edges.name.text.clone(),
                    span: edges.span,
                });
            }
            RecordAddress::new(
                context.namespace,
                context.database,
                edge_table,
                edge_identity(&out, &into),
            )
        };

        // An edge that is not there deletes as a record that is not there does:
        // `BEFORE` answers `NONE`, which is the true answer to what was removed.
        let before = match transaction.get(&address)? {
            Some(held) => decode_payload(&held)?,
            None => Value::None,
        };
        // Through the delete funnel, so an edge table's events see the edge go
        // as they saw it come (ADR-0110 D8).
        self.delete_record(transaction, address, edges.span)?;
        Ok(answered(answer, before, Value::None))
    }
}
