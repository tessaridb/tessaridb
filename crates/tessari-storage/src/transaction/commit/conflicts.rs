use super::*;

impl Transaction<'_> {
    /// Refuse the commit if any written record's stored versions disagree, and
    /// answer how many writes a declared last-writer-wins discarded instead.
    ///
    /// G027 S3.1 and the rule the goal exists for: a write concurrent with the
    /// stored version is **refused and named, never silently ranked**
    /// (ADR-0075).
    ///
    /// # Unless the table said otherwise, and then it is counted
    ///
    /// G027 S3.2. A table that declares `LAST WRITER WINS` takes the write, and
    /// the survivors it does not descend are returned as a count — spent by the
    /// caller only once the batch has actually landed. A count is the one thing
    /// that makes the discard observable: the losing version stays on disk
    /// byte-intact and vanishes from every answer, so without it an operator
    /// auditing storage finds both versions and concludes nothing was lost.
    ///
    /// # Why the refusal is here and not on the apply path
    ///
    /// A replica applying a record concurrent with what it holds must **accept**
    /// it. Refusing there would stop two masters' logs from ever meeting, which
    /// is what S2.2 asserts they do; holding several surviving versions is the
    /// whole purpose [`tessari_encoding::CausalVersions`] was built for.
    ///
    /// # And why it is not the incoming write compared against the stored one
    ///
    /// [`Self::log_record`] derives a commit's stamp *from* the newest stored
    /// version, so a local write always **descends** what it replaces and can
    /// never be concurrent with it. The concurrency a store meets is one that
    /// **arrived**, and what is refused is the next write made on top of it —
    /// by a writer holding one of two surviving versions, which cannot supersede
    /// the other without having seen it.
    ///
    /// # What it costs a settled record
    ///
    /// Nothing, outside a namespace that admits two writers. A second surviving
    /// version has one producer — a record applied from another writer's stream
    /// where the namespace's class admits two writers (`Store::apply`) — and a
    /// namespace's class is set when it is defined and never altered, so a
    /// record anywhere else has exactly one survivor and its versions are not
    /// read. That matters because a record can hold many: the one a table's
    /// generated identities are counted in is rewritten by every insert, and
    /// reading all its versions made a run of inserts quadratic (G058, Q-912's
    /// measurement). Asked once per namespace this transaction writes.
    ///
    /// Inside one, one scan of the record's versions per written address — all
    /// it still holds, which on a store that reclaims is those above the
    /// reclaim floor. The table's declaration is read only after two survivors
    /// have been found, so a settled record never reaches the catalog for it.
    ///
    /// # Errors
    ///
    /// [`Error::ConcurrentVersions`] when a contested record's table has not
    /// declared what to do, and the backend's failure when the versions or the
    /// declaration cannot be read.
    pub(super) fn refuse_a_contested_record(&self) -> Result<u64> {
        let mut discarded = 0_u64;
        let mut two_writers: BTreeMap<NamespaceId, bool> = BTreeMap::new();
        for address in self.writes.keys() {
            let admits = match two_writers.get(&address.namespace) {
                Some(admits) => *admits,
                None => {
                    let admits = self
                        .store
                        .admits_two_writers(Reach::Namespace(address.namespace))?;
                    two_writers.insert(address.namespace, admits);
                    admits
                }
            };
            if !admits {
                continue;
            }
            let surviving = self.surviving_versions(address)?;
            let (Some((ours, our_stamp)), Some((theirs, their_stamp))) =
                (surviving.first(), surviving.get(1))
            else {
                continue;
            };
            // G027 S3.2 — unless the table said what to do, in which case this
            // is not a refusal at all. The lookup sits HERE, after two
            // survivors have been found, so it is paid for only by a commit
            // that was otherwise about to be refused: an ordinary write leaves
            // `surviving_versions` with one version and never reaches the
            // catalog. That is W291's placement, and the unedited
            // `counted_reads` admission test is what holds it.
            //
            // The last writer is this caller, not a timestamp. Nothing here
            // reads a clock: the incoming write supersedes every surviving
            // version including the ones it never saw, so the writes discarded
            // are the survivors it does not descend — every one but the newest,
            // which is the one the stamp producer stood on.
            if self
                .store
                .conflict_policy(address.table)?
                .discards_the_loser()
            {
                let lost = surviving.len().saturating_sub(1);
                discarded = discarded.saturating_add(u64::try_from(lost).unwrap_or(u64::MAX));
                continue;
            }
            // The node whose write `theirs` carries and `ours` does not. There
            // is one for every two-master case this engine can produce, and the
            // first in node order is named when there is more than one — the
            // stamp is held in node order, so "first" is a property of the
            // bytes rather than of the order they were read in.
            let unseen = their_stamp
                .entries()
                .iter()
                .find(|(node, seen)| *seen > our_stamp.count(node))
                .map(|(node, _)| *node)
                .unwrap_or_default();
            return Err(Error::ConcurrentVersions {
                id: address.id.clone(),
                ours: *ours,
                theirs: *theirs,
                node: unseen,
            });
        }
        Ok(discarded)
    }

    /// Refuse the commit if any written record has moved since the snapshot.
    ///
    /// This is the write-write detection, and it is only sound because the
    /// commit batch asserts the tail has not moved either — together they turn
    /// check-then-write into a compare-and-set over the whole commit.
    pub(in crate::transaction) fn check_for_conflicts(&self) -> Result<()> {
        // A guarded read is held to the same rule as a write: whatever decided
        // this transaction's writes must not have changed under it.
        let guarded = self.guarded.borrow();
        for address in self.writes.keys().chain(guarded.iter()) {
            // The newest version as stored, intents included — one read, as
            // before intents existed.
            let Some((version, provenance, _)) = self.newest_stored_value(address)? else {
                continue;
            };
            // A resolution writes over its own intents; anybody else's intent,
            // and this one's on a record it does not resolve, refuses.
            let intent = provenance
                .as_ref()
                .is_some_and(|provenance| provenance.provisional)
                && !self.resolves(provenance.as_ref());
            // ADR-0112 D6a: a version resolved from a transaction this one
            // does not see is one it read the version under instead of. Writing
            // over it would lose that transaction's write — an increment read
            // from the old value — so it is refused as a conflict, and passes
            // once this node's copies hold the transaction's every part.
            let unseen = match provenance.as_ref() {
                Some(resolved) if !resolved.provisional => !self.sees(resolved)?,
                _ => false,
            };
            // ADR-0112 D5: a standing intent refuses the write whatever this
            // writer's snapshot. An intent prepared before the snapshot is not
            // newer than it, and replacing the value under it would lose the
            // write the transaction across leaders is about to commit.
            // Retriable: the intent resolves.
            let with = match provenance {
                Some(held) if intent => ConflictWith::Intent(held.transaction),
                Some(held) if unseen => ConflictWith::Unseen(held.transaction),
                _ if version > self.snapshot => ConflictWith::Commit,
                _ => continue,
            };
            return Err(Error::Conflict {
                id: address.id.clone(),
                snapshot: self.snapshot,
                committed: version,
                with,
            });
        }
        Ok(())
    }
}
