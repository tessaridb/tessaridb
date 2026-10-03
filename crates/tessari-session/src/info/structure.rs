//! INFO for the store, a namespace, a database, a table, a graph and a vector.

use std::collections::BTreeMap;

use tessari_ql::{Name, Span, TableRef};
use tessari_storage::{Catalog, ConsumerDefinition, GEO_FIELD, TableKind, Transaction};
use tessari_types::{Number, Value};

use crate::describe;
use crate::error::{Error, Result};
use crate::session::Session;

use super::{
    by_name, described_field, described_index, described_sample, nameable, readable_field,
    readable_index, refining, reported, shape_of,
};

impl Session<'_> {
    /// The namespaces.
    ///
    /// # The system tenancy is absent, and not because this filters it out
    ///
    /// Namespace zero holds the catalog and was never created through the
    /// language, so it has no definition record for [`Catalog::namespaces`] to
    /// find. It is unaddressable rather than hidden — the same property that
    /// makes `USE NAMESPACE <anything>` unable to select it. A listing that had
    /// to *remember* to exclude it would be one somebody could later forget to,
    /// which is exactly the change this statement was expected to bring.
    pub(super) fn info_store(
        &self,
        transaction: &mut Transaction<'_>,
    ) -> Result<BTreeMap<String, Value>> {
        let own = self.identity.user().and_then(|user| user.namespace);
        let mut names = Vec::new();
        for namespace in Catalog::new(transaction).namespaces()? {
            // A user declared `ON prod.orders` belongs to one namespace and may
            // not name another, so the store as they may see it holds one.
            if own.is_some_and(|id| id != namespace.id) {
                continue;
            }
            names.push(namespace.name);
        }
        Ok(BTreeMap::from([("namespaces".to_owned(), by_name(names))]))
    }

    /// The databases in the selected namespace, and how many copies of it the
    /// cluster is asked to keep.
    ///
    /// The replication key is **present either way**, and answers `NONE` — the
    /// `Value::None` that means *no value here*, not the policy spelled
    /// `REPLICATION NONE` — for a namespace that never stated one. Reporting it
    /// only when it was set would make silence look like a missing feature
    /// rather than an unanswered question, and this answer is the only place an
    /// operator can see which of the two they have (ADR-0060).
    pub(super) fn info_namespace(
        &self,
        transaction: &mut Transaction<'_>,
        span: Span,
    ) -> Result<BTreeMap<String, Value>> {
        let namespace = self.namespace_id(transaction, span)?;
        let own = self.identity.user().and_then(|user| user.database);
        let held = Catalog::new(transaction).namespace(namespace)?;
        let replication = held
            .as_ref()
            .and_then(|definition| definition.replication)
            .map_or(Value::None, tessari_types::Replication::to_value);
        // G027 S4.1. How many writers the range admits is not derivable from
        // anything else in this report, and it decides what a write to it MEANS:
        // on a multi-master range a concurrent write is refused and named
        // (ADR-0075), and on a single-leader one it cannot arise. An operator
        // who cannot read the class from the engine is guessing which semantics
        // their data has.
        //
        // `NONE` where nothing was declared, rather than the key being absent:
        // silence is a decision here — an undeclared namespace is single-leader
        // — and a missing key and a key holding `NONE` are different statements
        // to anything reading this report. Its neighbour above already uses the
        // same convention for the same reason.
        let class = held
            .as_ref()
            .and_then(|definition| definition.class)
            .map_or(Value::None, tessari_types::ReplicationClass::to_value);
        // ADR-0106 D2: what a write here waits for before its caller is told.
        // `NONE` where nothing was declared, for its neighbours' reason — the
        // level an unstated namespace waits for is derived from its
        // replication, and a derivation reported as a decision hides whether
        // anybody ever made one.
        let acknowledge = held
            .and_then(|definition| definition.acknowledge)
            .map_or(Value::None, tessari_types::Acknowledgement::to_value);
        let mut names = Vec::new();
        for database in Catalog::new(transaction).databases_in(namespace)? {
            if own.is_some_and(|id| id != database.id) {
                continue;
            }
            names.push(database.name);
        }
        Ok(BTreeMap::from([
            ("databases".to_owned(), by_name(names)),
            ("replication".to_owned(), replication),
            ("class".to_owned(), class),
            ("acknowledge".to_owned(), acknowledge),
        ]))
    }

    pub(super) fn info_database(
        &self,
        transaction: &mut Transaction<'_>,
        span: Span,
    ) -> Result<BTreeMap<String, Value>> {
        let context = self.context(transaction, None, span)?;
        let readable = self.readable_in(transaction)?;
        let mut names = Vec::new();
        let mut topics = Vec::new();
        let mut vaults = Vec::new();
        for table in Catalog::new(transaction).tables_in(context.namespace, context.database)? {
            // A bucket's chunks live in a companion table whose name carries a
            // byte no identifier can hold, so no statement can name it and
            // `SELECT * FROM media` answers with files rather than chunks
            // (ADR-0011 §2). Listing it here would undo that in the one place
            // that enumerates rather than resolves.
            if !nameable(&table.name) {
                continue;
            }
            if readable
                .as_ref()
                .is_some_and(|granted| !granted.contains(&table.id))
            {
                continue;
            }
            // The topics among them, by name, because a topic is read with its
            // own statements and a caller listing a database wants to know which
            // of its tables those are without asking about each one (G042).
            if matches!(table.kind, TableKind::Topic(_)) {
                topics.push(table.name.clone());
            }
            // And the vaults, for the same reason: a vault is read with `REVEAL`
            // and listed with `INFO FOR VAULT … RECORDS`, never with `SELECT`.
            if matches!(table.kind, TableKind::Vault(_)) {
                vaults.push(table.name.clone());
            }
            names.push(table.name);
        }
        // And the searches (ADR-0105), once each, where the caller reads at least
        // one of their tables: `INFO FOR TABLE` skips a search's members, so this
        // is the one enumeration that reaches them.
        let searches: std::collections::BTreeSet<String> = Catalog::new(transaction)
            .engine_members()?
            .into_iter()
            .filter(|member| {
                member.namespace == context.namespace
                    && member.database == context.database
                    && readable
                        .as_ref()
                        .is_none_or(|granted| granted.contains(&member.table))
            })
            .filter_map(|member| member.engine.map(|engine| engine.search))
            .collect();
        Ok(BTreeMap::from([
            ("tables".to_owned(), by_name(names)),
            ("topics".to_owned(), by_name(topics)),
            ("vaults".to_owned(), by_name(vaults)),
            (
                "searches".to_owned(),
                by_name(searches.into_iter().collect()),
            ),
        ]))
    }

    /// One table's shape, its fields and its indexes.
    ///
    /// The table itself is guarded before this runs: `INFO FOR TABLE` names its
    /// table, so `tables_named` hands it to the grant check and an ungranted
    /// caller is refused there, with the same message a `SELECT` from it gives.
    ///
    /// What is left is the **field** grant, which does not refuse — it edits.
    /// A caller granted `FIELDS name` reads records with `salary` already
    /// removed, so a report naming `salary` as a declared field would disclose
    /// what every read of theirs hides. The index list is filtered by the same
    /// rule and for the same reason: an index is named after the values it
    /// projects, so `by_salary ON staff FIELDS salary` says the field exists as
    /// plainly as the field list would.
    pub(super) fn info_table(
        &self,
        transaction: &mut Transaction<'_>,
        table: &TableRef,
    ) -> Result<BTreeMap<String, Value>> {
        // The one read that may name a view: describing one is the point of
        // asking, and a report refused because the subject is a view would be a
        // report nobody could get for the thing they asked about.
        let (_, id) = self.resolve_any_table(transaction, table)?;
        let visible = self.visible_in(transaction, id)?;
        let catalog = Catalog::new(transaction);
        let Some(definition) = catalog.table(id)? else {
            return Err(Error::Unknown {
                entity: "table",
                name: table.name.text.clone(),
                span: table.span,
            });
        };
        let mut fields = catalog.fields_on(id)?;
        let mut indexes = catalog.field_indexes_on(id)?;
        fields.sort_by(|left, right| left.name.cmp(&right.name));
        indexes.sort_by(|left, right| left.name.cmp(&right.name));
        let declared = (fields.len(), indexes.len());
        fields.retain(|field| readable_field(&visible, &field.name));
        indexes.retain(|index| readable_index(&visible, index));
        let whole = declared == (fields.len(), indexes.len());
        let mut report = shape_of(&definition);
        // What the balancing pass last measured of its shards (ADR-0113 D4),
        // present only on the node that measures them — the store line's
        // leader — and only once a pass has run.
        if let Some((_, sampled)) = self
            .store
            .sampled_shards()
            .into_iter()
            .find(|(table, _)| *table == id)
        {
            report.insert("sampled".to_owned(), described_sample(&sampled));
        }
        report.insert(
            "fields".to_owned(),
            Value::Array(fields.iter().map(described_field).collect()),
        );
        report.insert(
            "indexes".to_owned(),
            Value::Array(indexes.iter().map(described_index).collect()),
        );
        // Each event as the statement that defines it (ADR-0110 D1).
        report.insert(
            "events".to_owned(),
            Value::Array(
                definition
                    .events
                    .iter()
                    .map(|event| {
                        let mut statement = String::new();
                        describe::write_event(&mut statement, &definition.name, event);
                        Value::from(statement.trim_end())
                    })
                    .collect(),
            ),
        );
        let (key, held) = match describe::declaration(&definition, &fields, &indexes) {
            // A narrowed view gets no script. The report above is already the
            // subset this caller may read, and that is a truthful *description*;
            // a **declaration** built from the same subset is not, because it
            // claims to re-create the table and would re-create a different one.
            // Handing it over would also disclose through the definition exactly
            // what the field grant removes from every read they make.
            Ok(_) if !whole => (
                "undefinable",
                "fields or indexes of this table are hidden from this caller".to_owned(),
            ),
            Ok(mut script) => {
                for event in &definition.events {
                    describe::write_event(&mut script, &definition.name, event);
                }
                ("definition", script)
            }
            Err(unwritable) => ("undefinable", unwritable.part),
        };
        report.insert(key.to_owned(), Value::from(held.as_str()));
        // A kept view says how fresh it is (ADR-0109 D5): the version its rows
        // equal its read at, how many versions the store has moved past it, how
        // many rows it holds, and when it last reached the head.
        if let Some((state, head)) = crate::materialized::freshness(self.store, transaction, id)? {
            let count =
                |held: u64| Value::Number(Number::Integer(i64::try_from(held).unwrap_or(i64::MAX)));
            let rows = Catalog::new(transaction).record_count(id)?.unwrap_or(0);
            let mut kept = BTreeMap::new();
            kept.insert("version".to_owned(), count(state.version.get()));
            kept.insert(
                "behind".to_owned(),
                count(head.get().saturating_sub(state.version.get())),
            );
            kept.insert("rows".to_owned(), count(rows));
            kept.insert(
                "refreshed".to_owned(),
                Value::Number(Number::Integer(state.refreshed)),
            );
            report.insert("materialized".to_owned(), Value::Object(kept));
        }
        Ok(report)
    }

    /// The tables in the selected database, narrowed to those this session may
    /// read.
    /// One graph, and the tables that belong to it.
    ///
    /// An empty list is a legitimate answer and is what a graph declared a
    /// moment ago reports: the graph exists, and reporting it as absent would
    /// make the first thing anyone does after declaring one look like a failure.
    /// A graph that does not exist is the different case, and refuses.
    ///
    /// Membership is read by filtering the database's tables rather than from a
    /// list held on the graph, because the membership already lives on the table
    /// — a second copy on the graph would be a fact able to disagree with
    /// itself, which is the same reason a bucket's chunk table is derived from
    /// its name rather than stored beside it.
    pub(super) fn info_graph(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<BTreeMap<String, Value>> {
        let context = self.context(transaction, None, span)?;
        let id = Catalog::new(transaction)
            .graph_id(context.namespace, context.database, &name.text)?
            .ok_or_else(|| Error::Unknown {
                entity: "graph",
                name: name.text.clone(),
                span,
            })?;
        let readable = self.readable_in(transaction)?;
        let mut names = Vec::new();
        for table in Catalog::new(transaction).tables_in(context.namespace, context.database)? {
            if table.graph != Some(id) || !nameable(&table.name) {
                continue;
            }
            if readable
                .as_ref()
                .is_some_and(|granted| !granted.contains(&table.id))
            {
                continue;
            }
            names.push(table.name);
        }
        // The edge kinds are listed beside the node tables rather than folded in
        // with them, because they are not tables: nothing can select from one,
        // and a report that mixed the two would invite a caller to try.
        let kinds: Vec<_> = Catalog::new(transaction)
            .edge_kinds_in(context.namespace, context.database)?
            .into_iter()
            .filter(|kind| kind.graph == id)
            .map(|kind| kind.name)
            .collect();
        Ok(BTreeMap::from([
            ("name".to_owned(), Value::from(name.text.as_str())),
            ("tables".to_owned(), by_name(names)),
            ("edges".to_owned(), by_name(kinds)),
        ]))
    }

    /// `INFO FOR VECTOR embeddings` — its width, its distance, and its recall.
    ///
    /// The third field is the reason this is not `INFO FOR TABLE`. A vector
    /// index answers approximately, so the only number that says whether its
    /// answers are worth having is the recall it was **measured** at — and that
    /// is a property of a measurement rather than of a declaration, which is why
    /// no table report could ever carry it.
    ///
    /// It reads `none` until the index is **built**, which is where the whole
    /// graph and every stored vector are in hand at once and the measurement is
    /// therefore free of extra reads. A store declared and filled but never
    /// rebuilt reports `none`, and that is the honest answer rather than a gap:
    /// a figure derived from the build parameters would be a number nobody
    /// checked, wearing the name of one somebody did.
    ///
    /// When there is a figure it never appears alone. It carries the `k` it was
    /// measured at, how many queries it averaged, **how many records the store
    /// held at the time**, and the engine constants in force — because recall
    /// decays as records are added after a build, so a bare percentage goes
    /// stale in silence, which is the same failure by the other door.
    pub(super) fn info_vector(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<BTreeMap<String, Value>> {
        let context = self.context(transaction, None, span)?;
        let missing = || Error::Unknown {
            entity: "vector store",
            name: name.text.clone(),
            span,
        };
        let id = Catalog::new(transaction)
            .table_id(context.namespace, context.database, &name.text)?
            .ok_or_else(missing)?;
        let definition = Catalog::new(transaction).table(id)?.ok_or_else(missing)?;
        let TableKind::Vector(declared) = definition.kind else {
            return Err(missing());
        };
        // The store's index, found by the property that makes it one rather than
        // by the name the desugaring gave it — a name is a spelling and this is
        // the thing itself.
        let index = Catalog::new(transaction)
            .field_indexes_on(id)?
            .into_iter()
            .find(|index| index.vector.is_some());
        let (measured, footprint, quantized) = match index {
            Some(index) => (
                transaction.vector_recall(&index)?,
                transaction.vector_node_bytes(&index)?,
                index.quantized,
            ),
            None => (None, None, false),
        };
        Ok(BTreeMap::from([
            ("name".to_owned(), Value::from(name.text.as_str())),
            (
                "dimension".to_owned(),
                Value::Number(Number::Integer(i64::from(declared.dimension))),
            ),
            ("distance".to_owned(), Value::from(declared.distance.name())),
            // `None` and not a zero. A recall of zero is a measurement saying
            // the index finds nothing; absence says nobody has asked. Reporting
            // the second as the first is the failure this field exists to avoid.
            ("recall".to_owned(), measured.map_or(Value::None, reported)),
            ("quantized".to_owned(), Value::Bool(quantized)),
            // Measured off the stored nodes: the average bytes one takes, beside
            // how many there are. `None` while the store holds no vector.
            (
                "node_bytes".to_owned(),
                footprint.map_or(Value::None, |(_, average, _)| {
                    Value::Number(Number::Integer(i64::try_from(average).unwrap_or(i64::MAX)))
                }),
            ),
            (
                "vector_bytes".to_owned(),
                footprint.map_or(Value::None, |(_, _, per_vector)| {
                    Value::Number(Number::Integer(
                        i64::try_from(per_vector).unwrap_or(i64::MAX),
                    ))
                }),
            ),
            (
                "nodes".to_owned(),
                footprint.map_or(Value::None, |(nodes, _, _)| {
                    Value::Number(Number::Integer(i64::try_from(nodes).unwrap_or(i64::MAX)))
                }),
            ),
        ]))
    }

    pub(super) fn info_geo(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<BTreeMap<String, Value>> {
        let context = self.context(transaction, None, span)?;
        let missing = || Error::Unknown {
            entity: "geo store",
            name: name.text.clone(),
            span,
        };
        let id = Catalog::new(transaction)
            .table_id(context.namespace, context.database, &name.text)?
            .ok_or_else(missing)?;
        let definition = Catalog::new(transaction).table(id)?.ok_or_else(missing)?;
        if definition.kind != TableKind::Geo {
            return Err(missing());
        }
        // Found by the property that makes it the store's index rather than by
        // the name the desugaring gave it, for the reason the vector store gives:
        // a name is a spelling, and this is the thing itself.
        let index = Catalog::new(transaction)
            .field_indexes_on(id)?
            .into_iter()
            .find(|index| index.spatial);
        let measured = match &index {
            Some(index) => transaction.spatial_refinement(index)?,
            None => None,
        };
        Ok(BTreeMap::from([
            ("name".to_owned(), Value::from(name.text.as_str())),
            ("field".to_owned(), Value::from(GEO_FIELD)),
            (
                "index".to_owned(),
                index.map_or(Value::None, |index| Value::from(index.name.as_str())),
            ),
            // `None` and not a zero, for the reason the vector store's recall is:
            // a ratio of zero would read as a filter that admits nothing, where
            // absence says nobody measured. Absence also covers a store whose
            // records never reach one another, which has no refinement cost to
            // report rather than a perfect one.
            (
                "refinement".to_owned(),
                measured.map_or(Value::None, refining),
            ),
        ]))
    }

    /// The destination, written the way a statement would name it.
    ///
    /// Resolved back from ids rather than stored as text, so a table renamed
    /// under a consumer reports its new name instead of the one that was typed.
    pub(super) fn named_table(
        &self,
        transaction: &mut Transaction<'_>,
        consumer: &ConsumerDefinition,
    ) -> Result<String> {
        let named = Catalog::new(transaction)
            .tables_in(consumer.namespace, consumer.database)?
            .into_iter()
            .find(|table| table.id == consumer.destination)
            .map(|table| table.name);
        // A destination that has been dropped is reported as gone rather than
        // omitted: a consumer writing into nothing is the condition an operator
        // is looking for, and a missing field reads as a display bug.
        Ok(named.unwrap_or_else(|| "<dropped>".to_owned()))
    }
}
