//! A read of a strict table naming a field the table does not declare.
//!
//! A strict table refuses a write carrying a field nobody declared, so no record
//! of it holds one, and a read naming such a field finds `none` everywhere. That
//! answer is right for the statement and wrong for its author: the mistake a
//! strict write refuses (a misspelling, or `id`, which is a record's address
//! rather than a field) reads as *nothing matched*. Refusing the read instead
//! would break every statement that already runs; the note says it.

use std::collections::BTreeSet;

use tessari_ql::{Select, Source};
use tessari_storage::{Catalog, TableKind, Transaction};

use crate::error::Result;
use crate::outcome::Note;
use crate::session::Session;

impl Session<'_> {
    /// The note for the fields this read names that its strict table does not
    /// declare, when there are any.
    ///
    /// Only the condition and the projection of a read of one table are judged:
    /// a join, a traversal and a subquery read records other than the table's
    /// own, and a schemaless table or a collection may hold any field at all.
    pub(super) async fn undeclared_read(
        &self,
        transaction: &mut Transaction<'_>,
        select: &Select,
    ) -> Result<Option<Note>> {
        let (table, condition) = match &select.from {
            Source::Where {
                table, condition, ..
            } => (table, Some(condition)),
            Source::Table(table) | Source::Range { table, .. } => (table, None),
            _ => return Ok(None),
        };
        // Ordered, so the note names the fields the same way on every run.
        let mut named = BTreeSet::new();
        if let Some(condition) = condition {
            crate::plan::roots_read(condition, &mut named);
        }
        for projected in select.projection.written() {
            crate::plan::roots_read(&projected.value, &mut named);
        }
        if named.is_empty() {
            return Ok(None);
        }
        // A note never refuses: a name that does not resolve, or resolves to a
        // view, is the read's own resolution to report, in its own words.
        let Ok((_, id)) = self.resolve_any_table(transaction, table).await else {
            return Ok(None);
        };
        let catalog = Catalog::new(transaction);
        let Some(definition) = catalog.table(id).await? else {
            return Ok(None);
        };
        if !definition.schemafull || definition.kind != TableKind::Table {
            return Ok(None);
        }
        for field in catalog
            .fields_on(definition.namespace, definition.database, definition.id)
            .await?
        {
            let root = field.name.split('.').next().unwrap_or(&field.name);
            named.remove(root);
        }
        Ok((!named.is_empty()).then(|| Note::UndeclaredField {
            table: definition.name,
            fields: named.into_iter().collect(),
        }))
    }
}
