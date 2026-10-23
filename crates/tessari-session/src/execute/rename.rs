//! `ALTER TABLE t RENAME TO u` (ADR-0134 D9): the table moved to a new name.

use tessari_ql::{Name, Span, TableRef};
use tessari_storage::{Catalog, TableDefinition, TableKind, Transaction};

use crate::context::Context;
use crate::error::{Error, Result};
use crate::outcome::Outcome;
use crate::reach::tables_named;
use crate::session::Session;

impl Session<'_> {
    /// Rename a table, refusing while a view or an event names it by text.
    ///
    /// Everything that holds the table by id moves with it. What holds it by
    /// **name** is written text — a view's read, an event's statements — and a
    /// rename that left those naming nothing would succeed now and break them
    /// at their next run, so it is refused naming the first of them.
    pub(super) async fn rename_table(
        &self,
        transaction: &mut Transaction<'_>,
        table: &TableRef,
        to: &Name,
        span: Span,
    ) -> Result<Outcome> {
        let (context, id) = self.resolve_table(transaction, table).await?;
        let Some(definition) = Catalog::new(transaction).table(id).await? else {
            return Ok(Outcome::Done);
        };
        if let Some(why) = companion(transaction, &definition).await? {
            return Err(Error::NotRenamed {
                table: definition.name,
                why,
                span,
            });
        }
        let named = naming(transaction, &context, &definition.name).await?;
        if let Some(first) = named.first() {
            return Err(Error::TableNamedByText {
                table: definition.name,
                by: first.clone(),
                more: named.len().saturating_sub(1),
                span,
            });
        }
        Catalog::new(transaction).rename_table(id, &to.text).await?;
        Ok(Outcome::Done)
    }
}

/// Why a table cannot move on its own: something it brought into being is
/// found by its name.
async fn companion(
    transaction: &mut Transaction<'_>,
    definition: &TableDefinition,
) -> Result<Option<&'static str>> {
    if definition.is_bucket() {
        return Ok(Some("a bucket's bytes are kept in a table named after it"));
    }
    if let TableKind::Topic(topic) = &definition.kind
        && topic.partitions.is_some()
    {
        return Ok(Some("a split topic's partitions are named after it"));
    }
    if definition.deduplication_window().is_some() {
        return Ok(Some(
            "the markers it deduplicates with are kept in a table named after it",
        ));
    }
    if let Some(graph) = definition.graph
        && let Some(graph) = Catalog::new(transaction).graph(graph).await?
        && graph.name == definition.name
    {
        return Ok(Some(
            "it is its graph's own node collection, named as the graph is",
        ));
    }
    Ok(None)
}

/// The views and events in this database whose text names the table.
pub(super) async fn naming(
    transaction: &mut Transaction<'_>,
    context: &Context,
    name: &str,
) -> Result<Vec<String>> {
    let database = Catalog::new(transaction)
        .database(context.database)
        .await?
        .map(|found| found.name);
    let names_it = |reference: &TableRef| {
        reference.name.text == name
            && reference
                .database
                .as_ref()
                .is_none_or(|qualifier| Some(&qualifier.text) == database.as_ref())
    };
    let mut found = Vec::new();
    for held in Catalog::new(transaction)
        .tables_in(context.namespace, context.database)
        .await?
    {
        if let TableKind::View(view) = &held.kind
            && script_names(&view.read, &names_it)
        {
            found.push(format!("view `{}`", held.name));
        }
        for event in &held.events {
            let condition = event
                .when
                .as_ref()
                .is_some_and(|when| script_names(&format!("RETURN {when};"), &names_it));
            if condition || script_names(&event.body, &names_it) {
                found.push(format!("event `{}` on `{}`", event.name, held.name));
            }
        }
    }
    Ok(found)
}

/// Whether any statement of a stored text names the table.
fn script_names(text: &str, names_it: &impl Fn(&TableRef) -> bool) -> bool {
    // Stored text this engine accepted parses; one that does not names nothing
    // this rename could break further.
    tessari_ql::parse(text).is_ok_and(|script| {
        script
            .statements
            .iter()
            .any(|statement| tables_named(&statement.kind).into_iter().any(names_it))
    })
}
