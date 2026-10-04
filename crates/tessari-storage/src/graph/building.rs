use super::*;

impl Graph {
    /// Place a record in the graph, and return every node the placement changed.
    ///
    /// The new node links to its nearest neighbours, and each of those gains the
    /// reverse edge — pruned back to [`NEIGHBOURS`] by distance, ties on record
    /// id. Without the reverse edge a new record is reachable from nowhere and
    /// the graph is a collection of one-way streets.
    pub(crate) fn insert(
        &mut self,
        id: &RecordId,
        vector: Vec<f64>,
    ) -> Result<BTreeMap<RecordId, VectorNode>> {
        let mut touched = BTreeMap::new();
        let chosen = if self.is_empty()? {
            Vec::new()
        } else {
            // `None`, and never a caller's budget. The build walks the graph to
            // choose this node's neighbours, so a read's `EFFORT` reaching here
            // would make the index a function of the reads that happened to run
            // beside the writes — and two replicas replaying one log would build
            // different graphs. That is the determinism this index gave up its
            // hierarchical layer to keep.
            self.nearest(&vector, NEIGHBOURS, None)?
        };
        let node = VectorNode::new(self.stored(vector), chosen.clone());
        self.nodes.put(id.clone(), node.clone());
        touched.insert(id.clone(), node);

        for neighbour in chosen {
            let Some(held) = self.nodes.get(&neighbour)? else {
                continue;
            };
            if held.neighbours.contains(id) {
                continue;
            }
            let mut linked = held.neighbours.clone();
            linked.push(id.clone());
            let pruned = self.prune(&held.vector.to_vec(), linked)?;
            let updated = VectorNode::new(held.vector.clone(), pruned);
            self.nodes.put(neighbour.clone(), updated.clone());
            touched.insert(neighbour, updated);
        }
        Ok(touched)
    }

    /// Keep [`NEIGHBOURS`] of these, chosen for **reach** rather than nearness.
    ///
    /// # Why not simply the nearest
    ///
    /// Because that is what stops the graph working, and it does so invisibly.
    /// Keeping the sixteen nearest makes every node's links point at its own
    /// immediate crowd, so a walk that starts in one region can never leave it —
    /// the long edges that make a small world small are exactly the ones a
    /// nearest-first rule throws away first. Measured on this store, nearest-M
    /// pruning gave **one per cent** of the true ten; the rule below gives most
    /// of them, on the same data, with the same walk.
    ///
    /// # The rule
    ///
    /// Walking the candidates nearest-first, a candidate is kept only if it is
    /// closer to the base than to anything already kept. A candidate that sits
    /// behind an existing neighbour is reachable **through** it and adds no new
    /// direction; one that opens a direction nothing else covers is kept however
    /// far away it is. So the neighbour list spans the space around a node
    /// instead of huddling on one side of it.
    ///
    /// Ties break on record id, so the choice is a function of the vectors and
    /// nothing else — which is what lets two replicas build one graph.
    fn prune(&self, from: &[f64], candidates: Vec<RecordId>) -> Result<Vec<RecordId>> {
        let mut ranked: Vec<(f64, RecordId)> = Vec::with_capacity(candidates.len());
        for id in candidates {
            ranked.push((self.at(id.clone(), from)?, id));
        }
        ranked.sort_by(|left, right| {
            left.0
                .total_cmp(&right.0)
                .then_with(|| left.1.cmp(&right.1))
        });
        ranked.dedup_by(|left, right| left.1 == right.1);

        let mut kept: Vec<(RecordId, Vec<f64>)> = Vec::new();
        for (to_base, id) in ranked {
            if kept.len() >= NEIGHBOURS {
                break;
            }
            let Some(held) = self.nodes.get(&id)? else {
                continue;
            };
            let covered = kept
                .iter()
                .any(|(_, other)| separation_from(self.distance, &held.vector, other) < to_base);
            if covered {
                continue;
            }
            kept.push((id, held.vector.to_vec()));
        }
        Ok(kept.into_iter().map(|(id, _)| id).collect())
    }
}
