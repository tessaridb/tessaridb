use super::*;

impl Graph {
    /// The records nearest this vector, nearest first, at most `wanted` of them.
    ///
    /// A greedy walk from the entry point, keeping the best `effort` candidates
    /// seen. Approximate by construction — see the module documentation for why
    /// that is a language-level decision and not a detail.
    ///
    /// `effort` is `None` for the budget this engine was built with
    /// ([`EXPLORATION`]) and `Some` for a budget the **read** named. It is raised
    /// to at least `wanted`, because a walk that keeps fewer candidates than the
    /// answer asks for cannot fill the answer, and a budget silently overriding a
    /// `LIMIT` is a bound answering for a bound.
    ///
    /// **A read's budget never reaches the build.** [`Self::insert`] walks this
    /// same graph to choose a new node's neighbours and passes `None` — see the
    /// note there for what a leak would cost.
    pub(crate) fn nearest(
        &self,
        query: &[f64],
        wanted: usize,
        effort: Option<usize>,
    ) -> Result<Vec<RecordId>> {
        let effort = effort.unwrap_or(EXPLORATION).max(wanted);
        let Some(entry) = self.entry()? else {
            return Ok(Vec::new());
        };
        let mut seen: BTreeSet<RecordId> = BTreeSet::new();
        // Candidates to expand, and the best found so far. Both are kept sorted
        // by distance with the record id breaking ties, so the walk is a
        // function of the graph and the query and of nothing else.
        let mut frontier: Vec<(f64, RecordId)> = Vec::new();
        let mut best: Vec<(f64, RecordId)> = Vec::new();

        let start = self.at(entry.clone(), query)?;
        seen.insert(entry.clone());
        frontier.push((start, entry.clone()));
        best.push((start, entry));

        while let Some((_, current)) = take_nearest(&mut frontier) {
            let Some(node) = self.nodes.get(&current)? else {
                continue;
            };
            // Stop when nothing in hand can improve on what is already held:
            // the classic greedy cut-off, and what keeps the walk sub-linear.
            if let Some((furthest, _)) = best.last()
                && best.len() >= effort
                && self.at(current.clone(), query)? > *furthest
            {
                break;
            }
            for neighbour in &node.neighbours {
                if !seen.insert(neighbour.clone()) {
                    continue;
                }
                // An edge into a removed record is dangling — this graph does
                // not chase inbound edges when a node goes, so they exist. It is
                // not a candidate: offering it would answer with a record that
                // is not in the index, which the resolution step would drop and
                // which would meanwhile have taken a place in the answer.
                if !self.nodes.contains(neighbour)? {
                    continue;
                }
                let distance = self.at(neighbour.clone(), query)?;
                frontier.push((distance, neighbour.clone()));
                insert_sorted(&mut best, distance, neighbour.clone(), effort);
            }
        }

        Ok(best.into_iter().take(wanted).map(|(_, id)| id).collect())
    }

    /// What fraction of the true nearest this graph actually returns.
    ///
    /// The walk is compared against the exact answer over the same records, and
    /// the result is a **measurement** — the one thing [`VectorRecall`] is
    /// allowed to hold, and the reason it is not computed from [`NEIGHBOURS`]
    /// and [`EXPLORATION`] instead.
    ///
    /// # The queries are the store's own vectors, and that has a trap in it
    ///
    /// There are no others: nothing here records what anyone has searched for.
    /// So the sample is taken from the stored vectors themselves, **by position
    /// in key order** — every `⌈records / sample⌉`-th — which makes it a
    /// function of the stored set rather than of a draw, an insertion order or a
    /// clock. That matters for the same reason the graph has one layer: two
    /// replicas replaying one log must reach the same number.
    ///
    /// A stored vector queried against itself finds itself at **distance zero**.
    /// That is a free hit, and a measurement that kept it would report a floor of
    /// `1/at` on an index that finds nothing else — a figure that looks like a
    /// measurement and is not. So the query record is removed from both the
    /// truth and the answer, and the comparison is over what is left.
    ///
    /// Perturbing the sampled vectors instead was considered and rejected: a
    /// perturbation needs a random direction, and randomness is precisely what
    /// this index gave up its hierarchical layer to avoid.
    ///
    /// `None` when there is nothing to measure — an index over fewer than two
    /// records has no answer a walk could get wrong, and absence reads as *never
    /// measured*, which is a different statement from a measured zero.
    pub(crate) fn recall(&self) -> Result<Option<VectorRecall>> {
        self.nodes.load_all()?;
        let present = self.nodes.present();
        let records = present.len();
        if records < 2 {
            return Ok(None);
        }
        let stride = records.div_ceil(MEASURED_SAMPLE).max(1);
        let mut hit = 0_usize;
        let mut asked = 0_usize;
        let mut sample = 0_usize;
        for (id, node) in present.iter().step_by(stride) {
            let probe = node.vector.to_vec();
            let truth = exact(self.distance, &present, &probe, id, MEASURED_AT);
            if truth.is_empty() {
                continue;
            }
            // One more than the answer, because the query record is expected
            // back and is then dropped; `take` trims the case where it was not.
            let found: Vec<RecordId> = self
                .nearest(&probe, MEASURED_AT.saturating_add(1), None)?
                .into_iter()
                .filter(|other| other != id)
                .take(MEASURED_AT)
                .collect();
            hit = hit.saturating_add(found.iter().filter(|got| truth.contains(got)).count());
            asked = asked.saturating_add(truth.len());
            sample = sample.saturating_add(1);
        }
        let Some(recall) = hit.saturating_mul(100).checked_div(asked) else {
            return Ok(None);
        };
        Ok(Some(VectorRecall {
            recall: u32::try_from(recall).unwrap_or(100),
            at: u32::try_from(MEASURED_AT).unwrap_or(u32::MAX),
            sample: u32::try_from(sample).unwrap_or(u32::MAX),
            records: u64::try_from(records).unwrap_or(u64::MAX),
            neighbours: u32::try_from(NEIGHBOURS).unwrap_or(u32::MAX),
            exploration: u32::try_from(EXPLORATION).unwrap_or(u32::MAX),
        }))
    }
}

/// The records genuinely nearest this vector among `present`, by looking at
/// every one.
///
/// The truth half of [`Graph::recall`], and `O(records)` per call by definition —
/// there is no cheaper way to know what a walk missed. `excluding` is the query's
/// own record, because a vector is always nearest to itself.
pub(super) fn exact(
    distance: VectorDistance,
    present: &[(RecordId, std::sync::Arc<VectorNode>)],
    query: &[f64],
    excluding: &RecordId,
    wanted: usize,
) -> Vec<RecordId> {
    let mut held: Vec<(f64, RecordId)> = Vec::new();
    for (id, node) in present {
        if id == excluding {
            continue;
        }
        let separation = separation_from(distance, &node.vector, query);
        insert_sorted(&mut held, separation, id.clone(), wanted);
    }
    held.into_iter().map(|(_, id)| id).collect()
}
