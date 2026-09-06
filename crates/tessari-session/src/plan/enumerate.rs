use std::collections::{BTreeMap, BTreeSet};

use tessari_constants::{
    SEARCH_FUZZY_EXPANSION_CAP, SEARCH_FUZZY_MAX_EDITS, SEARCH_FUZZY_PREFIX,
    SEARCH_PREFIX_EXPANSION_CAP,
};
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
                            answers: None,
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
                            answers: None,
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
                    // Not `analyzer.terms(query)`: a phrase's wrapper, its slop
                    // marker and the boolean operators are not terms, and asking
                    // the dictionary for them returns an empty candidate set for
                    // a query the scan answers. The index and the predicate must
                    // ask the same question of the same string, so both call the
                    // one function that reads the query's shape.
                    let asked = crate::search::asked(analyzer, query);
                    // Whether the postings answer this clause or merely narrow
                    // it, decided here because this is where the query's shape
                    // is known. A phrase's terms reach every record holding
                    // them in any order and the predicate is what settles the
                    // order; an excluded term names a complement an inverted
                    // index cannot enumerate, so it is dropped from the read and
                    // left to the predicate. Both are supersets, and a superset
                    // is a candidate set.
                    let complete = matches!(
                        &asked,
                        crate::search::Asked::Boolean { excluded, .. } if excluded.is_empty()
                    );
                    let groups = match asked {
                        // A phrase's terms all have to be present before their
                        // order can matter, so the candidate set is the same
                        // intersection an unquoted conjunction asks for and the
                        // predicate settles the order.
                        crate::search::Asked::Phrase { terms, .. } => {
                            terms.into_iter().map(|term| vec![term]).collect()
                        }
                        // The excluded terms are dropped here on purpose: an
                        // index enumerates presence, so what it can produce is
                        // the records the required groups reach — a superset,
                        // which the condition then refines as it does every
                        // other candidate.
                        crate::search::Asked::Boolean { required, .. } => required,
                    };
                    if groups.is_empty() {
                        continue;
                    }
                    // A query with no `OR` is a conjunction of single terms, and
                    // saying so keeps `EXPLAIN` reporting `terms` for every query
                    // that could be written before this one could.
                    let plain = groups.iter().all(|group| group.len() == 1);
                    for index in serving(declared, seek.path, true) {
                        // The intersection cannot be larger than the smallest of
                        // the groups, and a group is no larger than the sum of
                        // its terms' postings. A document frequency is a count of
                        // keys rather than a set of decoded ids, so this ceiling
                        // is real and cheap. Were it expensive, a search
                        // candidate would have to rank by shape like the others.
                        let mut smallest = u64::MAX;
                        for group in &groups {
                            let mut reach: u64 = 0;
                            for term in group {
                                let held = transaction.document_frequency(index, term)?;
                                reach = reach.saturating_add(held);
                            }
                            smallest = smallest.min(reach);
                        }
                        let served = if plain {
                            Served::Terms(groups.iter().flatten().cloned().collect())
                        } else {
                            Served::AnyTerms(groups.clone())
                        };
                        offered.push(Candidate {
                            served,
                            index: (*index).clone(),
                            rows: Rows::AtMost(smallest),
                            // `plain` is doing double duty and both are the
                            // same fact: one term per group is what makes the
                            // read an intersection rather than a union of
                            // unions, and it is what makes that intersection
                            // exactly "holds all of these terms" — the
                            // predicate's own question, asked of the same
                            // analyzer over the same field.
                            answers: (plain && complete).then_some(seek.span),
                        });
                    }
                }
                Comparison::PrefixTerms | Comparison::FuzzyTerms => {
                    let (Value::String(query), Some(analyzer)) =
                        (bound, searched.analyzer(seek.path))
                    else {
                        continue;
                    };
                    let fuzzy = seek.comparison == Comparison::FuzzyTerms;
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
                            for spelling in alternatives {
                                // The two walks read the same dictionary and
                                // differ in what they keep, which is why the cap
                                // they answer to is a different number: a prefix
                                // expansion's size is chosen by the reader
                                // typing fewer letters, a fuzzy one's by the
                                // corpus.
                                let found = if fuzzy {
                                    transaction.terms_within_distance(
                                        index,
                                        spelling,
                                        SEARCH_FUZZY_MAX_EDITS,
                                        SEARCH_FUZZY_PREFIX,
                                        SEARCH_FUZZY_EXPANSION_CAP,
                                    )?
                                } else {
                                    transaction.terms_with_prefix(
                                        index,
                                        spelling,
                                        SEARCH_PREFIX_EXPANSION_CAP,
                                    )?
                                };
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
                            let ceiling_terms = if fuzzy {
                                SEARCH_FUZZY_EXPANSION_CAP
                            } else {
                                SEARCH_PREFIX_EXPANSION_CAP
                            };
                            if !serviceable || reached.len() > ceiling_terms {
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
                            served: if fuzzy {
                                Served::FuzzyTerms(expansions)
                            } else {
                                Served::PrefixTerms(expansions)
                            },
                            index: (*index).clone(),
                            rows: Rows::AtMost(ceiling),
                            // The expansions are capped, so what this read
                            // produces is bounded rather than complete and the
                            // predicate is still the answer.
                            answers: None,
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
                    answers: None,
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
                    // Cells are coarser than boxes and boxes are coarser than
                    // shapes, so this read is candidates by construction.
                    answers: None,
                });
            }
        }
        Ok(offered)
    }
}
