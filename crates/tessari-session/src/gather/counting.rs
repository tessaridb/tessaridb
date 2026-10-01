//! What a score on a partial holder measures the collection by (ADR-0103).
//!
//! This node's search index describes the shards it holds. The rest of the
//! collection is counted by each lacked shard's leader, through the function
//! that leader's index writer analyses a record with, and added to it.

use tessari_constants::GATHER_RECORDS;
use tessari_storage::{IndexDefinition, SearchCounts, Transaction};
use tessari_types::{IndexId, TableId};

use super::{Asked, Gathered, Unanswered, window_of};
use crate::error::Result;
use crate::evaluate::Part;
use crate::session::Session;

/// What a node asks a shard's leader to count for a score: the search index,
/// and the terms the score weighs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Counting {
    /// The search index whose field is counted.
    pub index: IndexId,
    /// The analysed terms whose holders are counted, in the order asked.
    pub terms: Vec<String>,
}

impl Session<'_> {
    /// The figures the shards of `table` this node lacks hold for `index`,
    /// summed — `None` when it holds the whole table.
    ///
    /// Refuses as a gathered read does: `NotHeldHere` where no gatherer may be
    /// asked, and `NotGathered` when a leader does not count. A score measured
    /// against part of the collection is never offered as one measured against
    /// all of it.
    pub(crate) fn counted_elsewhere(
        &self,
        transaction: &mut Transaction<'_>,
        table: TableId,
        index: &IndexDefinition,
        terms: &[String],
    ) -> Result<Option<SearchCounts>> {
        let Some(missing) = self.missing(transaction, table, Part::Whole)? else {
            return Ok(None);
        };
        let (Some(gatherer), Some(map)) = (self.gather.as_ref(), missing.map.as_ref()) else {
            return Err(self.not_held_here(&missing)?);
        };
        let counting = Counting {
            index: index.id,
            terms: terms.to_vec(),
        };
        let mut total = SearchCounts {
            holding: vec![0; terms.len()],
            ..SearchCounts::default()
        };
        for span in map
            .spans()
            .filter(|span| missing.lacking.contains(&span.id))
        {
            let Some(window) = window_of(&span, Part::Whole) else {
                continue;
            };
            let asked = Asked {
                namespace: missing.namespace,
                database: missing.database,
                table,
                shard: span.id,
                window,
                most: GATHER_RECORDS,
                pushed: None,
                enough: None,
                reduce: None,
                ordered: None,
                counting: Some(&counting),
            };
            let counted = match gatherer.gather(&asked) {
                Ok(Gathered {
                    counted: Some(counted),
                    ..
                }) if counted.holding.len() == terms.len() => counted,
                Ok(_) => {
                    return Err(missing.unanswered(
                        span.id,
                        Unanswered::Refused("the leader did not count the shard".to_owned()),
                        None,
                    ));
                }
                Err(why) => return Err(self.unanswered(&missing, span.id, why)?),
            };
            total.documents = total.documents.saturating_add(counted.documents);
            total.tokens = total.tokens.saturating_add(counted.tokens);
            for (held, more) in total.holding.iter_mut().zip(counted.holding) {
                *held = held.saturating_add(more);
            }
        }
        Ok(Some(total))
    }
}
