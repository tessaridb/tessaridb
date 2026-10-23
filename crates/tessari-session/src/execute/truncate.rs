//! `TRUNCATE TABLE t`: every record removed, the definition kept (ADR-0134 D9).

use tessari_ql::TableRef;
use tessari_storage::{Catalog, RecordAddress, TableKind, Transaction};

use crate::error::{Error, Result};
use crate::evaluate::Part;
use crate::outcome::Outcome;
use crate::session::Session;

use super::expiry::kind_word;

impl Session<'_> {
    /// Remove every record of a table in one transaction, running no event.
    ///
    /// The indexes lose their entries with the records, as any delete does, and
    /// the table's definition, indexes, grants and events stay. A node holding
    /// part of a split table refuses rather than emptying its part and calling
    /// that the table.
    pub(super) async fn truncate(
        &self,
        transaction: &mut Transaction<'_>,
        table: &TableRef,
    ) -> Result<Outcome> {
        self.not_referenced(transaction, table, "emptied").await?;
        let (context, id) = self.resolve_table(transaction, table).await?;
        if let Some(definition) = Catalog::new(transaction).table_shared(id).await?
            && !matches!(
                definition.kind,
                TableKind::Table
                    | TableKind::Collection
                    | TableKind::Edge(_)
                    | TableKind::Vector(_)
                    | TableKind::Geo
                    | TableKind::Queue(_)
                    | TableKind::Vault(_)
            )
        {
            return Err(Error::TruncateNotOnThisKind {
                table: table.name.text.clone(),
                kind: kind_word(&definition.kind),
                span: table.span,
            });
        }
        self.refuse_reading_a_part(transaction, id, Part::Whole)
            .await?;
        let found = transaction
            .sweep_table(context.namespace, context.database, id)
            .await?;
        let count = u64::try_from(found.len()).unwrap_or(u64::MAX);
        for (record_id, _) in found {
            transaction.delete(RecordAddress::new(
                context.namespace,
                context.database,
                id,
                record_id,
            ));
        }
        Ok(Outcome::Removed { count })
    }
}
