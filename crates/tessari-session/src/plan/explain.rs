use tessari_ql::{JoinSide, Select, Source};
use tessari_storage::{Catalog, Transaction};

use crate::error::Result;
use crate::outcome::AccessPath;
use crate::session::Session;

use super::rank::choose;
use super::reported::Plan;
use super::statement::{closest, nearest, ordered, scored};
use super::worth::worth_serving;

impl Session<'_> {
    /// The plan a read would take, without taking it.
    ///
    /// # It calls the same enumeration and the same `choose`
    ///
    /// Not a second planner that agrees today. Two of them would disagree the
    /// first time one changed, and a plan describing a read nobody runs is worse
    /// than no plan at all — it is a wrong answer to the one question this
    /// statement exists to answer truthfully.
    ///
    /// # It reports what the planner knows and nothing more
    ///
    /// The access path, the index by name, the shape that served it, and the
    /// ceiling — when there was one that was free to learn. No invented cost: a
    /// number this store cannot know is a number it will not print, and a plan
    /// carrying a made-up estimate is how somebody comes to trust one.
    ///
    /// # It is the same structure the answer carries
    ///
    /// [`Plan`], filled here from the choice and filled by the read from the
    /// choice it ran. The one field they can differ on is the access path, and
    /// only where the read fell back — which the planner cannot know, because
    /// whether an index will fill the statement's bound is the read itself. That
    /// difference is the honest answer, and a note on the answer names it.
    pub(crate) fn explain(
        &self,
        transaction: &mut Transaction<'_>,
        select: &Select,
    ) -> Result<crate::outcome::Outcome> {
        let plan = self.plan_of(transaction, select)?;
        Ok(crate::outcome::Outcome::Value(plan.to_value()))
    }

    /// The plan, before it is a value.
    fn plan_of(&self, transaction: &mut Transaction<'_>, select: &Select) -> Result<Plan> {
        match &select.from {
            // One value out of `meta`, with no table, no index and no choice.
            Source::Node => Ok(Plan {
                source: Some("node"),
                ..Plan::new(AccessPath::Record)
            }),
            // Straight to one record by its identity: there is nothing to choose.
            Source::Record(target) => {
                let (_, id) = self.resolve_table(transaction, &target.table)?;
                self.refuse_reading_a_vault(transaction, id, &target.table)?;
                Ok(Plan::new(AccessPath::Record).on(target.table.name.text.as_str()))
            }
            // The span is decided by the statement and not by what exists, so
            // the plan is known without asking anything — which is the same
            // reason `Source::Record` above needs no enumeration.
            Source::Range { table, .. } => {
                let (_, id) = self.resolve_table(transaction, table)?;
                self.refuse_reading_a_vault(transaction, id, table)?;
                Ok(Plan::new(AccessPath::Span).on(table.name.text.as_str()))
            }
            Source::Table(table) => {
                let named = table.name.text.as_str();
                // The same gate the read itself passes through, for the reason
                // in this module's own heading: a plan describing a read nobody
                // runs is a wrong answer to the one question `EXPLAIN` exists to
                // answer. A `SELECT` over a vault is refused, so there is no
                // plan — and reporting `scan` for it said a read would walk the
                // table when the store would not have let it start.
                let (_, gated) = self.resolve_table(transaction, table)?;
                self.refuse_reading_a_vault(transaction, gated, table)?;
                // A read with no condition has nothing for an index to narrow —
                // except the one shape an index answers differently from a scan,
                // which says so by name rather than hiding inside "index".
                if let Some(walk) = nearest(select) {
                    let (_, id) = self.resolve_table(transaction, table)?;
                    // Asked by path, exactly as the read asks it. Taking the
                    // first declared index carrying a vector named a different
                    // index than the read used whenever a table carried two.
                    if let Some(index) = self.index_on_path(transaction, id, walk.path)?
                        && index.vector.is_some()
                    {
                        return Ok(Plan {
                            index: Some(index.name.clone()),
                            ..Plan::new(AccessPath::Approximate).on(named)
                        });
                    }
                    return Ok(Plan::new(AccessPath::Approximate).on(named));
                }
                if let Some(place) = closest(select)
                    && let Some((index, _)) = {
                        let (context, id) = self.resolve_table(transaction, table)?;
                        self.index_serving_place(transaction, context, id, place.path)?
                    }
                {
                    // Named `nearest` rather than left inside "ordered", because
                    // the two answer differently at the bound: a value order
                    // reads entries already in that order, while this one walks
                    // cells and ranks what it finds. Same caveat as below — the
                    // one thing a plan cannot ask is whether the walk will fill
                    // the bound, so a read whose index runs out reports `scan`.
                    return Ok(Plan {
                        shape: Some("nearest"),
                        index: Some(index.name.clone()),
                        ..Plan::new(AccessPath::Ordered).on(named)
                    });
                }
                // Named `scored` for `nearest`'s reason: the walk enumerates the
                // postings of the query's own terms and prunes the rest of the
                // table, which is a different read from taking entries in the
                // order an index already holds. Same caveat again — whether the
                // postings hold enough records to fill the bound is the read's
                // own question, and one that finds too few answers `scan`.
                if let Some(read) = scored(select)
                    && read.wanted > 0
                {
                    let (context, id) = self.resolve_table(transaction, table)?;
                    if let Some(index) = self.index_on_path(transaction, id, read.field)?
                        && index.search
                        && self
                            .index_serving_score(transaction, context, id, read.field)?
                            .is_some()
                    {
                        return Ok(Plan {
                            shape: Some("scored"),
                            index: Some(index.name.clone()),
                            ..Plan::new(AccessPath::Ordered).on(named)
                        });
                    }
                }
                if let Some(bound) = ordered(select)
                    && let Some((index, _)) = {
                        let (context, id) = self.resolve_table(transaction, table)?;
                        self.index_serving_order(
                            transaction,
                            context,
                            id,
                            bound.path,
                            bound.descending,
                        )?
                    }
                {
                    // Every condition but one, and the one it cannot ask is
                    // whether the index will fill the bound — which is the read
                    // itself. So this reports the plan the read *takes*, and a
                    // read whose index runs out first answers `scan`, because
                    // the records below the last entry are the ones the index
                    // does not hold.
                    return Ok(Plan {
                        index: Some(index.name.clone()),
                        ..Plan::new(AccessPath::Ordered).on(named)
                    });
                }
                Ok(Plan::new(AccessPath::Scan).on(named))
            }
            Source::Where { table, condition } => {
                let (context, id) = self.resolve_table(transaction, table)?;
                self.refuse_reading_a_vault(transaction, id, table)?;
                let named = table.name.text.as_str();
                // Asked before the candidates, because the read asks it before
                // the candidates — and from the same function, so the two cannot
                // come to disagree. As in the unconditioned case, whether the
                // walk will fill the bound is the read's own question and not
                // one a plan can answer: a condition too unselective for the
                // order sends the read back to the scan, and this reports the
                // path the planner chose rather than the one it settled for.
                if let Some(bound) = ordered(select)
                    && let Some((index, _)) = self.index_serving_order(
                        transaction,
                        context,
                        id,
                        bound.path,
                        bound.descending,
                    )?
                {
                    return Ok(Plan {
                        index: Some(index.name.clone()),
                        ..Plan::new(AccessPath::Ordered).on(named)
                    });
                }
                let searched = self.searched_for(transaction, id, &[condition])?;
                let declared = Catalog::new(transaction).indexes_on(id)?;
                let offered = self.enumerate(transaction, condition, &declared, &searched)?;
                // The same guard the read applies, from the same function: a
                // winner that does not beat reading the table is not the path
                // the read will take, and an `EXPLAIN` that reported it would
                // be describing a plan nothing runs.
                let chosen = match choose(offered) {
                    Some(candidate) if worth_serving(transaction, id, &candidate)? => {
                        Some(candidate)
                    }
                    _ => None,
                };
                Ok(match chosen {
                    Some(chosen) => chosen.plan(Some(named)),
                    None => Plan::new(AccessPath::Scan).on(named),
                })
            }
            // A walk reads an index per step, and which index is not a choice:
            // an edge table is given one on each endpoint when it is declared.
            Source::Traverse { .. } => Ok(Plan::new(AccessPath::Graph)),
            // Neither side's own path is how a joined answer is reached, so
            // there is one word for it. What is worth naming is the index the
            // right side would be **probed** through — the difference between a
            // probe per left record and a map of the whole right table — and it
            // is asked here through the same function the join asks.
            Source::Join {
                right, right_key, ..
            } => {
                let JoinSide::Table { table, .. } = right.as_ref() else {
                    return Ok(Plan::new(AccessPath::Join));
                };
                let (_, id) = self.resolve_table(transaction, table)?;
                self.refuse_reading_a_vault(transaction, id, table)?;
                Ok(Plan {
                    index: crate::evaluate::ordered_index_on(transaction, id, right_key)?
                        .map(|index| index.name),
                    ..Plan::new(AccessPath::Join)
                })
            }
            // The inner read has a plan of its own and this one does not
            // describe it: a nested plan is its own feature, and one field for
            // it would describe only the shallowest case.
            Source::Subquery { .. } => Ok(Plan::new(AccessPath::Materialised)),
        }
    }
}
