use super::*;

impl Session<'_> {
    /// Offer a candidate on every spatial index for each path a geometric
    /// conjunct of `condition` constrains, after every comparison was offered.
    pub(super) fn offer_regional(
        &self,
        transaction: &mut Transaction<'_>,
        condition: &Expr,
        declared: &[IndexDefinition],
        offered: &mut Vec<Candidate>,
    ) -> Result<()> {
        // The geometric conjuncts, gathered separately because a relation is not
        // a comparison, and offered last so that an equal-ranked tie still falls
        // through to the order the conjuncts were written — the emitting order
        // within each kind is what makes a plan predictable from the condition.
        let mut placed: BTreeSet<&Path> = BTreeSet::new();
        for reach in regional(condition) {
            // Once per path, as an equality is. Two relations on one field are
            // two questions about the same cells, and the second would offer a
            // candidate the first already covers.
            if !placed.insert(reach.path) {
                continue;
            }
            let indexes = spatial(declared, reach.path);
            if indexes.is_empty() {
                continue;
            }
            // The query shape is evaluated here, once, like every other bound —
            // and for the extra reason that a covering is not cheap enough to
            // compute per index.
            let Value::Geometry(geometry) = self.evaluate(transaction, reach.query)? else {
                continue;
            };
            // A query shape off the grid is not stored, so it is not held to the
            // store's validity rules; it is held to being somewhere on the
            // planet, and one that is not cannot name cells. The scan answers it
            // exactly, and `geo::` reports the refusal from the predicate itself.
            let Some(mut bounds) = Geometry::of(&geometry)
                .ok()
                .and_then(|shape| shape.bounds())
            else {
                continue;
            };
            // A radius widens the box by the distance, which must be a number;
            // anything else — a string, `none`, a negative or unbounded distance —
            // names no box, and the scan answers it exactly.
            if let Some(radius) = reach.widened_by {
                let Value::Number(radius) = self.evaluate(transaction, radius)? else {
                    continue;
                };
                let Some(widened) = radius
                    .as_float()
                    .and_then(|metres| tessari_geo::within_reach(bounds, metres))
                else {
                    continue;
                };
                bounds = widened;
            }
            let cells: Vec<Cell> =
                tessari_geo::covering(bounds, tessari_constants::SPATIAL_QUERY_CELLS)
                    .into_iter()
                    .map(|(cell, _)| cell)
                    .collect();
            for index in indexes {
                offered.push(Candidate {
                    served: Served::Region {
                        cells: cells.clone(),
                        bounds,
                        relation: reach.relation,
                    },
                    index: index.clone(),
                    // A query box can hold the whole table and its size is not
                    // knowable without the read — the same answer a prefix and a
                    // range give, and for the same reason.
                    rows: Rows::Unknown,
                    // Cells are coarser than boxes and boxes are coarser than
                    // shapes, so this read is candidates by construction.
                    answers: None,
                });
            }
        }
        Ok(())
    }
}
