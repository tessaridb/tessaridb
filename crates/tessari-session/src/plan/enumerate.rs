use std::collections::{BTreeMap, BTreeSet};

use tessari_constants::SEARCH_PREFIX_EXPANSION_CAP;
use tessari_geo::{Cell, Shape as Geometry};
use tessari_ql::{BinaryOp, Expr};
use tessari_storage::{IndexDefinition, Transaction};
use tessari_types::{Path, Value};

use crate::condition::literal_prefix;
use crate::error::Result;
use crate::search::Searched;
use crate::session::Session;

use super::candidate::{Candidate, Rows, Served};
use super::conjunct::{Comparison, regional, seekable};
use super::serving::{gathered, ranged, serving, spatial};

impl Session<'_> {
    /// Every conjunct one of these indexes could serve, with what it promises.
    ///
    /// A bound is evaluated **here**, once, and carried on the candidate — both
    /// because ranking a search candidate needs it, and because evaluating it
    /// again in the executor would let a `time::now()` in a filter mean two
    /// different instants inside one statement.
    ///
    /// The gathering is **by index**, not by clause: an index is asked what the
    /// whole condition fixes for it, so `last = 'x' AND first = 'y'` against
    /// `(last, first)` becomes one lookup rather than a lookup on `last` and a
    /// re-test of `first`. Candidates are still emitted in the order the author
    /// wrote the conjuncts, because that is what makes an equal-ranked tie
    /// predictable from the condition rather than from the schema.
    pub(crate) fn enumerate(
        &self,
        transaction: &mut Transaction<'_>,
        condition: &Expr,
        declared: &[IndexDefinition],
        searched: &Searched,
    ) -> Result<Vec<Candidate>> {
        let seeks = seekable(condition);
        // Evaluated once each and in the order they were written, before
        // anything is gathered. Gathering has to look ahead at conjuncts the
        // emitting loop has not reached, and evaluating a bound at the point it
        // is looked at would evaluate some of them twice.
        //
        // A right-hand side that reads the record is not a constant, so it
        // cannot be a bound; `seekable` has already excluded those.
        let mut bounds_of = Vec::with_capacity(seeks.len());
        for seek in &seeks {
            bounds_of.push(self.evaluate(transaction, seek.value)?);
        }
        // What the condition fixes each path to. The **first** equality on a
        // path wins: `a = 1 AND a = 2` answers with nothing whichever value
        // narrows, because the condition is re-tested above the source, so a
        // second candidate for the same path is a second answer to one question.
        let mut fixed: BTreeMap<&Path, &Value> = BTreeMap::new();
        for (seek, bound) in seeks.iter().zip(&bounds_of) {
            if seek.comparison == Comparison::Equality {
                fixed.entry(seek.path).or_insert(bound);
            }
        }

        let mut offered = Vec::new();
        let mut already: BTreeSet<&Path> = BTreeSet::new();
        // `at >= x AND at < y` is one range written as two conjuncts, and
        // serving one of them would read half a table to find a day. Bounds on
        // one path are gathered before anything is offered; bounds on different
        // paths are not combined, because they narrow independently and the
        // planner is what chooses between them.
        let mut bounds: BTreeMap<&Path, (Option<Value>, Option<Value>)> = BTreeMap::new();
        for (seek, bound) in seeks.iter().zip(&bounds_of) {
            // An ordered index answers an equality and a prefix; it cannot answer
            // a term, and a search index cannot answer either of the other two.
            // Asking the wrong one would return the wrong rows rather than none.
            // A region does not arrive here and cannot be written to: a `Seek`
            // carries a `Comparison`, which has no variant for one, because a
            // geometric relation has no operator to be a `Seek` about. It is
            // gathered by `regional` below. This used to be an arm that skipped
            // it — a branch no input could reach, which is a branch no test can
            // exercise and therefore none can keep right.
            match seek.comparison {
                Comparison::Equality => {
                    // Once per path rather than once per conjunct: the tuple is
                    // gathered from the whole condition, so arriving at the same
                    // path again would re-offer what is already offered.
                    if !already.insert(seek.path) {
                        continue;
                    }
                    for index in serving(declared, seek.path, false) {
                        let values = gathered(index, &fixed);
                        // A unique index holds one entry per **whole tuple**, so
                        // an equality on one produces at most one record exactly
                        // when the condition fixes every field. Fixing only some
                        // of them promises nothing: one `last` may have any
                        // number of `first`s, and claiming a ceiling there would
                        // make the planner prefer an index that can return the
                        // whole table.
                        let rows = if index.unique && values.len() == index.fields.len() {
                            Rows::AtMost(1)
                        } else {
                            Rows::Unknown
                        };
                        offered.push(Candidate {
                            served: Served::Equality(values),
                            index: (*index).clone(),
                            rows,
                        });
                    }
                }
                Comparison::Prefix => {
                    let Value::String(pattern) = bound else {
                        continue;
                    };
                    let Some(prefix) = literal_prefix(pattern) else {
                        continue;
                    };
                    for index in serving(declared, seek.path, false) {
                        offered.push(Candidate {
                            served: Served::Prefix(prefix.clone()),
                            index: (*index).clone(),
                            rows: Rows::Unknown,
                        });
                    }
                }
                Comparison::Range => {
                    let (lower, upper) = bounds.entry(seek.path).or_default();
                    // The tighter of two bounds in one direction wins; a
                    // condition may say `at > 1 AND at > 5` and mean the second.
                    let end = match seek.op {
                        BinaryOp::Greater | BinaryOp::GreaterOrEqual => lower,
                        _ => upper,
                    };
                    let tighter = match (&end, &seek.op) {
                        (None, _) => true,
                        (Some(held), BinaryOp::Greater | BinaryOp::GreaterOrEqual) => {
                            *bound > *held
                        }
                        (Some(held), _) => *bound < *held,
                    };
                    if tighter {
                        *end = Some(bound.clone());
                    }
                }
                Comparison::Terms => {
                    let (Value::String(query), Some(analyzer)) =
                        (bound, searched.analyzer(seek.path))
                    else {
                        continue;
                    };
                    let terms = analyzer.terms(query);
                    if terms.is_empty() {
                        continue;
                    }
                    for index in serving(declared, seek.path, true) {
                        // The intersection of the postings cannot be larger than
                        // the smallest of them, and a document frequency is a
                        // count of keys rather than a set of decoded ids — so
                        // this ceiling is real and cheap. Were it expensive, a
                        // search candidate would have to rank by shape like the
                        // others.
                        let mut smallest = u64::MAX;
                        for term in &terms {
                            let held = transaction.document_frequency(index, term)?;
                            smallest = smallest.min(held);
                        }
                        offered.push(Candidate {
                            served: Served::Terms(terms.clone()),
                            index: (*index).clone(),
                            rows: Rows::AtMost(smallest),
                        });
                    }
                }
                Comparison::PrefixTerms => {
                    let (Value::String(query), Some(analyzer)) =
                        (bound, searched.analyzer(seek.path))
                    else {
                        continue;
                    };
                    let asked = analyzer.prefixes(query);
                    if asked.is_empty() {
                        continue;
                    }
                    for index in serving(declared, seek.path, true) {
                        // Every word is expanded before any candidate is
                        // offered, because one word reaching past the cap
                        // decides the whole read: the answer is a conjunction,
                        // so an index that cannot enumerate one of its parts
                        // cannot serve it at all.
                        //
                        // Past the cap the candidate is **not offered** and the
                        // scan answers. It is not a refusal, deliberately: a cap
                        // that refused would make a statement run on a table
                        // with no index and fail on the same table once somebody
                        // added one, which is the failure the access-path rule
                        // exists to prevent.
                        let mut expansions = Vec::with_capacity(asked.len());
                        let mut ceiling: u64 = u64::MAX;
                        let mut serviceable = true;
                        for alternatives in &asked {
                            let mut reached: BTreeSet<String> = BTreeSet::new();
                            for prefix in alternatives {
                                let found = transaction.terms_with_prefix(
                                    index,
                                    prefix,
                                    SEARCH_PREFIX_EXPANSION_CAP,
                                )?;
                                if found.capped {
                                    serviceable = false;
                                    break;
                                }
                                reached.extend(found.terms);
                            }
                            // The alternatives overlap — a word and its stem
                            // share a beginning — so the union is deduplicated
                            // before it is measured against the cap, and a word
                            // is judged by how many distinct terms it actually
                            // reaches rather than by how many times it was
                            // asked.
                            if !serviceable || reached.len() > SEARCH_PREFIX_EXPANSION_CAP {
                                serviceable = false;
                                break;
                            }
                            if reached.is_empty() {
                                // A word nothing begins with makes the whole
                                // conjunction empty, and the index can say so
                                // without reading a single posting.
                                expansions.clear();
                                expansions.push(Vec::new());
                                ceiling = 0;
                                break;
                            }
                            // The union of these postings is at most their sum,
                            // and the intersection across words is at most the
                            // smallest union. Both are counts of keys rather
                            // than sets of ids, so the ceiling stays cheap.
                            let mut union: u64 = 0;
                            for term in &reached {
                                union = union
                                    .saturating_add(transaction.document_frequency(index, term)?);
                            }
                            ceiling = ceiling.min(union);
                            expansions.push(reached.into_iter().collect());
                        }
                        if !serviceable {
                            continue;
                        }
                        offered.push(Candidate {
                            served: Served::PrefixTerms(expansions),
                            index: (*index).clone(),
                            rows: Rows::AtMost(ceiling),
                        });
                    }
                }
            }
        }

        for (path, (lower, upper)) in bounds {
            for (index, run) in ranged(declared, path, &fixed) {
                offered.push(Candidate {
                    served: Served::Range {
                        fixed: run,
                        lower: lower.clone(),
                        upper: upper.clone(),
                    },
                    index: index.clone(),
                    // A range can be the whole run it walks and its size is not
                    // knowable without doing the read, which is the same answer a
                    // prefix gives. What it narrows is carried by `Served::fixed`
                    // instead, where it is a proof rather than a guess.
                    rows: Rows::Unknown,
                });
            }
        }

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
            let Some(bounds) = Geometry::of(&geometry)
                .ok()
                .and_then(|shape| shape.bounds())
            else {
                continue;
            };
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
                });
            }
        }
        Ok(offered)
    }
}
