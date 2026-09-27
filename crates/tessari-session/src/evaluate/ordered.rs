//! Index reads that answer in the order an index keeps.

use tessari_constants::ORDERED_FILTER_REACH;
use tessari_ql::Expr;
use tessari_storage::{Catalog, RecordAddress, Transaction};
use tessari_types::{RecordId, TableId};

use crate::condition::boolean;
use crate::error::{Error, Result};
use crate::plan;
use crate::session::Session;

use super::{ORDERED_LEADING_FIELDS, Scope, Walked};

impl Session<'_> {
    /// A bounded descending read, taken from an index that is already in that
    /// order.
    ///
    /// `None` is the scan, and every `None` below is a way the store — rather
    /// than the statement, which [`plan::descending`] has already judged — makes
    /// the order the index holds differ from the order the read must answer in:
    ///
    /// - **no ordered index on that field.** A search index holds terms and a
    ///   vector index holds a graph; neither is stored in this order.
    ///   A composite index **is** taken when the ordered field is its leading
    ///   one, because its entries are stored by that field before anything else.
    ///   What changes with it is the tie group: the entries sharing one leading
    ///   value are ordered by the *next* indexed field rather than by the
    ///   record's identity, so the group at the bound is drained in **both**
    ///   directions. The group's edge is read off the key without decoding it —
    ///   [`tessari_encoding::IndexValues`] cannot be reversed, but two entries
    ///   agree on their leading values exactly when those bytes are equal, and
    ///   agreement is the only thing a tie test asks.
    /// - **the field is not visible to this caller.** A field permission removes
    ///   the field *before* anything looks at the record, so today a caller
    ///   without it sorts by `none` and gets identity order. An ordering taken
    ///   from the index would sort by the values themselves — the order
    ///   disclosing what the projection hides, one comparison at a time.
    /// - **this transaction has written to the table.** Entries are derived at
    ///   commit, so an uncommitted record has none and the index cannot place it.
    /// - **the snapshot is not the committed tail.** Entries hold the current
    ///   state and carry no version, so a record changed since the snapshot sits
    ///   in the index under a value this reader cannot see — and the answer that
    ///   comes back is not short, it is in the **wrong order**, with the record
    ///   placed where its newer value belongs. A condition served by an index
    ///   survives that because its candidates are re-tested against the record;
    ///   an ordering has nothing to re-test, since the entry's position is the
    ///   answer. The storage suite demonstrates it rather than this sentence
    ///   asserting it.
    ///
    /// The last `None` is the index running out before the bound was filled,
    /// which is the answer needing records the index does not hold.
    pub(super) fn walk_in_order(
        &self,
        transaction: &mut Transaction<'_>,
        context: crate::context::Context,
        table: TableId,
        wanted: &plan::Bounded<'_>,
    ) -> Result<Walked> {
        let Some((index, visible)) =
            self.index_serving_order(transaction, context, table, wanted.path, wanted.descending)?
        else {
            return Ok(Walked::NotServed);
        };
        // The two directions differ in what a short walk *means*, which is why
        // one returns an option and the other does not. Descending, an index
        // that runs out is missing the records whose value is absent — they sort
        // below everything it holds, so the answer needs them and the caller
        // scans. Ascending is admitted only where there are no absences, so an
        // index that runs out has answered the whole table and a short answer is
        // a complete one.
        let found = if wanted.descending {
            match transaction.records_in_descending_order(
                &index,
                ORDERED_LEADING_FIELDS,
                wanted.wanted,
            )? {
                Some(found) => found,
                None => return Ok(Walked::Declined),
            }
        } else {
            transaction.records_in_ascending_order(&index, ORDERED_LEADING_FIELDS, wanted.wanted)?
        };
        Ok(Walked::Served {
            found: self.records_of(found, &visible)?,
            index: index.name,
        })
    }

    /// A bounded ordered read **under a condition**, taken from the index that
    /// holds the order.
    ///
    /// `None` means the read is not served this way and the caller narrows and
    /// sorts, which is what it did before this existed.
    ///
    /// # The trap, and the whole of why this is not a call site
    ///
    /// An index narrows and the **condition decides** — every candidate is
    /// re-tested against the whole of it above the source. So a walk that filled
    /// the caller's bound with ten *entries* can answer with fewer than ten
    /// *records*, because some of them fail that test. Not an error, not a
    /// crash: real records, fewer of them, returned confidently. That is the
    /// same failure class as a limit pushed past a clause that changes the
    /// count, and it is why this asks for more until enough **survive** rather
    /// than until enough are read.
    ///
    /// # Why taking the first `wanted` survivors of the top `k` is the answer
    ///
    /// The walk yields records in index order, and that **is** the sort order —
    /// the index and the sort use one order, the value system's. So no record
    /// outside the top `k` can rank above one inside it, and the first `wanted`
    /// survivors of the top `k` are the first `wanted` survivors of the whole
    /// table.
    ///
    /// Absences are the one case that could break that argument, and the
    /// direction decides whether they can. A record with no value for the key
    /// has **no index entry**: descending it sorts *last*, so it is never near
    /// the top of the order and the walk cannot miss it; ascending it sorts
    /// *first*, which is exactly where the answer begins. So ascending is served
    /// only where there are no absences, and `REQUIRED` is that guarantee —
    /// [`Self::index_serving_order`] asks for it, and asks for it here by
    /// passing the bound's own direction rather than a constant. Both facts are
    /// inherited from the unconditioned case rather than re-derived.
    ///
    /// # Running out means opposite things in the two directions
    ///
    /// Descending, an index that cannot fill `asking` is missing the records
    /// whose value is absent — they sort below every entry it holds, so the
    /// answer needs them and the read gives the order up. Ascending over a
    /// `REQUIRED` field there are no such records, so a walk that runs out has
    /// read the **whole table** through the index: what survived the condition
    /// is the complete answer, and handing it back to the scan would read the
    /// same table a second time to reach the same records.
    ///
    /// # The ceiling bounds the cost and never the answer
    ///
    /// How far past the bound the walk must go depends on how selective the
    /// condition is over the order — the distribution statistic this store
    /// deliberately does not keep. Past [`ORDERED_FILTER_REACH`] multiples of
    /// the bound, the order is not worth serving from the index and the read
    /// falls back to the scan it would have taken anyway. Every exit is either
    /// an ordered answer that filled the bound or the scan; there is no exit
    /// that answers short.
    ///
    /// The answer may be **longer** than the bound, which is correct and
    /// deliberate: it is in order, and `shape::bounded` takes the window the
    /// statement asked for, as it does for every other path.
    pub(super) fn walk_matching(
        &self,
        transaction: &mut Transaction<'_>,
        context: crate::context::Context,
        table: TableId,
        wanted: &plan::Bounded<'_>,
        condition: &Expr,
        scope: Scope<'_>,
    ) -> Result<Walked> {
        let Some((index, visible)) =
            self.index_serving_order(transaction, context, table, wanted.path, wanted.descending)?
        else {
            return Ok(Walked::NotServed);
        };
        let ceiling = wanted.wanted.saturating_mul(ORDERED_FILTER_REACH);
        let mut asking = wanted.wanted;
        loop {
            // Descending, `None` is the index unable to fill `asking` — it has
            // run out of entries, and the records that would fill the rest of
            // the answer are ones it does not hold. The scan is the read that
            // can find those. Ascending there are none of them, so running out
            // is the end of the table rather than a hole in the answer, and a
            // walk shorter than it asked for says so.
            let (found, exhausted) = if wanted.descending {
                let Some(found) = transaction.records_in_descending_order(
                    &index,
                    ORDERED_LEADING_FIELDS,
                    asking,
                )?
                else {
                    return Ok(Walked::Declined);
                };
                (found, false)
            } else {
                let found = transaction.records_in_ascending_order(
                    &index,
                    ORDERED_LEADING_FIELDS,
                    asking,
                )?;
                let exhausted = found.len() < asking;
                (found, exhausted)
            };
            let mut matched = Vec::new();
            for (id, record) in self.records_of(found, &visible)? {
                let held = self.evaluate_in(transaction, condition, scope.with(&id, &record))?;
                if boolean(&held, condition.span)? {
                    matched.push((id, record));
                }
            }
            if matched.len() >= wanted.wanted || exhausted {
                return Ok(Walked::Served {
                    found: matched,
                    index: index.name,
                });
            }
            if asking >= ceiling {
                return Ok(Walked::Declined);
            }
            // Doubling, so reaching the ceiling costs about twice the ceiling in
            // entries rather than a walk per step.
            asking = asking.saturating_mul(2).min(ceiling);
        }
    }

    /// The index that may serve this order, if one may — and what the caller may
    /// see of the table, which deciding that had to read anyway.
    ///
    /// Everything above [`Self::descend`]'s list except the bound filling, which
    /// only the read itself can know. It is one function because `EXPLAIN` asks
    /// the same question and a second implementation would answer it correctly
    /// until the day one of them changed.
    ///
    /// The visible set travels back rather than being read again: it is a
    /// catalog read per statement, and the read that follows needs the same one.
    pub(crate) fn index_serving_order(
        &self,
        transaction: &mut Transaction<'_>,
        context: crate::context::Context,
        table: TableId,
        path: &tessari_types::Path,
        descending: bool,
    ) -> Result<Option<(tessari_storage::IndexDefinition, crate::redact::Visible)>> {
        let Some(index) = self.index_ordering_on_path(transaction, table, path)? else {
            return Ok(None);
        };
        if !index.is_ordered() {
            return Ok(None);
        }
        // Ascending, the records the index does **not** hold are the ones that
        // come first, so the read is only sound where there are none of them.
        // `REQUIRED` is that guarantee and it holds in both directions in time:
        // the declaration is refused against a table already holding a record
        // without the field, and every write after it is checked.
        //
        // Asked here rather than at each call site, so the executor and
        // `EXPLAIN` cannot come to disagree about which reads are servable —
        // the same reason the other four refusals below live here.
        if !descending && !self.every_record_has(transaction, table, path)? {
            return Ok(None);
        }
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

    /// Whether every record of this table is guaranteed to hold a value at this
    /// route — which is what makes it certain that every record has an index
    /// entry.
    ///
    /// **Only a plain top-level field can answer yes.** `REQUIRED` is declared
    /// on a field, so it says nothing about what lives *inside* one: a required
    /// `address` does not promise an `address.city`, and a route with steps
    /// below its root is therefore refused rather than approximated.
    ///
    /// # Errors
    ///
    /// Returns an error when the catalog cannot be read.
    pub(super) fn every_record_has(
        &self,
        transaction: &mut Transaction<'_>,
        table: TableId,
        path: &tessari_types::Path,
    ) -> Result<bool> {
        if !path.steps().is_empty() {
            return Ok(false);
        }
        Ok(Catalog::new(transaction)
            .fields_on(table)?
            .iter()
            .any(|field| field.name == path.root() && field.required))
    }

    /// Run the candidate the plan chose.
    ///
    /// Every arm has everything it needs on the candidate — the value, the
    /// literal prefix, the analysed terms — because [`crate::plan`] computed
    /// them while ranking. There is nothing here to recompute and no shape that
    /// can arrive without its argument.
    ///
    /// The one thing that does not travel on the candidate is the field's
    /// analyzer, and the two term reads need it: they settle this transaction's
    /// own writes by re-deriving each pending record's terms, and re-deriving
    /// them with a different analyzer than the query was built with would make
    /// the two halves of one read disagree. It is passed rather than looked up
    /// again for exactly that reason.
    pub(super) fn serve(
        &self,
        transaction: &mut Transaction<'_>,
        context: crate::context::Context,
        table: TableId,
        chosen: &plan::Candidate,
        analyzer: Option<&tessari_types::Analyzer>,
    ) -> Result<Vec<(RecordId, Vec<u8>)>> {
        match &chosen.served {
            plan::Served::Equality(values) => transaction.records_by_index(&chosen.index, values),
            plan::Served::Prefix(prefix) => {
                transaction.records_with_string_prefix(&chosen.index, prefix)
            }
            plan::Served::Range {
                fixed,
                lower,
                upper,
            } => transaction.records_in_range(&chosen.index, fixed, lower.as_ref(), upper.as_ref()),
            plan::Served::Region {
                cells,
                bounds,
                relation,
            } => {
                // The filter half. What comes back is a **candidate set** — the
                // cells are coarser than the boxes and the boxes are coarser
                // than the shapes — and the condition above refines it against
                // the real geometry, as it does for every other index read here.
                //
                // The counts the read measured are dropped on this path and that
                // is deliberate rather than an oversight: this store has no
                // statement that runs a read and reports its cost, so there is
                // nowhere truthful to put them yet. They are returned, asserted
                // by the tests that gate the query budget, and will surface here
                // when an analysing `EXPLAIN` exists to carry them.
                transaction
                    .records_in_region(&chosen.index, cells, *bounds, *relation)
                    .map(|region| region.rows)
            }
            plan::Served::Terms(terms) => {
                let mut rows = Vec::new();
                for id in transaction.records_by_terms(&chosen.index, analyzer, terms)? {
                    let at = RecordAddress::new(context.namespace, context.database, table, id);
                    if let Some(payload) = transaction.get(&at)? {
                        rows.push((at.id, payload));
                    }
                }
                Ok(rows)
            }
            // One arm for three variants: a union of posting lists per group,
            // intersected across groups, is one read however the groups were
            // arrived at — a prefix walk, a fuzzy walk, or the `OR`s somebody
            // wrote. They stay separate variants so `EXPLAIN` can still say
            // which question produced them.
            plan::Served::PrefixTerms(expansions)
            | plan::Served::FuzzyTerms(expansions)
            | plan::Served::AnyTerms(expansions) => {
                let mut rows = Vec::new();
                for id in transaction.records_by_expansions(&chosen.index, analyzer, expansions)? {
                    let at = RecordAddress::new(context.namespace, context.database, table, id);
                    if let Some(payload) = transaction.get(&at)? {
                        rows.push((at.id, payload));
                    }
                }
                Ok(rows)
            }
        }
        .map_err(Error::from)
    }
}
