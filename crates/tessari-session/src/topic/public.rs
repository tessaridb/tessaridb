//! The one thing a closed store lets a caller nobody signed in do: append to a
//! topic declared `PUBLIC`, at the rate it declares (G037).
//!
//! # What the door admits, and why each limit
//!
//! - **`USE`**, because it records names and reaches nothing — it resolves no
//!   namespace, so it answers nothing about what exists — and every surface
//!   sends it ahead of the statement.
//! - **`CREATE t = { … }` and `INSERT INTO t …`**, into a `PUBLIC` topic, with
//!   an identity the store generates. A caller-named id is refused because the
//!   refusal of a taken one would tell an anonymous caller which messages exist.
//! - **Nothing that reads.** The statement must name that topic and no other
//!   table at all — a subquery inside the value would answer with whatever it
//!   read in the append's own reply, even from the topic itself.
//!
//! Everything else is refused as it was before, as `NotSignedIn`, from the same
//! place that refuses it for every other statement. The rate is counted per
//! node, in messages rather than statements.

use tessari_ql::{CreateTarget, StatementKind, TableRef};
use tessari_storage::{Catalog, Store, TableKind, TopicDeclaration};

use crate::error::{Error, Result};
use crate::session::Session;

impl<'a> Session<'a> {
    /// Authorize an anonymous caller on a closed store.
    ///
    /// # Errors
    ///
    /// [`Error::NotSignedIn`] for anything but the door above, and
    /// [`Error::TopicRateExceeded`] for an append past the topic's rate.
    pub(crate) fn public_append(
        &self,
        store: &'a Store,
        kind: &StatementKind,
        span: tessari_ql::Span,
    ) -> Result<()> {
        let appended = match kind {
            StatementKind::Use { .. } => return Ok(()),
            StatementKind::Create {
                target: CreateTarget::Generated(table),
                ..
            } => Some((table, 1)),
            StatementKind::Insert { table, rows, .. } => {
                Some((table, u64::try_from(rows.len()).unwrap_or(u64::MAX)))
            }
            _ => None,
        };
        let Some((table, count)) = appended else {
            return Err(Error::NotSignedIn { span });
        };
        // The topic and nothing else: one name, so no read hides in the value.
        if crate::reach::tables_named(kind).len() != 1 {
            return Err(Error::NotSignedIn { span });
        }
        let Some((topic, rule)) = self.public_topic(store, table)? else {
            return Err(Error::NotSignedIn { span });
        };
        if store.admit_public_append(topic, rule, count) {
            Ok(())
        } else {
            Err(Error::TopicRateExceeded {
                topic: table.name.text.clone(),
                rate: rule.rate,
                per: rule.per.to_literal(),
                span,
            })
        }
    }

    /// The topic `table` names and its public rule, when it is one that opens.
    fn public_topic(
        &self,
        store: &'a Store,
        table: &TableRef,
    ) -> Result<Option<(tessari_types::TableId, tessari_storage::PublicAppend)>> {
        let mut transaction = store.begin()?;
        // A table that does not resolve is not a door, and saying which is not
        // this caller's to learn: the answer is the same refusal either way.
        let Ok((_, id)) = self.resolve_table(&mut transaction, table) else {
            transaction.rollback();
            return Ok(None);
        };
        let definition = Catalog::new(&mut transaction).table(id)?;
        transaction.rollback();
        Ok(match definition.map(|definition| definition.kind) {
            Some(TableKind::Topic(TopicDeclaration {
                public: Some(rule), ..
            })) => Some((id, rule)),
            _ => None,
        })
    }
}
