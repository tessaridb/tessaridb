//! Reads served by a vector or spatial index, and what those indexes measure.

use super::{RecordAddress, Transaction};
use crate::catalog::IndexDefinition;
use crate::error::Result;
use std::collections::BTreeMap;
use tessari_encoding::{
    IndexAddress, IndexValues, SpatialRefinement, SpatialRefinementKey, StoreKey, StoreValue,
    VectorRecall, VectorRecallKey,
};
use tessari_types::{RecordId, Value};

impl Transaction<'_> {
    /// The records a vector index says are nearest, nearest first.
    ///
    /// **Approximate**, and the only method on this type that is. A navigable
    /// graph returns the neighbours a greedy walk found, and showing that it
    /// missed none would mean the scan the index exists to avoid — which is why
    /// the language makes a statement ask for this before it may be used.
    ///
    /// A candidate set like every index read: each record is resolved at the
    /// reader's own snapshot, so a node left behind by a deleted record can
    /// never produce a row.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or a node cannot be decoded.
    /// `effort` is the walk's budget: `None` for the engine's own, `Some` for a
    /// budget the read named with `APPROXIMATE EFFORT n`.
    pub fn records_by_vector(
        &self,
        index: &IndexDefinition,
        query: &[f64],
        wanted: usize,
        effort: Option<usize>,
    ) -> Result<Vec<RecordId>> {
        let Some(distance) = index.vector else {
            return Ok(Vec::new());
        };
        let address = IndexAddress::new(index.namespace, index.database, index.table, index.id);
        let graph = crate::graph::Graph::read(self.store, &address, distance)?;
        Ok(graph.nearest(query, wanted, effort))
    }

    /// The recall this vector index was last measured at, if it ever was.
    ///
    /// `None` means nobody has measured — an index is measured when it is built,
    /// so a store filled by writes since its last build reports the figure from
    /// that build, and one never built reports nothing. That is the honest
    /// answer and the reason the figure carries `records`: a reader can see the
    /// store has outgrown the number.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or the stored value cannot be
    /// decoded.
    pub fn vector_recall(&self, index: &IndexDefinition) -> Result<Option<VectorRecall>> {
        let address = IndexAddress::new(index.namespace, index.database, index.table, index.id);
        let key = VectorRecallKey::new(address).encode();
        match self
            .store
            .backend()
            .get(VectorRecallKey::keyspace(), &key)?
        {
            Some(bytes) => Ok(Some(VectorRecall::decode(bytes.as_slice())?)),
            None => Ok(None),
        }
    }

    /// What refining this spatial index's candidates last cost, if it was ever
    /// measured.
    ///
    /// `None` means nobody has measured, and it covers two cases a reader should
    /// not have to tell apart: an index never built, and one whose records never
    /// reach one another so there was no refinement to observe. Both are the
    /// absence of a measurement rather than a measurement of nothing, which is
    /// why neither is reported as a ratio.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or the stored value cannot be
    /// decoded.
    pub fn spatial_refinement(&self, index: &IndexDefinition) -> Result<Option<SpatialRefinement>> {
        let address = IndexAddress::new(index.namespace, index.database, index.table, index.id);
        let key = SpatialRefinementKey::new(address).encode();
        match self
            .store
            .backend()
            .get(SpatialRefinementKey::keyspace(), &key)?
        {
            Some(bytes) => Ok(Some(SpatialRefinement::decode(bytes.as_slice())?)),
            None => Ok(None),
        }
    }

    /// The records an index says hold `values`, as of this transaction's
    /// snapshot.
    ///
    /// # Sound, and not complete, at an older snapshot
    ///
    /// Index entries hold the **current** state — they carry no version, and an
    /// update removes the entry for the value it replaced. This method therefore
    /// treats them as candidates and confirms each one by re-deriving the
    /// record's indexed values at the reader's own snapshot, so a stale entry
    /// can never produce a row that does not match.
    ///
    /// What it cannot do is find a record that held `values` at the snapshot and
    /// has since changed: its entry is gone, so there is no candidate to
    /// confirm. A reader at the latest committed state is exact; an older one
    /// gets no wrong rows and may get fewer.
    ///
    /// Uncommitted writes of this transaction participate, because entries are
    /// derived at commit and a writer would otherwise be unable to find what it
    /// just wrote.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or stored bytes cannot be
    /// decoded.
    pub fn records_by_index(
        &self,
        index: &IndexDefinition,
        values: &[Value],
    ) -> Result<Vec<(RecordId, Vec<u8>)>> {
        let address = IndexAddress::new(index.namespace, index.database, index.table, index.id);
        // The **leading** bytes, not the complete encoding, and one rule for
        // both cases. A complete encoding ends with a marker a longer key does
        // not carry in that position, so it is not a byte-prefix of a composite
        // index's key — which is why a composite index used to be offered for
        // nothing at all while being maintained on every write.
        //
        // For a complete lookup the leading bytes are the complete ones minus
        // that marker, and since an index has a fixed arity, "the record's entry
        // begins with these bytes" is equality there and a leading match here.
        let wanted = IndexValues::leading(values);
        let complete = values.len() == index.fields.len();

        let mut found: BTreeMap<RecordId, Vec<u8>> = BTreeMap::new();
        for id in self.candidates(index, &address, values, &wanted, complete)? {
            let record = RecordAddress::new(index.namespace, index.database, index.table, id);
            if let Some(payload) = self.confirm(index, &record, &wanted)? {
                found.insert(record.id, payload);
            }
        }

        // A record this transaction wrote has no entry yet, and one it changed
        // still has the entry for its former value. Both are settled by asking
        // the pending write itself.
        for pending in self.writes.keys() {
            if pending.namespace != index.namespace
                || pending.database != index.database
                || pending.table != index.table
            {
                continue;
            }
            match self.confirm(index, pending, &wanted)? {
                Some(payload) => {
                    found.insert(pending.id.clone(), payload);
                }
                None => {
                    found.remove(&pending.id);
                }
            }
        }
        Ok(found.into_iter().collect())
    }
}
