//! The consumers the catalog declares, and where each writes.

use super::{Destination, plain};
use tessari_storage::{Catalog, ConsumerDefinition, Feed, Store};

/// Every declaration, with the statement text its destination is written as.
///
/// The destination is resolved to **names** here, once, rather than per batch:
/// the runner writes through the language, and the only text it composes comes
/// from the catalog. No byte of any message ever reaches a statement.
pub(crate) fn declarations(
    store: &Store,
) -> Result<Vec<(ConsumerDefinition, Destination)>, tessari_storage::Error> {
    let mut transaction = store.begin()?;
    let declared = Catalog::new(&mut transaction).consumers()?;
    let mut found = Vec::new();
    // Kafka declarations only: a topic consumer is run by the topic runner, and
    // opening one here would fail against a broker and mark it stopped in the
    // registry both runners share (ADR-0087).
    for definition in declared
        .into_iter()
        .filter(|definition| matches!(definition.feed, Feed::Kafka { .. }))
    {
        match destination_of(&mut transaction, &definition) {
            Some(table) => found.push((definition, table)),
            None => tracing::warn!(
                consumer = %definition.name,
                "a stream consumer has no destination any more, so it is not started"
            ),
        }
    }
    transaction.rollback();
    Ok(found)
}

/// Resolve a declaration's ids back to the names a statement uses.
pub(crate) fn destination_of(
    transaction: &mut tessari_storage::Transaction<'_>,
    definition: &ConsumerDefinition,
) -> Option<Destination> {
    let catalog = Catalog::new(transaction);
    let namespace = catalog
        .namespaces()
        .ok()?
        .into_iter()
        .find(|held| held.id == definition.namespace)?
        .name;
    let database = catalog
        .databases_in(definition.namespace)
        .ok()?
        .into_iter()
        .find(|held| held.id == definition.database)?
        .name;
    let table = catalog
        .tables_in(definition.namespace, definition.database)
        .ok()?
        .into_iter()
        .find(|held| held.id == definition.destination)?
        .name;
    // Every one of these came from a `DEFINE` statement, so it is already an
    // identifier — but it is checked rather than trusted, because this is the
    // only text this crate composes and the check costs nothing.
    if !plain(&namespace) || !plain(&database) || !plain(&table) {
        return None;
    }
    Some(Destination {
        namespace,
        database,
        table,
    })
}
