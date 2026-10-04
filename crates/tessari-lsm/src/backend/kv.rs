use super::*;

impl KvBackend for LsmBackend {
    fn background_errors(&self) -> tessari_kv::Result<u64> {
        Self::background_errors(self)
    }

    fn name(&self) -> &'static str {
        BACKEND_NAME
    }

    fn get(&self, keyspace: Keyspace, key: &Key) -> Result<Option<Value>> {
        let region = self.region(keyspace)?;
        let found = self
            .database
            .get_cf(region, key.as_slice())
            .map_err(|error| from_engine(&error))?;
        Ok(found.map(Value::new))
    }

    fn scan(&self, request: &ScanRequest) -> Result<Vec<(Key, Value)>> {
        self.read_range(request, Caching::Fill)
    }

    fn sweep(&self, request: &ScanRequest) -> Result<Vec<(Key, Value)>> {
        self.read_range(request, Caching::Skip)
    }

    /// Advance an iterator over the range and count what it passes.
    ///
    /// # What this does and does not save
    ///
    /// It saves the **allocations**: [`Self::scan`] copies every key and every
    /// value out of the engine into an owned `Key` and `Value` and collects
    /// them, so counting through it costs two allocations per entry and a
    /// `Vec` proportional to the range. Here nothing is copied out.
    ///
    /// It does **not** avoid reading the values from storage. The engine's
    /// iterator materialises a block at a time and a value lives beside its key
    /// in that block; there is no key-only iteration to ask for without a
    /// second structure to iterate. Said plainly because the reverse is easy to
    /// assume from the name.
    ///
    /// # Why not the engine's own estimate
    ///
    /// The engine can report an approximate key count per column family for
    /// nothing. It is an estimate — it counts entries not yet merged, so a key
    /// written twice counts twice and a deleted one still counts — and it is
    /// per column family rather than per range. A ranking fed an estimate
    /// produces a plausible ordering that is quietly wrong, which is worse than
    /// a slow one that is right.
    ///
    /// # Errors
    ///
    /// Returns the backend's failure. The status check after the walk is not
    /// defensiveness: an iterator that has stopped is either past the end of
    /// the range or has failed to read, the two are indistinguishable from
    /// `valid()` alone, and without the check a failed read returns a short
    /// count that looks like an answer.
    fn count(&self, keyspace: Keyspace, range: &KeyRange) -> Result<u64> {
        if range.is_provably_empty() {
            return Ok(0);
        }
        let region = self.region(keyspace)?;
        let mut iterator = self
            .database
            .raw_iterator_cf_opt(region, read_options(range, Caching::Fill));
        // The read options carry both bounds, so the first key at or after the
        // lower one is where this lands and the upper one ends the walk.
        iterator.seek_to_first();
        let mut total: u64 = 0;
        while iterator.valid() {
            total = total.saturating_add(1);
            iterator.next();
        }
        iterator.status().map_err(|error| from_engine(&error))?;
        Ok(total)
    }

    /// One iterator, seeked many times, instead of one iterator per range.
    ///
    /// [`Self::scan`] creates an engine iterator per call, and creating one is
    /// not free: it pins the engine's view of the store for as long as it lives.
    /// Resolving the records an index range names issues one bounded read per
    /// record, so answering them one at a time creates one iterator per record —
    /// a per-item cost that grows with how much the store holds rather than with
    /// how large the answer is. Here the iterator is created once and moved.
    ///
    /// Each hit is checked against **its own** range's end before it is
    /// accepted. A seek positions at the first key at or after the target and
    /// knows nothing about where the caller wanted to stop, so without that
    /// check a range with nothing in it would answer with the next range's first
    /// key: a real pair, decodable, and the wrong record's.
    fn first_of_each(
        &self,
        keyspace: Keyspace,
        ranges: &[KeyRange],
    ) -> Result<Vec<Option<(Key, Value)>>> {
        if ranges.is_empty() {
            return Ok(Vec::new());
        }
        let region = self.region(keyspace)?;
        let mut options = ReadOptions::default();
        // No prefix extractor is configured today, so a seek is already a
        // total-order seek. Saying so is what keeps this call correct if one is
        // ever configured: with an extractor and without this, a seek is allowed
        // to stop at the end of the target's prefix and report absence for a key
        // that exists.
        options.set_total_order_seek(true);
        let mut iterator = self.database.raw_iterator_cf_opt(region, options);

        let mut found = Vec::with_capacity(ranges.len());
        for range in ranges {
            found.push(seek_first(&mut iterator, range)?);
        }
        Ok(found)
    }

    fn apply(&self, batch: WriteBatch) -> Result<()> {
        self.write(batch, self.durability == Durability::PowerLossSafe)
    }

    fn apply_unsynced(&self, batch: WriteBatch) -> Result<()> {
        // Written ahead and not synced: it survives the process, and
        // `sync_applied` makes it survive the machine.
        self.write(batch, false)
    }

    fn sync_applied(&self) -> Result<()> {
        // One sync of the write-ahead log covers every write landed before it.
        // A store that promises only process-crash safety owes no sync here,
        // exactly as its own `apply` makes none.
        if self.durability != Durability::PowerLossSafe {
            return Ok(());
        }
        // Covered already when a synced commit, or another round's flush,
        // began after these writes landed (`syncs`).
        self.syncs.sync_through(self.syncs.landed_so_far(), || {
            self.database
                .flush_wal(true)
                .map_err(|error| from_engine(&error))
        })
    }

    fn apply_group(&self, batches: Vec<WriteBatch>) -> (usize, Result<()>) {
        self.apply_grouped(batches)
    }

    fn groups_writes(&self) -> bool {
        // Only a synced write has a sync to share.
        self.durability == Durability::PowerLossSafe
    }

    /// One range tombstone, rather than one tombstone per key.
    ///
    /// The difference is not a constant factor. A point delete on this engine is
    /// a write, and every one of those writes has to be carried down the levels
    /// and compacted away before the space it describes comes back. Pruning a
    /// log of a hundred thousand records by scanning and deleting would write a
    /// hundred thousand tombstones to reclaim records that are one contiguous
    /// span of the region. The engine has a primitive for exactly this shape and
    /// it writes ONE tombstone, applied by reads, iterators and compaction
    /// across the whole range.
    ///
    /// Two things this does not do, and both are the layer above's:
    ///
    /// - **It does not return space.** A tombstone lives until a compaction can
    ///   prove nothing older remains beneath it, so the bytes come back on the
    ///   engine's schedule. Each keyspace is its own region with a thirty-day
    ///   SST rewrite TTL, so *eventually* is bounded — but a caller that wants
    ///   the space now asks for it, and a caller that wants to know whether it
    ///   came back measures rather than assumes.
    /// - **It does not decide what to delete.** The trait's header is explicit
    ///   that retention policy belongs above this layer.
    ///
    /// # The one range shape the engine cannot be given
    ///
    /// Its range delete is half-open over two byte strings, so an unbounded END
    /// has nothing to pass — there is no greatest key. An unbounded START does:
    /// the empty key sorts below everything. So that one case falls back to
    /// [`delete_range_by_scanning`], which is the same contract spelled slowly,
    /// rather than to a guessed sentinel that would be wrong for any key above
    /// it.
    fn delete_range(&self, keyspace: Keyspace, range: &KeyRange) -> Result<()> {
        use std::ops::Bound;

        let from = match range.start() {
            Bound::Included(key) => key.as_slice().to_vec(),
            Bound::Excluded(key) => successor(key),
            Bound::Unbounded => Vec::new(),
        };
        let to = match range.end() {
            Bound::Excluded(key) => key.as_slice().to_vec(),
            Bound::Included(key) => successor(key),
            Bound::Unbounded => return delete_range_by_scanning(self, keyspace, range),
        };
        // An empty or inverted range deletes nothing, which is the answer
        // `empty_range_returns_nothing` gives a read of the same span. Stated
        // here rather than left to the engine, because what it does with an
        // inverted range is not part of any contract this store holds.
        if from >= to {
            return Ok(());
        }
        let region = self.region(keyspace)?;
        let _writer = self.writer();
        let before = self.syncs.landed_so_far();
        let mut engine_batch = EngineBatch::default();
        engine_batch.delete_range_cf(region, &from, &to);
        self.database
            .write_opt(engine_batch, &self.durability.write_options())
            .map_err(|error| from_engine(&error))?;
        self.syncs
            .landed(before, self.durability == Durability::PowerLossSafe);
        Ok(())
    }
}
