use super::*;

impl Transaction<'_> {
    /// What a search index knows about its collection as a whole.
    ///
    /// An index that has never been written to has no statistics key, and the
    /// answer is the empty collection rather than an error: nothing is wrong
    /// with an index over no documents, and a caller ranking against one gets
    /// the same score for every record because that is the true answer.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or the stored value cannot be
    /// decoded.
    pub fn search_statistics(&self, index: &IndexDefinition) -> Result<SearchStatistics> {
        let address = IndexAddress::new(index.namespace, index.database, index.table, index.id);
        let key = SearchStatisticsKey::new(address).encode();
        match self
            .store
            .backend()
            .get(SearchStatisticsKey::keyspace(), &key)?
        {
            Some(bytes) => Ok(SearchStatistics::decode(bytes.as_slice())?),
            None => Ok(SearchStatistics::default()),
        }
    }

    /// A search member's statistics: the collection's two numbers and each
    /// field's token total, in the member's field order (ADR-0105).
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or the stored value cannot be
    /// decoded.
    pub fn member_statistics(
        &self,
        index: &IndexDefinition,
    ) -> Result<(SearchStatistics, Vec<u64>)> {
        let address = IndexAddress::new(index.namespace, index.database, index.table, index.id);
        let key = SearchStatisticsKey::new(address).encode();
        match self
            .store
            .backend()
            .get(SearchStatisticsKey::keyspace(), &key)?
        {
            Some(bytes) => Ok(SearchStatistics::fielded(bytes.as_slice())?),
            None => Ok((SearchStatistics::default(), Vec::new())),
        }
    }

    /// The records a search member nominates: for every group, a record posted
    /// against at least one of its terms — and every record this transaction
    /// wrote on the member's table, whose postings do not exist yet.
    ///
    /// A **candidate set and nothing more** (ADR-0105): the caller re-tests each
    /// record against the whole query on the text it may read, so a pending
    /// write nominated here that no longer matches is dropped there, and a
    /// deleted one is not found to test.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or a key cannot be decoded.
    pub fn member_candidates(
        &self,
        index: &IndexDefinition,
        groups: &[Vec<String>],
    ) -> Result<BTreeSet<RecordId>> {
        let address = IndexAddress::new(index.namespace, index.database, index.table, index.id);
        let mut holding: Option<BTreeSet<RecordId>> = None;
        for group in groups {
            let found = self.union(&address, group)?;
            let narrowed = match holding {
                None => found,
                Some(mut held) => {
                    held.retain(|id| found.contains(id));
                    held
                }
            };
            let empty = narrowed.is_empty();
            holding = Some(narrowed);
            if empty {
                break;
            }
        }
        let mut holding = holding.unwrap_or_default();
        for pending in self.writes.keys() {
            if pending.namespace == index.namespace
                && pending.database == index.database
                && pending.table == index.table
            {
                holding.insert(pending.id.clone());
            }
        }
        Ok(holding)
    }

    /// How many documents this index posts the term against.
    ///
    /// The number a ranking weighs a term by. It is read from the term's
    /// dictionary entry — a **point read** — and only counted from the postings
    /// when there is no entry to read.
    ///
    /// # Why there are two paths and why the second one is not a fallback in the
    /// usual sense
    ///
    /// This was a count of the term's whole posting range: not materialised, but
    /// still a walk proportional to how many records hold the word, performed
    /// once per query term per query. On a common word in a large table that is
    /// the dominant cost of a ranked read, and it is spent to arrive at one
    /// integer the writer already knew.
    ///
    /// The dictionary holds that integer. An index written before the dictionary
    /// existed has none, and its terms have no entries — so the count is what
    /// answers there, and such an index keeps ranking correctly at the old cost
    /// rather than reporting every term as unheld. The two paths agree by
    /// construction: the entry is maintained in the same batch as the postings it
    /// counts, so a discrepancy is not a state this store can reach.
    ///
    /// A term nobody holds has no entry either, and the count it falls through to
    /// is a walk of an empty range — the cheapest read in the store.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or a stored entry cannot be
    /// decoded.
    pub fn document_frequency(&self, index: &IndexDefinition, term: &str) -> Result<u64> {
        Ok(self.term_statistics(index, term)?.documents)
    }

    /// The term's whole dictionary entry — its frequency and, when the entry
    /// carries them, the extremes an upper bound is scored from.
    ///
    /// The same point read [`Transaction::document_frequency`] makes, reported
    /// without discarding the rest of what it read. A caller that prunes needs
    /// both, and reading the key twice to get them would spend the saving the
    /// dictionary exists for.
    ///
    /// The fall-through is the same one and means the same thing: no entry is an
    /// index written before the dictionary existed, and the count answers there.
    /// What it cannot answer is the extremes, so the statistics come back
    /// **unbounded** — which obliges a caller to score the term's postings
    /// rather than prune them (see [`TermStatistics::bound`]).
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or a stored entry cannot be
    /// decoded.
    pub fn term_statistics(&self, index: &IndexDefinition, term: &str) -> Result<TermStatistics> {
        let address = IndexAddress::new(index.namespace, index.database, index.table, index.id);
        let encoded = IndexValues::of(&[Value::from(term)]);
        let key = SearchTermKey::new(address, encoded.clone()).encode();
        if let Some(bytes) = self.store.backend().get(SearchTermKey::keyspace(), &key)? {
            return Ok(TermStatistics::decode(bytes.as_slice())?);
        }
        let prefix = PostingKey::term_prefix(&address, &encoded);
        let counted = self
            .store
            .backend()
            .count(PostingKey::keyspace(), &KeyRange::prefix(&prefix))?;
        Ok(TermStatistics::new(counted))
    }
}
