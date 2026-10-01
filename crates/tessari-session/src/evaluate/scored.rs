//! Index reads that answer in score order or at a place.

use std::collections::BTreeMap;

use tessari_encoding::Posting;
use tessari_storage::{Catalog, RecordAddress, Transaction};
use tessari_types::{RecordId, TableId};

use crate::error::Result;
use crate::plan;
use crate::rank::{self};
use crate::session::Session;

use super::Walked;

impl Session<'_> {
    /// A bounded read ordered by how well a record answers a query.
    ///
    /// # The candidate set is the answer's, and the rest of the table is pruned
    ///
    /// A record holding none of the query's words scores zero, so the records
    /// that can fill a `LIMIT k` are the ones the index posts against at least
    /// one of those words. The scan reaches the same answer by scoring every
    /// record in the table and discarding the zeros.
    ///
    /// Which terms are worth enumerating is then decided between them, and this
    /// is where the stored extremes earn their keep. The terms are taken in
    /// descending order of the most they can contribute, and the remaining
    /// suffix is abandoned once its **combined** bound falls below the score
    /// already held in `k`th place: a record holding only those terms cannot
    /// score above that suffix's sum, so it cannot reach the answer, so its
    /// postings are never read. On a query pairing a rare word with a common one
    /// that is the common word's whole posting list.
    ///
    /// The threshold is strict — `sum < kth` rather than `<=` — so a record the
    /// suffix excludes scores strictly below the `k`th, and cannot tie with it
    /// either.
    ///
    /// # What it hands back, and why not the top `k`
    ///
    /// The whole candidate set, for the ordering stage to sort exactly as it
    /// sorts a scan's. Selecting the `k` best here would mean implementing that
    /// stage's comparison a second time — and it is a **total** order, ties
    /// broken by record id (see [`crate::shape`]), precisely so that which access
    /// path ran cannot reorder equal rows. A second copy of it would agree until
    /// somebody edited one of them.
    ///
    /// Which is also why the order these candidates are produced in does not
    /// matter, and why nothing here sorts them.
    ///
    /// # Declining
    ///
    /// Fewer candidates than the read wants means the answer is filled out with
    /// records that hold none of the query's words. Their order among themselves
    /// is the scan's, so the scan is the read that can produce it.
    pub(super) fn walk_scored(
        &self,
        transaction: &mut Transaction<'_>,
        context: crate::context::Context,
        table: TableId,
        wanted: &plan::Scored<'_>,
        searched: &crate::search::Searched,
    ) -> Result<Walked> {
        // A read wanting nothing has nothing to prune against — `best[0]` below
        // is the score in last place, and there is no last place in an empty
        // answer.
        if wanted.wanted == 0 {
            return Ok(Walked::NotServed);
        }
        let Some(ranked) = searched.ranked(wanted.field) else {
            return Ok(Walked::NotServed);
        };
        let Some(visible) = self.index_serving_score(transaction, context, table, wanted.field)?
        else {
            return Ok(Walked::NotServed);
        };
        let corpus = &ranked.corpus;
        // A starred word's expansions have no bound of their own to prune by —
        // they are weighed as one term (ADR-0104) — so the read goes back to the
        // scan, which scores every record exactly.
        if !corpus.blends.is_empty() {
            return Ok(Walked::NotServed);
        }
        // A page after the first resumes below its anchor's score (ADR-0100
        // D1.9). Only a record scoring strictly below the anchor is certainly
        // past the cursor — a tie may sit on either side of it — so only those
        // set the pruning threshold and count towards filling the page. That
        // threshold is never above the true one, so the walk can only read more
        // than it needs, never less; the ordering stage then applies the cursor
        // to the candidates exactly as it does to a scan's records.
        let ceiling = match wanted.after {
            None => None,
            Some(anchor) => {
                let (_, address) = self.address(transaction, anchor)?;
                // An anchor in another table, or gone, is the scan's to judge:
                // it raises the refusal that says so.
                if address.table != table || transaction.get(&address)?.is_none() {
                    return Ok(Walked::NotServed);
                }
                let Some(held) =
                    self.scored_from_postings(transaction, ranked, corpus, &address.id)?
                else {
                    return Ok(Walked::NotServed);
                };
                // An anchor scoring zero leaves nothing strictly below it, so the
                // walk declines and the scan reads the records holding none of
                // the words.
                Some(rank::scored(corpus, &held))
            }
        };

        // A term written twice in a query weighs twice, so its bound is twice as
        // large. Counting the multiset here rather than deduplicating it keeps
        // the bound above the score it bounds.
        let mut terms = Vec::new();
        for term in corpus.terms.keys() {
            let repeats = corpus.asked.iter().filter(|asked| *asked == term).count();
            // A term this build cannot bound is a term the walk may not prune,
            // and no suffix containing it is bounded either. Handing the read
            // back rather than walking the postings unpruned is not caution: a
            // union of posting lists is read one record at a time, and over a
            // word most of the table holds that is measurably **more** work than
            // the scan it was standing in for.
            let Some(one) = rank::bound(corpus, term) else {
                return Ok(Walked::NotServed);
            };
            terms.push((term, one * as_count(repeats)));
        }
        terms.sort_by(|left, right| right.1.total_cmp(&left.1));
        // `suffix[at]` is the most everything from `at` onward could contribute,
        // accumulated from the back so the walk can ask, at each term, what the
        // whole remaining tail is worth.
        let mut suffix = Vec::with_capacity(terms.len().saturating_add(1));
        let mut running = 0.0_f64;
        suffix.push(running);
        for (_, one) in terms.iter().rev() {
            running += one;
            suffix.push(running);
        }
        suffix.reverse();

        let mut candidates = BTreeMap::new();
        let mut best: Vec<f64> = Vec::new();
        let mut past = 0_usize;
        for (at, (term, _)) in terms.iter().enumerate() {
            if best.len() >= wanted.wanted && suffix[at] < best[0] {
                break;
            }
            for id in transaction.records_with_term(&ranked.index, term)? {
                let address = RecordAddress::new(context.namespace, context.database, table, id);
                let Some(payload) = transaction.get(&address)? else {
                    continue;
                };
                if candidates.insert(address.id.clone(), payload).is_some() {
                    continue;
                }
                // A posting written before it carried a payload. Its numbers are
                // in the record's text, and re-analysing it here would be the
                // scan's own work paid inside the walk that exists to avoid it —
                // so the read goes back to the scan rather than losing its
                // threshold and finishing as an unpruned union.
                let Some(held) =
                    self.scored_from_postings(transaction, ranked, corpus, &address.id)?
                else {
                    return Ok(Walked::NotServed);
                };
                let score = rank::scored(corpus, &held);
                if ceiling.is_none_or(|anchor| score < anchor) {
                    past = past.saturating_add(1);
                    keep_best(&mut best, score, wanted.wanted);
                }
            }
        }
        if past < wanted.wanted {
            return Ok(Walked::Declined);
        }
        Ok(Walked::Served {
            found: self.records_of(candidates.into_iter().collect(), &visible)?,
            index: ranked.index.name.clone(),
        })
    }

    /// One candidate's score, read from the postings alone.
    ///
    /// `None` is a posting that predates the payload, which says the term is
    /// there and not how often. Only the record's own text answers then, and
    /// re-analysing it here would be the scan's cost paid inside the walk that
    /// exists to avoid it — so the caller stops pruning instead.
    pub(super) fn scored_from_postings(
        &self,
        transaction: &mut Transaction<'_>,
        ranked: &crate::search::Ranked,
        corpus: &rank::Corpus,
        id: &RecordId,
    ) -> Result<Option<rank::Held>> {
        let mut occurrences = BTreeMap::new();
        let mut length = 0_u32;
        for term in corpus.terms.keys() {
            match transaction.posting(&ranked.index, term, id)? {
                None => {}
                Some(Posting::Counted {
                    frequency,
                    length: tokens,
                }) => {
                    occurrences.insert(term.clone(), frequency);
                    length = tokens;
                }
                Some(Posting::Membership) => return Ok(None),
            }
        }
        Ok(Some(rank::Held::counted(occurrences, length)))
    }

    /// Whether a search index can serve a ranked read of this field, and what
    /// the caller may see of the table.
    ///
    /// The corpus resolution already found the index — this asks the questions
    /// that are about the *read* rather than about the field, and it asks them
    /// in one place so the executor and `EXPLAIN` cannot come to disagree, the
    /// same reason [`Evaluator::index_serving_place`] gathers its own.
    ///
    /// # Errors
    ///
    /// Returns an error when the catalog cannot be read.
    pub(crate) fn index_serving_score(
        &self,
        transaction: &mut Transaction<'_>,
        context: crate::context::Context,
        table: TableId,
        path: &tessari_types::Path,
    ) -> Result<Option<crate::redact::Visible>> {
        let visible = self.visible_in(transaction, table)?;
        if visible
            .as_ref()
            .is_some_and(|fields| !fields.contains(path.root()))
        {
            return Ok(None);
        }
        // Uncommitted writes are not in the postings, so a walk over them would
        // answer without the records this transaction itself just wrote.
        if transaction.writes_in(context.namespace, context.database, table) {
            return Ok(None);
        }
        if !transaction.indexes_are_current()? {
            return Ok(None);
        }
        Ok(Some(visible))
    }

    /// The spatial index that may serve an order by distance from this field,
    /// with the caller's field visibility.
    ///
    /// The kind check is not a formality: an index keyed by **values** on the
    /// same field would let a walk over places loose in a keyspace it has no
    /// business in, where it would find nothing, answer with no rows, and report
    /// no error. So the test names what it admits rather than what it rejects,
    /// and a kind added later is refused by default rather than admitted by
    /// omission.
    ///
    /// Asked here rather than at each call site, so the executor and `EXPLAIN`
    /// cannot come to disagree about which reads are servable — the same reason
    /// [`Evaluator::index_serving_order`] gathers its own four.
    ///
    /// # Errors
    ///
    /// Returns an error when the catalog cannot be read.
    pub(crate) fn index_serving_place(
        &self,
        transaction: &mut Transaction<'_>,
        context: crate::context::Context,
        table: TableId,
        path: &tessari_types::Path,
    ) -> Result<Option<(tessari_storage::IndexDefinition, crate::redact::Visible)>> {
        let Some(index) = Catalog::new(transaction)
            .indexes_on(table)?
            .into_iter()
            .find(|index| index.spatial && index.fields.first() == Some(path))
        else {
            return Ok(None);
        };
        let visible = self.visible_in(transaction, table)?;
        if visible
            .as_ref()
            .is_some_and(|fields| !fields.contains(path.root()))
        {
            return Ok(None);
        }
        if transaction.writes_in(context.namespace, context.database, table) {
            return Ok(None);
        }
        if !transaction.indexes_are_current()? {
            return Ok(None);
        }
        Ok(Some((index, visible)))
    }
}

/// Remember one score if it is among the best `wanted` seen so far.
///
/// Kept ascending and capped, so `best[0]` is the score in last place — the
/// threshold a pruning walk compares a term suffix against. A shorter list is a
/// read that has not yet seen enough records to have a last place, which is why
/// the caller checks the length before reading the front.
fn keep_best(best: &mut Vec<f64>, score: f64, wanted: usize) {
    if best.len() >= wanted && score <= best[0] {
        return;
    }
    let at = best.partition_point(|seen| *seen < score);
    best.insert(at, score);
    if best.len() > wanted {
        best.remove(0);
    }
}

/// A count of repeats as a weight, without an `as` cast.
fn as_count(repeats: usize) -> f64 {
    f64::from(u32::try_from(repeats).unwrap_or(u32::MAX))
}
