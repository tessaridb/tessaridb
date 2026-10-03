//! A grouping read of a split table this node holds only part of, folded on
//! the shards' leaders rather than gathered as records (ADR-0097 D2).

use tessari_ql::{Select, Source};
use tessari_storage::Transaction;

use crate::aggregate::Groups;
use crate::error::Result;
use crate::noticed::Noticed;
use crate::outcome::{AccessPath, Note};
use crate::plan::Plan;
use crate::session::Session;

impl Session<'_> {
    /// The groups `select` folds into, when it is a grouping read every part of
    /// which the leaders can fold exactly and this node lacks part of its table;
    /// `None` for every other read, which then prepares its source as before.
    pub(super) fn prepare_folded(
        &self,
        transaction: &mut Transaction<'_>,
        select: &Select,
        (notes, noticed): (&mut Vec<Note>, &Noticed),
        within: Option<crate::budget::Deadline>,
    ) -> Result<Option<(Groups, Plan)>> {
        let Some(mut reduce) = crate::reduce::reduce_of(select) else {
            return Ok(None);
        };
        let (table, condition) = match &select.from {
            Source::Table(table) => (table, None),
            Source::Where { table, condition } => (table, Some(&**condition)),
            _ => return Ok(None),
        };
        let (_, id) = self.resolve_table(transaction, table)?;
        self.refuse_reading_a_vault(transaction, id, table)?;
        reduce.visible = self.visible_in(transaction, id)?;
        let Some((groups, note)) = self.gather_folded(
            transaction,
            id,
            &reduce,
            (select, condition),
            noticed,
            within,
        )?
        else {
            return Ok(None);
        };
        notes.push(note);
        Ok(Some((
            groups,
            Plan::new(AccessPath::Scan).on(table.name.text.as_str()),
        )))
    }
}
