//! Declaring and dropping graphs and their edge kinds.

use crate::error::{Depended, Error, Result};
use crate::outcome::Outcome;
use crate::session::Session;
use tessari_ql::{Name, Span};
use tessari_storage::{Catalog, RecordAddress, TableKind, TableShape, Transaction};
use tessari_types::IdentityKind;

impl Session<'_> {
    /// `DEFINE GRAPH social` — the graph, **and the collection its own nodes
    /// live in**.
    ///
    /// Two statements behind one word, exactly as [`Self::define_vector`] and
    /// [`Self::define_geo`] are three behind theirs, and for the same reason: a
    /// graph that owns no records is a label rather than a structure. Without
    /// the collection a caller cannot write a single node until they have
    /// declared a table of their own and marked it `IN <graph>` — so the word
    /// named a structure and delivered a membership flag, which is the objection
    /// that was raised against it twice.
    ///
    /// The collection takes the **graph's own name**, which is what makes
    /// `CREATE social:1 = { … }` the obvious spelling and keeps the declared
    /// engines symmetrical: `DEFINE VECTOR embeddings` is written into as
    /// `embeddings`, and now `DEFINE GRAPH social` is written into as `social`.
    ///
    /// The two names do not collide because [`qualify`] reserves a name under
    /// its **level**, so `graph:<ns>/<db>/social` and `table:<ns>/<db>/social`
    /// are separate reservations. That same reservation is load-bearing a second
    /// time: it is what guarantees the graph's node collection is the *one*
    /// member that can carry the graph's name, which is how [`Self::drop_graph`]
    /// tells it apart from a table the caller attached. A pre-existing table
    /// called `social` therefore refuses this statement with `NameTaken` rather
    /// than being silently adopted, and the graph row rolls back with it.
    ///
    /// A collection rather than a declared table, because the node shape is the
    /// caller's to decide — `DEFINE FIELD … ON social` narrows it afterwards for
    /// anyone who wants that, the same way it would on any other collection.
    ///
    /// [`qualify`]: tessari_storage::Catalog
    pub(crate) fn define_graph(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        if_not_exists: bool,
        span: Span,
    ) -> Result<Outcome> {
        let context = self.context(transaction, None, span)?;
        if if_not_exists
            && Catalog::new(transaction)
                .graph_id(context.namespace, context.database, &name.text)?
                .is_some()
        {
            return Ok(Outcome::Done);
        }
        let graph = Catalog::new(transaction).create_graph(
            context.namespace,
            context.database,
            &name.text,
        )?;
        self.define_table(
            transaction,
            name,
            TableShape {
                schemafull: false,
                kind: TableKind::Collection,
                identity: IdentityKind::default(),
                graph: Some(graph.id),
                conflict: None,
                split: Vec::new(),
                partition: None,
                spread: false,
            },
            if_not_exists,
            span,
        )?;
        Ok(Outcome::Done)
    }

    /// `DROP GRAPH social` — refused while a table still belongs to it.
    ///
    /// The same stance [`Self::drop_database`] takes one level away: the
    /// statement asks whether anything still depends, because it holds the span
    /// to refuse with. Dropping anyway would leave every member table pointing
    /// at an id nothing resolves, and the symptom would surface later as a walk
    /// that finds no graph rather than now as the drop that caused it.
    pub(crate) fn drop_graph(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<Outcome> {
        let context = self.context(transaction, None, span)?;
        let id = Catalog::new(transaction)
            .graph_id(context.namespace, context.database, &name.text)?
            .ok_or_else(|| Error::Unknown {
                entity: "graph",
                name: name.text.clone(),
                span,
            })?;
        // The graph's own node collection is not a dependant — it is part of the
        // structure being dropped, and it carries the graph's name because
        // nothing else is allowed to. Counting it here would make every graph
        // this store creates permanently undroppable, refused by a table the
        // caller never declared and cannot name. That is the companion-table
        // shape the bucket already found once; see `StatementKind::DropTable`.
        let (own, attached): (Vec<_>, Vec<_>) = Catalog::new(transaction)
            .tables_in(context.namespace, context.database)?
            .into_iter()
            .filter(|table| table.graph == Some(id))
            .partition(|table| table.name == name.text);
        if let Some(first) = attached.first() {
            return Err(Error::StillDepended {
                depended: Depended::GraphByTable,
                name: name.text.clone(),
                count: attached.len(),
                first: first.name.clone(),
                span,
            });
        }
        let kinds: Vec<_> = Catalog::new(transaction)
            .edge_kinds_in(context.namespace, context.database)?
            .into_iter()
            .filter(|kind| kind.graph == id)
            .collect();
        if let Some(first) = kinds.first() {
            return Err(Error::StillDepended {
                depended: Depended::GraphByEdgeKind,
                name: name.text.clone(),
                count: kinds.len(),
                first: first.name.clone(),
                span,
            });
        }
        for table in own {
            Catalog::new(transaction).drop_table(table.id)?;
        }
        Catalog::new(transaction).drop_graph(id)?;
        Ok(Outcome::Done)
    }

    /// `DEFINE EDGE works_at IN social FROM person TO company`.
    ///
    /// Everything is resolved before anything is written, so a declaration that
    /// names a graph or a table that is not there leaves the store exactly as it
    /// found it — the same ordering the membership clause keeps.
    pub(crate) fn define_edge(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        graph: &Name,
        (from, to): (&Name, &Name),
        if_not_exists: bool,
        span: Span,
    ) -> Result<Outcome> {
        let context = self.context(transaction, None, span)?;
        if if_not_exists
            && Catalog::new(transaction)
                .edge_kind_id(context.namespace, context.database, &name.text)?
                .is_some()
        {
            return Ok(Outcome::Done);
        }
        let graph_id = Catalog::new(transaction)
            .graph_id(context.namespace, context.database, &graph.text)?
            .ok_or_else(|| Error::Unknown {
                entity: "graph",
                name: graph.text.clone(),
                span,
            })?;
        let declared =
            Catalog::new(transaction)
                .graph(graph_id)?
                .ok_or_else(|| Error::Unknown {
                    entity: "graph",
                    name: graph.text.clone(),
                    span,
                })?;

        let mut endpoints = Vec::with_capacity(2);
        for endpoint in [from, to] {
            let id = Catalog::new(transaction)
                .table_id(context.namespace, context.database, &endpoint.text)?
                .ok_or_else(|| Error::Unknown {
                    entity: "table",
                    name: endpoint.text.clone(),
                    span,
                })?;
            // Both endpoints must be in the graph, and this is the refusal that
            // bounds a walk: a far side outside the structure would let a
            // traversal leave it and still answer.
            let member = Catalog::new(transaction)
                .table(id)?
                .is_some_and(|table| table.graph == Some(graph_id));
            if !member {
                return Err(Error::EndpointOutsideGraph {
                    table: endpoint.text.clone(),
                    graph: graph.text.clone(),
                    span,
                });
            }
            endpoints.push(id);
        }

        // The companion table holds the edges as ordinary records, which is what
        // carries them — and the adjacency derived from them — through the log to
        // every replica. Nothing can name it.
        let edges = Catalog::new(transaction).create_table(
            context.namespace,
            context.database,
            &Catalog::edges_named(&name.text),
            TableShape::default(),
        )?;
        Catalog::new(transaction).create_edge_kind(
            &declared,
            &name.text,
            endpoints[0],
            endpoints[1],
            edges.id,
        )?;
        Ok(Outcome::Done)
    }

    /// `DROP EDGE works_at` — the kind, its edges, and the adjacency they wrote.
    ///
    /// The edges are deleted rather than the entries being range-swept, and that
    /// is deliberate: deleting a record produces a tombstone in the same log
    /// record, and the adjacency derived from it is removed in the batch that
    /// carries the deletion. A second, parallel way to remove an entry is how one
    /// of the two ends up forgotten.
    pub(crate) fn drop_edge(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<Outcome> {
        let context = self.context(transaction, None, span)?;
        let id = Catalog::new(transaction)
            .edge_kind_id(context.namespace, context.database, &name.text)?
            .ok_or_else(|| Error::Unknown {
                entity: "edge kind",
                name: name.text.clone(),
                span,
            })?;
        let kind = Catalog::new(transaction)
            .edge_kind(id)?
            .ok_or_else(|| Error::Unknown {
                entity: "edge kind",
                name: name.text.clone(),
                span,
            })?;

        let edges: Vec<_> = transaction
            .scan_table(context.namespace, context.database, kind.edges)?
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        for id in edges {
            transaction.delete(RecordAddress::new(
                context.namespace,
                context.database,
                kind.edges,
                id,
            ));
        }
        Catalog::new(transaction).drop_table(kind.edges)?;
        Catalog::new(transaction).drop_edge_kind(id)?;
        Ok(Outcome::Done)
    }
}
