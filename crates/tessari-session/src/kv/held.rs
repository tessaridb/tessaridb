//! A read of a space held in memory, on a cluster (G079 W5d).
//!
//! On a cluster a held space lives on the leader of its range and nowhere else,
//! so any other node holds none of it — or a copy the leadership has since left
//! behind — and an answer there would pass for an empty or a current space. The
//! read is sent to the leader instead, as a bounded read is.

use tessari_ql::{Span, StatementKind};
use tessari_storage::{Catalog, SpaceDeclaration, Store, TableKind};

use crate::error::{Error, Result};
use crate::session::Session;

impl<'a> Session<'a> {
    /// Refuse a read of a held space this node does not lead, naming its
    /// leader.
    pub(crate) async fn held_read_here(
        &self,
        store: &'a Store,
        kind: &StatementKind,
        span: Span,
    ) -> Result<()> {
        for table in crate::reach::tables_named(kind) {
            let mut transaction = store.begin().await?;
            // A table that does not resolve is refused by whatever resolves it.
            let Ok((context, id)) = self.resolve_readable_table(&mut transaction, table).await
            else {
                continue;
            };
            let held = Catalog::new(&mut transaction)
                .table_shared(id)
                .await?
                .is_some_and(|definition| {
                    matches!(
                        definition.kind,
                        TableKind::Space(SpaceDeclaration { memory: true, .. })
                    )
                });
            transaction.rollback();
            if held
                && let Some((endpoint, node, epoch)) = store
                    .led_elsewhere(context.namespace, context.database)
                    .await?
            {
                return Err(Error::ReadIsElsewhere {
                    because: "a read of a space held in memory".to_owned(),
                    endpoint,
                    node,
                    epoch,
                    span,
                });
            }
        }
        Ok(())
    }
}
