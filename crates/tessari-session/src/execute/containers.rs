//! Declaring and dropping namespaces, databases, graphs and edge kinds.

use tessari_ql::{Name, Span};
use tessari_storage::{Catalog, RecordAddress, TableKind, TableShape, Transaction};

use tessari_types::{IdentityKind, Replication, ReplicationClass};

use crate::error::{Depended, Error, Result};
use crate::outcome::Outcome;
use crate::session::Session;

impl Session<'_> {
    pub(super) fn define_namespace(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        if_not_exists: bool,
        replication: Option<Replication>,
        class: Option<ReplicationClass>,
    ) -> Result<Outcome> {
        if if_not_exists
            && Catalog::new(transaction)
                .namespace_id(&name.text)?
                .is_some()
        {
            // The clause is not applied on this branch, and that is the same
            // reading `IF NOT EXISTS` already has everywhere else: the
            // statement did nothing because the namespace was there, so it
            // changes nothing about it either. A definition that quietly
            // re-set a policy on a namespace it did not create would be an
            // `ALTER` wearing a `DEFINE`'s spelling.
            return Ok(Outcome::Done);
        }
        // Asked only of the branch that actually creates one, and only when the
        // statement said nothing: a store with no peers has nowhere to put a
        // second copy, so there the bare form is what a single-node install has
        // always written and is stored as *never stated*. A store that declares
        // a peer is a cluster, and there a namespace holding one copy is a
        // decision somebody is making — ADR-0060's whole point — so it is
        // written down rather than inherited.
        if replication.is_none() {
            let peers = Catalog::new(transaction).replicas()?.len();
            if peers > 0 {
                return Err(Error::ReplicationUnstated {
                    namespace: name.text.clone(),
                    peers,
                    span: name.span,
                });
            }
        }
        let definition = Catalog::new(transaction).create_namespace(&name.text)?;
        if let Some(replication) = replication {
            // Through the same call an `ALTER` makes, so the two statements
            // cannot set this field differently.
            Catalog::new(transaction).set_replication(definition.id, replication)?;
        }
        if let Some(class) = class {
            // The same route for the same reason. No `ALTER` sets the class
            // today — G027 S2.1 needs only a declaration — and the setter
            // exists in the shape an `ALTER` would use so that adding one later
            // is a statement rather than a second write path.
            Catalog::new(transaction).set_replication_class(definition.id, class)?;
        }
        Ok(Outcome::Done)
    }

    /// `ALTER NAMESPACE prod REPLICATION FACTOR 3`
    ///
    /// Turning replication on for a namespace that already holds data, and off
    /// again (owner requirement D12). **Nothing is redistributed**, and the
    /// absence of a repair step is the point rather than an omission: the log
    /// already holds every write the namespace ever took, so a follower that
    /// begins subscribing replays it from origin. Cassandra's `ALTER KEYSPACE`
    /// needs a `nodetool repair` afterwards because its replicas hold data
    /// rather than a history; ours needs none because the history is the store.
    pub(super) fn alter_namespace(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        replication: Replication,
    ) -> Result<Outcome> {
        let Some(namespace) = Catalog::new(transaction).namespace_id(&name.text)? else {
            return Err(Error::Unknown {
                entity: "namespace",
                name: name.text.clone(),
                span: name.span,
            });
        };
        Catalog::new(transaction).set_replication(namespace, replication)?;
        Ok(Outcome::Done)
    }

    pub(super) fn define_database(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        if_not_exists: bool,
        span: Span,
    ) -> Result<Outcome> {
        let namespace = self.namespace_id(transaction, span)?;
        if if_not_exists
            && Catalog::new(transaction)
                .database_id(namespace, &name.text)?
                .is_some()
        {
            return Ok(Outcome::Done);
        }
        Catalog::new(transaction).create_database(namespace, &name.text)?;
        Ok(Outcome::Done)
    }

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
    pub(super) fn define_graph(
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
    pub(super) fn drop_graph(
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
    #[expect(
        clippy::too_many_arguments,
        reason = "the statement's own shape; a struct here would name a grouping \
                  the grammar does not have"
    )]
    pub(super) fn define_edge(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        graph: &Name,
        from: &Name,
        to: &Name,
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
    pub(super) fn drop_edge(
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

    /// `DROP DATABASE staging` — refused while it still holds a table.
    ///
    /// The bound is the one `DELETE … LIMIT` established: a destructive
    /// statement carrying no predicate at all is the widest thing this language
    /// can be asked to run, and the person writing it is thinking about one
    /// name. The refusal counts and names, so acting on it needs no second
    /// query.
    pub(super) fn drop_database(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<Outcome> {
        let context = self.context(transaction, Some(name.text.as_str()), span)?;
        let held = Catalog::new(transaction).tables_in(context.namespace, context.database)?;
        if let Some(first) = held.first() {
            return Err(Error::StillDepended {
                depended: Depended::DatabaseByTable,
                name: name.text.clone(),
                count: held.len(),
                first: first.name.clone(),
                span,
            });
        }
        Catalog::new(transaction).drop_database(context.database)?;
        Ok(Outcome::Done)
    }

    /// `DROP NAMESPACE acme` — refused while it still holds a database.
    ///
    /// One level up from [`Self::drop_database`] and refusing on the same
    /// ground. Resolved by name against the catalog rather than through the
    /// session's tenancy, because a namespace is what a tenancy is selected
    /// *within* — asking the context for it would require having selected it.
    pub(super) fn drop_namespace(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<Outcome> {
        let id = Catalog::new(transaction)
            .namespace_id(&name.text)?
            .ok_or_else(|| Error::Unknown {
                entity: "namespace",
                name: name.text.clone(),
                span,
            })?;
        let held = Catalog::new(transaction).databases_in(id)?;
        if let Some(first) = held.first() {
            return Err(Error::StillDepended {
                depended: Depended::NamespaceByDatabase,
                name: name.text.clone(),
                count: held.len(),
                first: first.name.clone(),
                span,
            });
        }
        Catalog::new(transaction).drop_namespace(id)?;
        Ok(Outcome::Done)
    }
}
