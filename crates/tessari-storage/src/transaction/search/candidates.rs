use super::*;

impl Transaction<'_> {
    /// The records one term is posted against.
    ///
    /// The candidate set a ranked read enumerates, one term at a time rather
    /// than as a union, because which terms are worth enumerating is decided
    /// between them — a term whose whole contribution cannot reach the answer's
    /// running threshold is one whose postings are never read.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or a key cannot be decoded.
    pub fn records_with_term(
        &self,
        index: &IndexDefinition,
        term: &str,
    ) -> Result<BTreeSet<RecordId>> {
        let address = IndexAddress::new(index.namespace, index.database, index.table, index.id);
        self.postings(&address, term)
    }

    /// What this index says one term does in one record.
    ///
    /// A **point read** of the posting, and the two numbers a score needs about
    /// the record it is scoring: how often the record holds the term, and how
    /// long the record's analysed field is. The writer knew both, and wrote both
    /// beside the membership they qualify (see [`Posting`]).
    ///
    /// `None` is the index not posting this record against this term — which for
    /// a score is the term contributing nothing, the same answer re-reading the
    /// record's text would reach by finding no occurrence of it.
    ///
    /// [`Posting::Membership`] is a posting written before the payload existed.
    /// It says the term is in the record and no more, so a caller that needs the
    /// numbers has to reach them another way; this method reports the distinction
    /// rather than resolving it, because only the caller knows what it can fall
    /// back to.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or the stored value cannot be
    /// decoded.
    pub fn posting(
        &self,
        index: &IndexDefinition,
        term: &str,
        id: &RecordId,
    ) -> Result<Option<Posting>> {
        let address = IndexAddress::new(index.namespace, index.database, index.table, index.id);
        let encoded = IndexValues::of(&[Value::from(term)]);
        let key = PostingKey::new(address, encoded, id.clone()).encode();
        match self.store.backend().get(PostingKey::keyspace(), &key)? {
            Some(bytes) => Ok(Some(Posting::decode(bytes.as_slice())?)),
            None => Ok(None),
        }
    }

    /// Where one term sits in one record, as a `POSITIONS` / `OFFSETS` index
    /// stored it — `None` when the record holds no posting for the term, and
    /// empty lists when the index keeps neither (ADR-0100 D4).
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or the stored value cannot be
    /// decoded.
    pub fn located(
        &self,
        index: &IndexDefinition,
        term: &str,
        id: &RecordId,
    ) -> Result<Option<Located>> {
        let address = IndexAddress::new(index.namespace, index.database, index.table, index.id);
        let encoded = IndexValues::of(&[Value::from(term)]);
        let key = PostingKey::new(address, encoded, id.clone()).encode();
        match self.store.backend().get(PostingKey::keyspace(), &key)? {
            Some(bytes) => Ok(Some(Posting::located(bytes.as_slice())?)),
            None => Ok(None),
        }
    }

    /// The record ids the index entries point at, unconfirmed.
    ///
    /// A **complete** lookup on a unique index is a point read, because that is
    /// what unique means. Everything else is a prefix scan — including a leading
    /// lookup on a unique composite index, where one value of the first field
    /// may have many entries and a point read would find none of them.
    pub(in crate::transaction) fn candidates(
        &self,
        index: &IndexDefinition,
        address: &IndexAddress,
        values: &[Value],
        wanted: &[u8],
        complete: bool,
    ) -> Result<Vec<RecordId>> {
        if index.unique {
            if complete {
                let key = UniqueIndexKey::new(*address, IndexValues::of(values)).encode();
                let found = self.store.backend().get(UniqueIndexKey::keyspace(), &key)?;
                return found
                    .map(|bytes| Ok(IndexTarget::decode(bytes.as_slice())?.id))
                    .transpose()
                    .map(Vec::from_iter);
            }
            let mut prefix = address.prefix(KeyKind::UniqueIndex);
            prefix.extend_from_slice(wanted);
            let mut found = Vec::new();
            self.walk_range(
                UniqueIndexKey::keyspace(),
                &KeyRange::prefix(&prefix),
                |_, value| {
                    found.push(IndexTarget::decode(value.as_slice())?.id);
                    Ok(())
                },
            )?;
            return Ok(found);
        }

        let mut prefix = address.prefix(KeyKind::SecondaryIndex);
        prefix.extend_from_slice(wanted);
        let mut found = Vec::new();
        self.walk_range(
            SecondaryIndexKey::keyspace(),
            &KeyRange::prefix(&prefix),
            |key, _| {
                found.push(SecondaryIndexKey::decode(key.as_slice())?.id);
                Ok(())
            },
        )?;
        Ok(found)
    }

    /// The record's payload, if it exists at the snapshot and one of its entries
    /// begins with `wanted`.
    pub(in crate::transaction) fn confirm(
        &self,
        index: &IndexDefinition,
        address: &RecordAddress,
        wanted: &[u8],
    ) -> Result<Option<Vec<u8>>> {
        let Some(payload) = self.get(address)? else {
            return Ok(None);
        };
        let value = decode_payload(&payload)?;
        // The confirmation is what makes an index unable to change an answer: an
        // entry is a claim about a record, and this asks the record. Two things
        // widen it beyond equality and neither loosens it. A multi-valued route
        // gives a record several entries, so the claim is about **any** of them.
        // And a leading lookup asks about the first *k* values, so the claim is
        // that an entry **begins** with them — which for a complete lookup is
        // equality, because an index has a fixed arity.
        if crate::index::project(index, &value)
            .iter()
            .any(|held| held.as_slice().starts_with(wanted))
        {
            return Ok(Some(payload));
        }
        Ok(None)
    }
}
