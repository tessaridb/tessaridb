use super::*;

impl<'a> Transaction<'a> {
    /// The same version, with the causal context its writer had seen.
    ///
    /// What the commit path's stamp producer reads. A write carries forward
    /// every count the version it replaces held and raises only its own, and
    /// that carrying is the entire reason a later comparison can tell ignorance
    /// from sequence — a producer that started from an empty stamp would make
    /// every write concurrent with every other one.
    pub(crate) fn read_newest_stamped(
        &self,
        address: &RecordAddress,
    ) -> Result<Option<(Sequence, StampedValue)>> {
        let prefix = address.versions_prefix();
        self.first_in_range(KeyRange::prefix(&prefix))
    }

    /// The versions of a record that nothing has superseded, newest first.
    ///
    /// One whenever the record is settled, which is every record on a
    /// single-leader range and most records on a multi-master one. More than
    /// one means two nodes wrote it without seeing each other, and that is the
    /// state a commit is refused on rather than resolved (ADR-0075, G027 S3.1).
    ///
    /// # Every version, and not a walk that stops early
    ///
    /// Stopping at the first version the newest descends is O(1) and **wrong**:
    /// with versions `{B:1}`, `{A:1}`, `{A:2}` the newest descends the one
    /// before it and the walk stops, while `{B:1}` is concurrent with it and
    /// survives. The scan is bounded by the versions above the reclaim floor —
    /// which `crate::reclaim` already manages and which is a function of
    /// snapshot lifetime, not of how long the record has existed.
    ///
    /// # Supersession is decided in one place
    ///
    /// [`CausalVersions::record`] owns the rule and this folds every stamp
    /// through it, then maps the survivors back to the versions they came from.
    /// Re-implementing the two lines of that rule here is how two routines
    /// answering one question come to disagree, and the disagreement would be a
    /// live version quietly dropped or a superseded one quietly refused over.
    ///
    /// An unstamped version carries the empty stamp, and two empty stamps are
    /// `Same` — so a store written before stamps existed folds to exactly one
    /// survivor and is never contested.
    pub(in crate::transaction) fn surviving_versions(
        &self,
        address: &RecordAddress,
    ) -> Result<Vec<(Sequence, CausalStamp)>> {
        let held = self.held_versions(address)?;
        let mut surviving = CausalVersions::new();
        for (_, stamp) in &held {
            surviving.record(stamp.clone());
        }
        Ok(surviving
            .stamps()
            .iter()
            .filter_map(|stamp| {
                held.iter()
                    .find(|(_, candidate)| candidate == stamp)
                    .map(|(version, _)| (*version, stamp.clone()))
            })
            .collect())
    }

    /// Every surviving version of a record, each with the node that wrote it,
    /// and whether they are contested (G027 S4.3).
    ///
    /// # The path that returns a version a read resolved away
    ///
    /// A record with two survivors answers every ordinary read with the newest
    /// of them, and the other is still on disk, byte-intact, reachable by
    /// nothing. An operator auditing for data loss finds both versions and
    /// concludes nothing was lost — the bytes are there, and what is missing is
    /// any path that returns them. This is that path.
    ///
    /// # Which node wrote a version, and why it cannot be read off one stamp
    ///
    /// A stamp counts writes per node, so it says what a version has SEEN and
    /// not who wrote it. `{A:5, B:1}` was written by B, which had seen all five
    /// of A's writes, and the largest entry is A's — so "the node with the
    /// highest count" is wrong in exactly the two-master case this exists for.
    ///
    /// The derivation is comparative: the writer of a version is the node whose
    /// count exceeds its count in the newest version this one descends. A first
    /// version descends nothing, and its writer is the only node with a count at
    /// all. This is the same comparison `refuse_a_contested_record` makes to
    /// name the unseen node, so the report and the refusal cannot name different
    /// nodes for one version.
    ///
    /// # The contested flag is not computed here
    ///
    /// [`CausalVersions::is_contested`] answers it, for the reason
    /// [`Self::surviving_versions`] folds through [`CausalVersions::record`]:
    /// two routines answering one question come to disagree, and here the
    /// disagreement would be a contested record reported as settled.
    pub fn surviving_writers(
        &self,
        address: &RecordAddress,
    ) -> Result<(Vec<WrittenVersion>, bool)> {
        let held = self.held_versions(address)?;
        let mut surviving = CausalVersions::new();
        for (_, stamp) in &held {
            surviving.record(stamp.clone());
        }
        let mut answered = Vec::new();
        for stamp in surviving.stamps() {
            let Some((version, _)) = held.iter().find(|(_, candidate)| candidate == stamp) else {
                continue;
            };
            answered.push((*version, writer_of(stamp, &held, *version)));
        }
        answered.sort_by_key(|(at, _)| Reverse(*at));
        Ok((answered, surviving.is_contested()))
    }

    /// Every version of a record above the reclaim floor, with its stamp.
    ///
    /// Extracted so that the survivor walk and the writer walk read one scan
    /// rather than two that could drift apart.
    fn held_versions(&self, address: &RecordAddress) -> Result<Vec<(Sequence, CausalStamp)>> {
        let prefix = address.versions_prefix();
        let request = ScanRequest {
            keyspace: RecordKey::keyspace(),
            range: KeyRange::prefix(&prefix),
            direction: ScanDirection::Forward,
            limit: None,
        };
        let mut held: Vec<(Sequence, CausalStamp)> = Vec::new();
        for (key, value) in self.store.backend().scan(&request)? {
            let version = RecordKey::decode(key.as_slice())?.version;
            let stored = StampedValue::decode(value.as_slice())?;
            // An intent is not a version anybody wrote yet (ADR-0112 D5).
            if crate::intents::is_intent(&stored) {
                continue;
            }
            held.push((version, stored.stamp().clone()));
        }
        Ok(held)
    }

    /// The newest version of a record at or before `snapshot`.
    pub(super) fn read_at(
        &self,
        address: &RecordAddress,
        snapshot: Sequence,
    ) -> Result<Option<(Sequence, RecordValue)>> {
        let now = self.reading_at();
        Ok(self
            .read_stamped_as_of(address, snapshot)?
            .map(|(version, stamped)| (version, stamped.into_visible_at(now))))
    }

    /// The version a reader at this transaction's snapshot resolves to, stamp
    /// and expiry included and **not** judged against the clock.
    pub(crate) fn read_stamped_at(&self, address: &RecordAddress) -> Result<Option<StampedValue>> {
        Ok(self
            .read_stamped_as_of(address, self.snapshot)?
            .map(|(_, stamped)| stamped))
    }

    fn read_stamped_as_of(
        &self,
        address: &RecordAddress,
        snapshot: Sequence,
    ) -> Result<Option<(Sequence, StampedValue)>> {
        let prefix = address.versions_prefix();
        let bounds = KeyRange::prefix(&prefix);
        let range = KeyRange::from_bounds(
            Bound::Included(address.key_at(snapshot).encode()),
            bounds.end().clone(),
        );
        self.first_in_range(range)
    }

    /// The newest entry in a span of one record's versions, stamp and all.
    ///
    /// Decodes as [`StampedValue`] rather than as `RecordValue` because that is
    /// what the store holds: `RecordValue::decode` allows only the tombstone
    /// flag and *refuses* a stamped value outright, so a reader that took the
    /// narrower type would start failing the day commits began carrying a
    /// stamp. One decode site for the reason the codec gives for its splitters
    /// — two readings of one byte string is a thing that can come to disagree.
    ///
    /// A version of a transaction across leaders this one does not see — an
    /// intent not yet committed, or any version of one whose parts this
    /// snapshot does not all hold — is passed over and the version under it
    /// answers (ADR-0112 D5, D6a). One read per such version, which is none for
    /// every record no transaction across leaders wrote.
    pub(in crate::transaction) fn first_in_range(
        &self,
        mut range: KeyRange,
    ) -> Result<Option<(Sequence, StampedValue)>> {
        loop {
            let request = ScanRequest {
                keyspace: RecordKey::keyspace(),
                range: range.clone(),
                direction: ScanDirection::Forward,
                limit: Some(1),
            };
            let found = self.store.backend().scan(&request)?;
            let Some((key, value)) = found.first() else {
                return Ok(None);
            };
            let decoded_value = StampedValue::decode(value.as_slice())?;
            if self.passes_over(&decoded_value)? {
                range = KeyRange::from_bounds(Bound::Excluded(key.clone()), range.end().clone());
                continue;
            }
            let decoded_key = RecordKey::decode(key.as_slice())?;
            return Ok(Some((decoded_key.version, decoded_value)));
        }
    }

    /// A record's newest version as stored — an intent included — with where
    /// it came from and what it holds.
    ///
    /// Asked by the conflict check, which must see what readers pass over: a
    /// write landing on a standing intent would replace a value a transaction
    /// across leaders has prepared, whatever this writer's snapshot. And by a
    /// resolution, which reads the intent it turns into a value.
    pub(in crate::transaction) fn newest_stored_value(
        &self,
        address: &RecordAddress,
    ) -> Result<Option<(Sequence, Option<tessari_encoding::Provenance>, RecordValue)>> {
        let request = ScanRequest {
            keyspace: RecordKey::keyspace(),
            range: KeyRange::prefix(&address.versions_prefix()),
            direction: ScanDirection::Forward,
            limit: Some(1),
        };
        let found = self.store.backend().scan(&request)?;
        let Some((key, value)) = found.first() else {
            return Ok(None);
        };
        let stored = StampedValue::decode(value.as_slice())?;
        Ok(Some((
            RecordKey::decode(key.as_slice())?.version,
            stored.provenance().cloned(),
            stored.into_value(),
        )))
    }
}
