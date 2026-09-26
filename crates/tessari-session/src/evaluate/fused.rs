//! The last stages of a fused read: order by rank, bound, then project (G038).
//!
//! # Why this read projects after it orders
//!
//! Every other read projects first, so a sort key may name what the answer
//! carries. A fused read cannot: `search::ranks()` answers where the record came
//! in each branch, which exists only once every branch has ranked every record.
//! So the branches here read the record as it is stored — a branch naming an
//! alias of the projection reads nothing — and the projection runs over the
//! records the bound kept, with their ranks in scope.

use tessari_ql::Select;
use tessari_storage::Transaction;
use tessari_types::{RecordId, Value};

use super::{Answered, alone, asserted};
use crate::budget::Budget;
use crate::consume::{Consumer, Shaping};
use crate::error::Result;
use crate::noticed::Noticed;
use crate::outcome::Note;
use crate::plan::Plan;
use crate::search::Searched;
use crate::session::Session;
use crate::shape::Topmost;

impl Session<'_> {
    /// The answer of a fused read over the records its source produced.
    pub(super) fn fused_answer(
        &self,
        transaction: &mut Transaction<'_>,
        select: &Select,
        records: Vec<(RecordId, Value)>,
        (searched, noticed): (&Searched, &Noticed),
        (budget, plan, mut notes): (&mut Budget, Plan, Vec<Note>),
    ) -> Result<Answered> {
        let keys = self.folded_order(transaction, &select.order)?;
        let mut shaping = Shaping::new(
            self,
            None,
            keys,
            searched,
            Topmost::of(select, None),
            budget,
            noticed,
        );
        for (id, record) in records {
            if shaping.take(transaction, id, record)?.is_break() {
                break;
            }
        }
        let skip = select
            .start
            .map_or(0, |start| usize::try_from(start).unwrap_or(usize::MAX));
        let keep = select.limit.map_or(usize::MAX, |limit| {
            usize::try_from(limit).unwrap_or(usize::MAX)
        });
        let wanted = self.shaped(transaction, select)?;
        let mut answered = Vec::new();
        for (id, record, ranks) in shaping.finish_fused().into_iter().skip(skip).take(keep) {
            let shaped = match &wanted {
                Some(wanted) => self.project_with(
                    transaction,
                    &id,
                    &record,
                    wanted,
                    (searched, noticed),
                    Some(&ranks),
                )?,
                None => record,
            };
            answered.push((id, shaped));
        }
        asserted(select, &plan)?;
        notes.extend(noticed.drained());
        alone(select, &answered)?;
        Ok(Answered {
            records: answered,
            plan,
            notes,
            suggestion: searched.suggestion(),
        })
    }
}
