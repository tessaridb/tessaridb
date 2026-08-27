use std::collections::BTreeMap;

use tessari_ql::{Select, Source};
use tessari_storage::{Catalog, Transaction};
use tessari_types::Value;

use crate::error::Result;
use crate::session::Session;

use super::candidate::{Rows, Served};
use super::rank::choose;
use super::statement::{closest, nearest, ordered};

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
    pub(crate) fn explain(
        &self,
        transaction: &mut Transaction<'_>,
        select: &Select,
    ) -> Result<crate::outcome::Outcome> {
        let mut plan = BTreeMap::new();
        match &select.from {
            // One value out of `meta`, with no table, no index and no choice.
            Source::Node => {
                plan.insert("access".to_owned(), Value::from("record"));
                plan.insert("source".to_owned(), Value::from("node"));
            }
            // Straight to one record by its identity: there is nothing to choose.
            Source::Record(target) => {
                plan.insert("access".to_owned(), Value::from("record"));
                plan.insert(
                    "table".to_owned(),
                    Value::from(target.table.name.text.as_str()),
                );
            }
            Source::Table(table) => {
                plan.insert("table".to_owned(), Value::from(table.name.text.as_str()));
                // A read with no condition has nothing for an index to narrow —
                // except the one shape an index answers differently from a scan,
                // which says so by name rather than hiding inside "index".
                if nearest(select).is_some() {
                    let (_, id) = self.resolve_table(transaction, table)?;
                    let declared = Catalog::new(transaction).indexes_on(id)?;
                    let named = declared
                        .iter()
                        .find(|index| index.vector.is_some())
                        .map(|index| index.name.clone());
                    plan.insert("access".to_owned(), Value::from("approximate"));
                    if let Some(name) = named {
                        plan.insert("index".to_owned(), Value::from(name.as_str()));
                    }
                } else if let Some(place) = closest(select)
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
                    plan.insert("access".to_owned(), Value::from("ordered"));
                    plan.insert("shape".to_owned(), Value::from("nearest"));
                    plan.insert("index".to_owned(), Value::from(index.name.as_str()));
                } else if let Some(bound) = ordered(select)
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
                    plan.insert("access".to_owned(), Value::from("ordered"));
                    plan.insert("index".to_owned(), Value::from(index.name.as_str()));
                } else {
                    plan.insert("access".to_owned(), Value::from("scan"));
                }
            }
            Source::Where { table, condition } => {
                let (context, id) = self.resolve_table(transaction, table)?;
                plan.insert("table".to_owned(), Value::from(table.name.text.as_str()));
                // Asked before the candidates, because the read asks it before
                // the candidates — and from the same function, so the two cannot
                // come to disagree. As in the unconditioned case, whether the
                // walk will fill the bound is the read's own question and not
                // one a plan can answer: a condition too unselective for the
                // order sends the read back to the scan, and this reports the
                // path the planner chose rather than the one it settled for.
                if let Some(bound) = ordered(select)
                    && bound.descending
                    && let Some((index, _)) = self.index_serving_order(
                        transaction,
                        context,
                        id,
                        bound.path,
                        bound.descending,
                    )?
                {
                    plan.insert("access".to_owned(), Value::from("ordered"));
                    plan.insert("index".to_owned(), Value::from(index.name.as_str()));
                    return Ok(crate::outcome::Outcome::Value(Value::Object(plan)));
                }
                let searched = self.searched_for(transaction, id, &[condition])?;
                let declared = Catalog::new(transaction).indexes_on(id)?;
                let offered = self.enumerate(transaction, condition, &declared, &searched)?;
                match choose(offered) {
                    Some(chosen) => {
                        plan.insert("access".to_owned(), Value::from("index"));
                        plan.insert("index".to_owned(), Value::from(chosen.index.name.as_str()));
                        plan.insert(
                            "shape".to_owned(),
                            Value::from(chosen.served.shape().name()),
                        );
                        // How many of the index's fields the lookup fixes. A
                        // separate number rather than a second `shape` word,
                        // because `Shape`'s ordering is the ranking's tie-break
                        // and a new variant would move plans this is only
                        // reporting on. Equal to the index's arity is a complete
                        // lookup — the thing §8 said could not be asked for.
                        plan.insert(
                            "columns".to_owned(),
                            Value::Number(tessari_types::Number::Integer(
                                i64::try_from(chosen.served.fixed()).unwrap_or(i64::MAX),
                            )),
                        );
                        // How much of the key space a region read will touch,
                        // which is the one cost of it a plan can know without
                        // running it: each cell is a scan plus a lookup per
                        // level above it. The candidate-to-result ratio is the
                        // number that says whether the index is *working*, and
                        // it needs the read to have happened — so it is not
                        // invented here. A plan carrying a made-up estimate is
                        // how somebody comes to trust one.
                        if let Served::Region { cells, .. } = &chosen.served {
                            plan.insert(
                                "cells".to_owned(),
                                Value::Number(tessari_types::Number::Integer(
                                    i64::try_from(cells.len()).unwrap_or(i64::MAX),
                                )),
                            );
                        }
                        if let Rows::AtMost(held) = chosen.rows {
                            plan.insert(
                                "at_most".to_owned(),
                                Value::Number(tessari_types::Number::Integer(
                                    i64::try_from(held).unwrap_or(i64::MAX),
                                )),
                            );
                        }
                    }
                    None => {
                        plan.insert("access".to_owned(), Value::from("scan"));
                    }
                }
            }
            // A walk reads an index per step, and which index is not a choice:
            // an edge table is given one on each endpoint when it is declared.
            Source::Traverse { .. } => {
                plan.insert("access".to_owned(), Value::from("graph"));
            }
            Source::Join { .. } => {
                plan.insert("access".to_owned(), Value::from("join"));
            }
            // The inner read has a plan of its own and this one does not
            // describe it. Reporting a single structure that covers both is R-2,
            // and it belongs with the note channel rather than here.
            Source::Subquery { .. } => {
                plan.insert("access".to_owned(), Value::from("materialised"));
            }
        }
        Ok(crate::outcome::Outcome::Value(Value::Object(plan)))
    }
}
