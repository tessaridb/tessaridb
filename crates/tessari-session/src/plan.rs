//! Choosing which index runs, rather than taking the first one written.
//!
//! # What this changes, and what it deliberately cannot
//!
//! A condition may offer several conjuncts an index could serve. Until this
//! module existed the first one in the statement won, which meant
//! `WHERE city = 'x' AND email = 'ada@example.com'` read the `city` index even
//! though `email` is unique and selects exactly one record.
//!
//! That was never a *wrong answer* — candidates are re-tested against the whole
//! condition, so the rows come back right whichever index narrowed them. It was
//! a wrong **cost**, and a wrong cost raises nothing, which is why it survived
//! several waves. The rule that replaces it is stated here in one place so that
//! it can be read, argued with, and tested without a store.
//!
//! What it cannot change is the answer. The chosen candidate narrows; the
//! condition still decides. That is the store's governing rule, and a planner is
//! precisely the component most tempted to break it.
//!
//! # Exact numbers only where they are free
//!
//! A planner that counts every candidate pays for each answer twice: counting the
//! records under a secondary index's value costs the same scan as reading them.
//! So the ranking uses a real number only where knowing it is free, and a
//! declared ordering everywhere else.
//!
//! | Candidate | Rows | What knowing that costs |
//! |---|---|---|
//! | equality on a **unique** index | at most 1 | nothing — it is what unique means |
//! | `MATCHES` on a search index | at most the smallest term's `df` | one prefix count per term |
//! | equality on a secondary index | unknown | the read itself |
//! | `LIKE 'a%'` prefix range | unknown, possibly the whole table | the read itself |
//!
//! The search bound is only cheap because SGC.T3 made a document frequency a
//! count of keys rather than a set of decoded record ids. The two nodes compose
//! by accident of good luck rather than design, and it is worth saying so: had
//! `df` stayed expensive, a search candidate would rank by shape like the others.
//!
//! # Why rule-based and not cost-based
//!
//! A cost model needs statistics about *value distribution* — how many records
//! hold `city = 'london'` as against `city = 'tromsø'` — and that means
//! histograms. A histogram is maintained state whose staleness silently changes
//! plans, which is a much larger decision than this one and wants a benchmark
//! harness (SGG.T1) to justify it rather than an intuition.
//!
//! # Ties break on source order
//!
//! Not arbitrarily, and not on index id: two runs of one statement must choose
//! the same way, and an author who reads their own condition should be able to
//! predict which of two equal candidates wins.
//!
//! One rule sits above it, and only because it is a **proof** rather than a
//! preference: a candidate narrowing more of its index's columns cannot return
//! more records than one narrowing fewer of them, since its entries are a
//! subset. An equal count still falls through to the order the conjuncts were
//! written.
//!
//! That proof sits above the **shape** ranking too, and it has to. The shape
//! order in `Shape` describes what a candidate is *trusted* to narrow when
//! nothing exact is known, which is a heuristic and is stated as one. Below the
//! proof it made a composite range unreachable: `a = 1 AND b > 2` on `(a, b)`
//! narrows two columns as a range and one as an equality, and `Equality` sorts
//! before `Range`, so the wider candidate won on the guess. Nothing this store
//! chose before moves, because every candidate that narrowed more than one
//! column was an equality — which the shape order already preferred.
//!
//! This is the rule the gathering restructuring most easily loses. Asking each
//! index what the whole condition fixes for it invites an outer loop over
//! *indexes*, which would make an equal tie resolve by declaration order — a
//! schema fact the author of the condition cannot see. So the gathering is by
//! index and the emitting is by conjunct, which keeps both properties at once.

use core::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use tessari_geo::{Bounds, Cell, Relation, Shape as Geometry};
use tessari_ql::{BinaryOp, Expr, ExprKind, Function, Projected, Projection, Select, Source};
use tessari_storage::{Catalog, IndexDefinition, Transaction, VectorDistance};
use tessari_types::{Path, Value};

use crate::condition::literal_prefix;
use crate::error::Result;
use crate::search::Searched;
use crate::session::Session;

/// What shape of test an index is being asked to answer.
///
/// Ordered by how much a candidate of this shape is trusted to narrow when
/// nothing exact is known: a single value beats a range, because a range can be
/// the whole table (`LIKE 'a%'`) and a value cannot be more than the records
/// holding it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Shape {
    /// `<path> = <constant>` — one entry, or one value's worth of them.
    Equality,
    /// `<path> MATCHES '<text>'` — the postings of every term, intersected.
    Terms,
    /// `<path> LIKE '<literal>%'` — a range over the values beginning with it.
    Prefix,
    /// `<path> < <constant>`, and the other three orderings — a bounded scan.
    ///
    /// Ranked beside a prefix and for the same reason: both can be the whole
    /// table, and neither's size is knowable without doing the read.
    Range,
    /// `geo::intersects(<path>, <constant>)`, and the five other relations a box
    /// test is a superset of — the cells a query box covers.
    ///
    /// Last, and conservatively so. A query box can be the world, so like a
    /// prefix and a range its size is not knowable without the read; unlike
    /// them, what it produces is a set of **candidates** to refine rather than a
    /// set of matches, so among shapes that promise equally little it is the one
    /// doing the most work per row it returns. Ranking it above anything would
    /// be a claim about selectivity this store keeps no statistics to make.
    Region,
}

/// What a chosen candidate hands the executor, ready to run.
///
/// A sum rather than a struct of optional fields, so that a prefix candidate
/// cannot exist without its prefix and a term candidate cannot exist without its
/// terms. The alternative — one `bound: Value` plus a `terms: Vec<String>` that
/// is empty for two shapes out of three — puts branches in the executor that
/// cannot be reached and therefore cannot be tested, which is how an unreachable
/// branch quietly becomes reachable.
///
/// It also means the work of deciding is not repeated: the literal prefix and
/// the analysed terms are computed once, while ranking, and travel to the read.
#[derive(Debug, Clone)]
pub(crate) enum Served {
    /// The values the index is looked up by — one per field it fixes, in field
    /// order, and always a **leading** run of them.
    ///
    /// A tuple rather than a value, because an index that could answer about
    /// two of its columns and was only ever asked about one is the cost §8 named
    /// twice. When the run reaches the index's arity the lookup is complete, and
    /// `Transaction::records_by_index` turns a complete lookup on a unique index
    /// into a point read — which is what makes a `UNIQUE` composite able to
    /// promise a ceiling of one at all.
    ///
    /// The run stops at the first field the condition does not fix with an
    /// equality, so `a = 1 AND b LIKE 'x%'` on `(a, b)` fixes `[1]`: the lookup
    /// takes exact values per field and a mixed tuple cannot be said in it. The
    /// condition re-tests the rest, as it does for every candidate anyway.
    Equality(Vec<Value>),
    /// The literal prefix the range starts at.
    Prefix(String),
    /// The terms whose postings are intersected.
    Terms(Vec<String>),
    /// The two ends of an ordered scan, either of which may be absent.
    ///
    /// Both are carried as **values**, not as byte bounds, because the index
    /// encoding normalises — `1` and `1.0` become the same bytes — so an
    /// exclusive end cannot be said in bytes at all. It does not need to be: the
    /// scan takes both ends inclusive and the condition that asked discards what
    /// it does not want, which it was going to do to every candidate anyway.
    Range {
        /// The leading run of the index's fields the condition fixes to exact
        /// values, with the ranged field immediately after it.
        ///
        /// Empty for a range on the index's own leading field, which is every
        /// range this store served before composite ranges existed. Non-empty
        /// for `a = 1 AND b > 2` on `(a, b)`, where the bounds apply *inside* the
        /// entries holding `a = 1` rather than across the whole index.
        fixed: Vec<Value>,
        /// The lower end, when the condition gave one.
        lower: Option<Value>,
        /// The upper end, when it gave one.
        upper: Option<Value>,
    },
    /// The cells a query box covers, the box itself, and which box test the
    /// predicate's semantics allow.
    ///
    /// All three travel together because all three are computed once, here,
    /// while ranking. The cells decide **where** the read looks; the box decides
    /// which of what it finds is worth fetching; the relation is what makes the
    /// box test a superset of the predicate rather than a second, subtly
    /// different predicate.
    Region {
        /// The covering of the query box.
        cells: Vec<Cell>,
        /// The query box, which the stored boxes are compared against.
        bounds: Bounds,
        /// Which comparison the predicate's semantics permit.
        relation: Relation,
    },
}

impl Served {
    /// Which shape this is.
    pub(crate) const fn shape(&self) -> Shape {
        match self {
            Self::Equality(_) => Shape::Equality,
            Self::Prefix(_) => Shape::Prefix,
            Self::Terms(_) => Shape::Terms,
            Self::Range { .. } => Shape::Range,
            Self::Region { .. } => Shape::Region,
        }
    }

    /// How many of the index's fields this lookup narrows.
    ///
    /// One for a shape that reaches only the leading field, the length of the
    /// run for an equality tuple, and for a range the fixed run **plus one** for
    /// the field it bounds. It is a **proof of narrowing**, not an estimate: the
    /// entries matching two fixed fields are a subset of those matching the
    /// first alone, and the entries a range keeps are a subset of the run it
    /// walks — whatever the data holds. That is why it ranks candidates and a
    /// guess would not.
    pub(crate) fn fixed(&self) -> usize {
        match self {
            Self::Equality(values) => values.len(),
            Self::Prefix(_) | Self::Terms(_) | Self::Region { .. } => 1,
            Self::Range { fixed, .. } => fixed.len().saturating_add(1),
        }
    }
}

/// How many records a candidate can produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rows {
    /// A ceiling that was free to learn.
    AtMost(u64),
    /// No ceiling without doing the work the candidate would do anyway.
    Unknown,
}

impl Rows {
    /// Which of two bounds promises fewer records.
    ///
    /// Every known ceiling beats every unknown one, which is the whole ranking
    /// in a sentence. Two unknowns are equal here and the shape breaks the tie.
    fn rank(self, other: Self) -> Ordering {
        match (self, other) {
            (Self::AtMost(held), Self::AtMost(theirs)) => held.cmp(&theirs),
            (Self::AtMost(_), Self::Unknown) => Ordering::Less,
            (Self::Unknown, Self::AtMost(_)) => Ordering::Greater,
            (Self::Unknown, Self::Unknown) => Ordering::Equal,
        }
    }
}

/// One conjunct an index could serve, with everything the executor needs.
///
/// The bound is already evaluated, because ranking needs it — and evaluating it
/// again in the executor would let a `time::now()` in a filter mean two
/// different instants inside one statement.
///
/// It carries no path. Enumeration used the path to find the index and to reach
/// the field's analyzer; by the time a candidate exists both are on it, and a
/// field nothing reads is a field that goes out of step with what does.
#[derive(Debug, Clone)]
pub(crate) struct Candidate {
    /// What the read needs, in the form that read takes.
    pub(crate) served: Served,
    /// The index that would serve it.
    pub(crate) index: IndexDefinition,
    /// How many records it can produce.
    pub(crate) rows: Rows,
}

/// The word a shape answers under, for a plan somebody is reading.
impl Shape {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Equality => "equality",
            Self::Prefix => "prefix",
            Self::Range => "range",
            Self::Region => "region",
            Self::Terms => "terms",
        }
    }
}

/// The candidate that promises to narrow the most.
///
/// `None` when nothing can be served by an index, which is the scan.
///
/// Total and pure: it reads no store and can therefore be tested over the whole
/// ranking matrix directly, rather than inferred from how long a query took.
pub(crate) fn choose(candidates: Vec<Candidate>) -> Option<Candidate> {
    // A later candidate must be strictly better to displace an earlier one, so
    // an equal one loses and source order survives — which is what makes the
    // plan predictable from the condition the author wrote.
    candidates
        .into_iter()
        .reduce(|best, next| if better(&next, &best) { next } else { best })
}

/// Whether the first candidate promises fewer records than the second.
///
/// The order of the three tests is the whole rule, and the middle one moved
/// here. A ceiling decides first because it is a counted fact. Then the number
/// of fields narrowed, because **that is a proof** — a subset cannot be larger
/// than the set it is drawn from — and a proof outranks the shape ranking, which
/// its own doc describes as what a candidate is *trusted* to narrow when nothing
/// exact is known. Shape decides last, among candidates that narrow equally
/// many fields, and an equal shape still falls through to source order.
///
/// Placing the count below the shape, as it was, made a composite range
/// unreachable: `a = 1 AND b > 2` on `(a, b)` offers an equality fixing one
/// field and a range fixing one and bounding a second, and `Shape::Equality`
/// sorts before `Shape::Range`, so the narrower candidate lost to the wider one
/// on a heuristic. No decision this store made before changes, because every
/// candidate that narrowed more than one field was an equality, which the shape
/// ranking already preferred.
fn better(candidate: &Candidate, than: &Candidate) -> bool {
    match candidate.rows.rank(than.rows) {
        Ordering::Less => true,
        Ordering::Greater => false,
        Ordering::Equal => match candidate.served.fixed().cmp(&than.served.fixed()) {
            Ordering::Greater => true,
            Ordering::Less => false,
            Ordering::Equal => candidate.served.shape() < than.served.shape(),
        },
    }
}

/// Every declared index whose **leading** field is this path, of the kind that
/// can answer the question asking.
///
/// An index serves a condition on its first field, whether or not it has others:
/// the key encoding puts that field first and byte order is value order, so the
/// entries for one value of it are contiguous. Only the first — the entries for
/// one value of a *later* field are scattered across every value of the ones
/// before it, so an index offered for that would be answering about the wrong
/// column.
///
/// **Every** such index, not the first one declared, and that is the wave's
/// restructuring in one line. Taking the first meant an index was never asked
/// what it could serve; it was handed a conjunct. So a composite `(last, first)`
/// was invisible whenever a plain `last` happened to be declared before it, and
/// a field carrying both a search index and an ordered one could be served by
/// neither — the search index won the search and then failed the equality guard.
///
/// A `Vec` rather than an iterator because the list is a handful of definitions
/// and a borrowing iterator here would tie the caller's hands for nothing.
fn serving<'a>(
    declared: &'a [IndexDefinition],
    path: &Path,
    search: bool,
) -> Vec<&'a IndexDefinition> {
    declared
        .iter()
        .filter(|index| {
            index.fields.first() == Some(path)
                && if search {
                    index.search
                } else {
                    // Not `!index.search`: a vector or spatial index is not a
                    // search index and is not an ordered one either, and asking
                    // the negative admitted both. See `IndexDefinition::is_ordered`.
                    index.is_ordered()
                }
        })
        .collect()
}

/// Every declared index that can bound a range on this path, with the leading
/// run of values the condition fixes before it.
///
/// An index qualifies when `path` is one of its fields **and** every field
/// before it is fixed to an exact value by the condition. That is what makes the
/// entries the range walks contiguous: the key puts the fixed values first, so
/// fixing all of them names one run, and inside that run the entries are ordered
/// by the very field being bounded.
///
/// Position zero is the ordinary case and yields an empty run — a range on the
/// index's own leading field, which is every range this store served before.
/// Position `p > 0` with any of `0..p` unfixed does **not** qualify: the tags in
/// range would be scattered across every value of the fields before them, and
/// finding them means visiting each run's slice in turn. That is a different
/// traversal and it is deliberately not built here.
///
/// Only an ordered index qualifies at all. A search, vector or spatial index
/// holds terms, a graph or cells rather than an order over the value, so a range
/// over one would not be a scan wearing an index's name — it would be a lookup
/// in a keyspace that is not keyed by the value, answering with fewer rows.
fn ranged<'a>(
    declared: &'a [IndexDefinition],
    path: &Path,
    fixed: &BTreeMap<&Path, &Value>,
) -> Vec<(&'a IndexDefinition, Vec<Value>)> {
    let mut found = Vec::new();
    for index in declared {
        if !index.is_ordered() {
            continue;
        }
        let Some(at) = index.fields.iter().position(|field| field == path) else {
            continue;
        };
        let mut run = Vec::with_capacity(at);
        for field in index.fields.iter().take(at) {
            let Some(held) = fixed.get(field) else {
                break;
            };
            run.push((*held).clone());
        }
        if run.len() == at {
            found.push((index, run));
        }
    }
    found
}

/// Every declared spatial index on this path.
///
/// Asked **positively**, like [`IndexDefinition::is_ordered`] and for the same
/// reason: a guard over index kinds that names the ones it skips admits every
/// kind added after it, and the symptom is a read answering with fewer rows and
/// no error at all. Asking `index.spatial` cannot go wrong that way — a seventh
/// kind is excluded here by default, which is slow and correct.
///
/// The leading field only, as everywhere else. A spatial index keys by the cells
/// covering one geometry, so a route that is not the one it was built on is a
/// question about a different column.
fn spatial<'a>(declared: &'a [IndexDefinition], path: &Path) -> Vec<&'a IndexDefinition> {
    declared
        .iter()
        .filter(|index| index.spatial && index.fields.first() == Some(path))
        .collect()
}

/// The leading run of this index's fields the condition fixes to a value.
///
/// It stops at the first field nothing fixes, so the result is always a genuine
/// prefix of the index — which is what `Transaction::records_by_index` requires
/// and what makes "complete" mean the same thing on both sides.
fn gathered(index: &IndexDefinition, fixed: &BTreeMap<&Path, &Value>) -> Vec<Value> {
    let mut values = Vec::new();
    for field in &index.fields {
        let Some(held) = fixed.get(field) else {
            break;
        };
        values.push((*held).clone());
    }
    values
}

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
            if seek.shape == Shape::Equality {
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
            match seek.shape {
                // `seekable` builds a `Seek` only from a comparison, so a region
                // never arrives here — it is gathered by `regional` below,
                // because a geometric relation has no operator to be a `Seek`
                // about. The arm is written out rather than left to a wildcard:
                // a wildcard would swallow the next shape somebody adds, and
                // swallowing it silently is how a kind ends up unserved with
                // nothing to say so.
                Shape::Region => continue,
                Shape::Equality => {
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
                Shape::Prefix => {
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
                Shape::Range => {
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
                Shape::Terms => {
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

/// One `geo::` conjunct a spatial index could narrow with.
///
/// Its own walk rather than a fifth [`Shape`] on [`Seek`], because a geometric
/// relation is not a comparison: it has no [`BinaryOp`], and giving `Seek` an
/// optional one would put a branch in every arm that reads it which no input can
/// reach. The store's own rule about [`Served`] applies here too — a shape that
/// cannot exist without its argument is a sum, not a struct of optional fields.
struct Regional<'a> {
    path: &'a Path,
    /// The other argument, which must be a constant.
    query: &'a Expr,
    /// Which box test this predicate's semantics permit, with the record's
    /// argument already normalised into first position.
    relation: Relation,
}

/// The `geo::` conjuncts of a condition a spatial index could serve.
///
/// Walks `AND` only, exactly as [`seekable`] does and for the same reasons:
/// under `OR` neither side alone narrows the answer, and under `NOT` an index
/// that finds the matching records finds precisely the wrong set.
fn regional(condition: &Expr) -> Vec<Regional<'_>> {
    match &condition.kind {
        ExprKind::And(left, right) => {
            let mut found = regional(left);
            found.extend(regional(right));
            found
        }
        ExprKind::Call {
            function,
            arguments,
            ..
        } => {
            let [one, other] = arguments.as_slice() else {
                return Vec::new();
            };
            // Which argument names the record, and which is the query. Both
            // being paths means neither is a constant and there is nothing to
            // look up; both being constants means the whole call is a constant
            // and no index is involved either.
            let (field, query, field_first) = match (&one.kind, &other.kind) {
                (ExprKind::Path(field), _) if !reads_a_record(other) => (field, other, true),
                (_, ExprKind::Path(field)) if !reads_a_record(one) => (field, one, false),
                _ => return Vec::new(),
            };
            let Some(relation) = relation_of(*function, field_first) else {
                return Vec::new();
            };
            vec![Regional {
                path: &field.path,
                query,
                relation,
            }]
        }
        _ => Vec::new(),
    }
}

/// Which box test a predicate allows, once it is known which argument is the
/// record's.
///
/// # Why the argument's position changes the answer
///
/// `geo::within(at, Q)` asks whether the record lies inside the query, and
/// `geo::within(Q, at)` asks the opposite. Writing it the second way is ordinary
/// — it reads as "the query is within the shape" — and a planner that only
/// understood the first would leave half the natural phrasings on the scan.
///
/// # Why `geo::disjoint` is not here
///
/// It is the complement of a region, and a complement has no box test that is a
/// superset of it: every record whose box misses the query is disjoint, and so
/// is every record whose box *meets* it but whose geometry does not. There is no
/// set of cells that holds them, so the answer is the scan — which is exact, and
/// says so. A relation invented for it would drop rows silently, which is the
/// one thing a filter must never do.
const fn relation_of(function: Function, field_first: bool) -> Option<Relation> {
    match (function, field_first) {
        // Symmetric: which argument is the record does not change the question.
        // `touches` shares the `intersects` test because two shapes that touch
        // share a position, so their boxes meet — a superset, which is the only
        // property a filter relation has to have.
        (Function::GeoIntersects | Function::GeoTouches, _) => Some(Relation::Meets),
        (Function::GeoEquals, _) => Some(Relation::Same),
        (Function::GeoWithin | Function::GeoCoveredBy, true)
        | (Function::GeoContains | Function::GeoCovers, false) => Some(Relation::Inside),
        (Function::GeoContains | Function::GeoCovers, true)
        | (Function::GeoWithin | Function::GeoCoveredBy, false) => Some(Relation::Around),
        _ => None,
    }
}

/// One conjunct an index could narrow with.
struct Seek<'a> {
    path: &'a Path,
    value: &'a Expr,
    shape: Shape,
    /// Which comparison it was, which a range needs and the others do not: the
    /// direction and whether the end is inclusive both live here.
    op: BinaryOp,
}

/// The conjuncts of a condition an index could serve, outermost first.
///
/// Only `AND` is walked into. Under `OR` neither side alone narrows the
/// answer — a record satisfying the other half would be missed — and under `NOT`
/// an index that finds the matching records is exactly the wrong set. Both are
/// left to the scan rather than served with a bound that would be a guess.
///
/// A right-hand side that reads the record is not a constant and cannot be a
/// bound, so it is excluded here rather than discovered when it is evaluated
/// without a record in scope.
fn seekable(condition: &Expr) -> Vec<Seek<'_>> {
    match &condition.kind {
        ExprKind::And(left, right) => {
            let mut found = seekable(left);
            found.extend(seekable(right));
            found
        }
        ExprKind::Binary { op, left, right } => {
            let ExprKind::Path(field) = &left.kind else {
                return Vec::new();
            };
            // A route holding `[*]` needs no guard of its own here, and that is
            // worth saying rather than leaving to be rediscovered. An index is
            // matched to a condition by **exact route equality** below, so
            // `tags[*]` matches only an index declared on `tags[*]` — a multikey
            // index, which keeps one entry per element. An ordinary index over
            // `tags` holds one entry for the whole array and is declared on
            // `tags`, so it cannot be offered here and cannot answer a question
            // about elements with an answer about arrays. The matching rule *is*
            // the guard, which is why the condition still takes the scan when no
            // multikey index exists.
            if reads_a_record(right) {
                return Vec::new();
            }
            let shape = match op {
                BinaryOp::Equal => Shape::Equality,
                BinaryOp::Like => Shape::Prefix,
                BinaryOp::Matches => Shape::Terms,
                // The four orderings are a bounded scan over the ordered index,
                // which is safe because byte order **is** value order
                // (`docs/key-grammar.md` §1).
                BinaryOp::Less | BinaryOp::LessOrEqual => Shape::Range,
                BinaryOp::Greater | BinaryOp::GreaterOrEqual => Shape::Range,
                _ => return Vec::new(),
            };
            vec![Seek {
                path: &field.path,
                value: right,
                shape,
                op: *op,
            }]
        }
        _ => Vec::new(),
    }
}

/// Whether an expression reads the record being tested.
fn reads_a_record(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Path(_) => true,
        ExprKind::Not(inner) | ExprKind::Negate(inner) => reads_a_record(inner),
        ExprKind::And(left, right) | ExprKind::Or(left, right) => {
            reads_a_record(left) || reads_a_record(right)
        }
        ExprKind::Arithmetic { left, right, .. } | ExprKind::Binary { left, right, .. } => {
            reads_a_record(left) || reads_a_record(right)
        }
        ExprKind::Call { arguments, .. } => arguments.iter().any(reads_a_record),
        ExprKind::Array(items) | ExprKind::Set(items) => items.iter().any(reads_a_record),
        ExprKind::Object(fields) => fields.iter().any(|field| reads_a_record(&field.value)),
        ExprKind::Range(range) => reads_a_record(&range.start) || reads_a_record(&range.end),
        // A fold reads records, so it is not constant and must never be
        // evaluated once above the loop.
        //
        // Today nothing asks: a projection holding a fold is answered by the
        // grouped path, which never reaches the constant-folding pass. Answering
        // `false` here would still be a statement about the world that is
        // wrong — this question is "can this be computed without records", and
        // for a fold it cannot — and the day the two paths are reordered, a
        // wrong `false` becomes a fold evaluated with no group to fold over.
        ExprKind::Fold { .. } => true,
        // A parameter is a value, so it reads no record — and after binding
        // there is none left to ask.
        ExprKind::Literal(_)
        | ExprKind::Parameter(_)
        | ExprKind::Table(_)
        | ExprKind::Record(_)
        | ExprKind::Get(_)
        | ExprKind::Select(_) => false,
    }
}

/// Every top-level field name an expression reads, however deeply nested.
///
/// The neighbour of [`reads_a_record`], answering the finer question: not
/// *whether* the tree touches the record but **which** of its fields. Kept
/// beside it deliberately — two walks over the same tree that drifted apart
/// would each still compile, and the disagreement would surface as a read that
/// silently loses its ordering.
///
/// Exhaustive over `ExprKind` with no wildcard, so a new expression kind is a
/// compile error here rather than a node this walk quietly steps over.
pub(crate) fn roots_read(expr: &Expr, into: &mut BTreeSet<String>) {
    match &expr.kind {
        ExprKind::Path(field) => {
            into.insert(field.path.root().to_owned());
        }
        ExprKind::Not(inner) | ExprKind::Negate(inner) => roots_read(inner, into),
        ExprKind::And(left, right)
        | ExprKind::Or(left, right)
        | ExprKind::Arithmetic { left, right, .. }
        | ExprKind::Binary { left, right, .. } => {
            roots_read(left, into);
            roots_read(right, into);
        }
        ExprKind::Call { arguments, .. } => {
            for argument in arguments {
                roots_read(argument, into);
            }
        }
        ExprKind::Array(items) | ExprKind::Set(items) => {
            for item in items {
                roots_read(item, into);
            }
        }
        ExprKind::Object(fields) => {
            for field in fields {
                roots_read(&field.value, into);
            }
        }
        ExprKind::Range(range) => {
            roots_read(&range.start, into);
            roots_read(&range.end, into);
        }
        ExprKind::Fold { over, .. } => {
            if let Some(over) = over {
                roots_read(over, into);
            }
        }
        ExprKind::Literal(_)
        | ExprKind::Parameter(_)
        | ExprKind::Table(_)
        | ExprKind::Record(_)
        | ExprKind::Get(_)
        | ExprKind::Select(_) => {}
    }
}

/// Evaluate the parts of an expression that do not depend on a record, once.
///
/// # Why this is here and not in the evaluator
///
/// It is the same judgement the index selection makes — [`reads_a_record`]
/// decides whether a filter's right-hand side can be an index bound, and it
/// answers exactly the question a fold asks. Two notions of "constant" in one
/// query engine is one more than can be kept in step.
///
/// # What it was worth
///
/// The benchmark harness measured a nearest-neighbour read over 2000 records at
/// 12.5 ms and decomposed it: sorting the same records by a plain path costs
/// 1.5 ms, and the same distance with a **one**-component query vector costs
/// 2.3 ms. The cost tracked the size of the literal rather than the arithmetic
/// done on it, because a `[…32 numbers]` written once in the statement was being
/// rebuilt from the syntax tree for every record — 2000 fresh arrays of 32
/// values to re-create something that never changed.
///
/// # It cannot change an answer, with one exception that improves one
///
/// A constant folds to the value it already evaluated to. The exception is
/// `time::now()`, which was evaluated per record — so one statement could
/// observe two instants and sort by them. Folded, one statement observes one
/// instant, which is what a read should mean.
impl Session<'_> {
    /// This expression with its record-independent parts already evaluated.
    pub(crate) fn folded(&self, transaction: &mut Transaction<'_>, expr: &Expr) -> Result<Expr> {
        if !reads_a_record(expr) {
            // Already a literal: folding would rebuild an identical node and
            // lose nothing but time.
            if matches!(expr.kind, ExprKind::Literal(_)) {
                return Ok(expr.clone());
            }
            let held = self.evaluate(transaction, expr)?;
            return Ok(Expr {
                // The original span, so an error still points where the author
                // wrote rather than where the fold put it.
                span: expr.span,
                kind: ExprKind::Literal(held),
            });
        }
        // It reads the record, so only its children can be constant.
        let kind = match &expr.kind {
            ExprKind::Not(inner) => ExprKind::Not(self.boxed(transaction, inner)?),
            ExprKind::Negate(inner) => ExprKind::Negate(self.boxed(transaction, inner)?),
            ExprKind::And(left, right) => ExprKind::And(
                self.boxed(transaction, left)?,
                self.boxed(transaction, right)?,
            ),
            ExprKind::Or(left, right) => ExprKind::Or(
                self.boxed(transaction, left)?,
                self.boxed(transaction, right)?,
            ),
            ExprKind::Binary { op, left, right } => ExprKind::Binary {
                op: *op,
                left: self.boxed(transaction, left)?,
                right: self.boxed(transaction, right)?,
            },
            ExprKind::Arithmetic { op, left, right } => ExprKind::Arithmetic {
                op: *op,
                left: self.boxed(transaction, left)?,
                right: self.boxed(transaction, right)?,
            },
            ExprKind::Call {
                function,
                arguments,
                span,
            } => ExprKind::Call {
                function: *function,
                arguments: self.each(transaction, arguments)?,
                span: *span,
            },
            ExprKind::Array(items) => ExprKind::Array(self.each(transaction, items)?),
            // A set and an object are left alone. Both are built from their
            // items the way an array is, and neither appears in the positions
            // this pass exists for; folding them would be reach for its own sake.
            _ => return Ok(expr.clone()),
        };
        Ok(Expr {
            kind,
            span: expr.span,
        })
    }

    /// The same fold applied to each expression a projection evaluates.
    ///
    /// A fold is left alone: it answers after the per-record pass has finished,
    /// so it never reaches the loop this exists to relieve.
    pub(crate) fn folded_projection(
        &self,
        transaction: &mut Transaction<'_>,
        wanted: &[Projected],
    ) -> Result<Vec<Projected>> {
        let mut held = Vec::with_capacity(wanted.len());
        for projected in wanted {
            held.push(Projected {
                value: self.folded(transaction, &projected.value)?,
                name: projected.name.clone(),
            });
        }
        Ok(held)
    }

    /// The same fold applied to each key an order sorts by.
    ///
    /// One place rather than two, because both the streaming ordering stage and
    /// the one fed from a collected vector need it, and two copies would be two
    /// chances for a key to be folded differently from the record it is compared
    /// against.
    pub(crate) fn folded_order(
        &self,
        transaction: &mut Transaction<'_>,
        order: &[tessari_ql::Ordering],
    ) -> Result<Vec<Expr>> {
        let mut folded = Vec::with_capacity(order.len());
        for key in order {
            folded.push(self.folded(transaction, &key.key)?);
        }
        Ok(folded)
    }

    fn boxed(&self, transaction: &mut Transaction<'_>, expr: &Expr) -> Result<Box<Expr>> {
        Ok(Box::new(self.folded(transaction, expr)?))
    }

    fn each(&self, transaction: &mut Transaction<'_>, items: &[Expr]) -> Result<Vec<Expr>> {
        items
            .iter()
            .map(|item| self.folded(transaction, item))
            .collect()
    }
}

/// A read a vector index could serve, when the statement asks for one.
///
/// Recognised rather than requested: the language has no nearest-neighbour
/// operator, because "the ten most similar" is an order and a bound and it
/// already had both (SGC.T4 W1). So the index's job is to notice that shape and
/// answer it faster — and to notice it **only** when the statement said
/// `APPROXIMATE`, because a graph's answer is not the scan's.
///
/// Every condition below is a way the shape can fail to be the one a graph
/// answers, and each is a scan rather than a guess:
///
/// - no `APPROXIMATE`, so the caller has not accepted an approximate ordering;
/// - more than one sort key, or a descending one — a distance orders ascending,
///   and a second key orders records the graph never ranked;
/// - no `LIMIT`, so the read wants every record and a walk has nothing to cut;
/// - a sort key that is not a distance call on a path and a constant;
/// - `GROUP BY`, which folds the records a walk would have chosen between.
pub(crate) struct Nearest<'a> {
    /// The field holding the vectors.
    pub(crate) path: &'a Path,
    /// The query vector, still an expression.
    pub(crate) query: &'a Expr,
    /// Which distance the statement asked for.
    pub(crate) distance: Function,
    /// How many records to walk for, `START` included.
    pub(crate) wanted: usize,
}

/// The nearest-neighbour read this statement is, if it is one.
pub(crate) fn nearest(select: &Select) -> Option<Nearest<'_>> {
    if !select.approximate || !select.group.is_empty() {
        return None;
    }
    let [ordering] = select.order.as_slice() else {
        return None;
    };
    if ordering.descending {
        return None;
    }
    let ExprKind::Call {
        function,
        arguments,
        ..
    } = &ordering.key.kind
    else {
        return None;
    };
    // `dot` is excluded: the inner product grows with similarity, so ordering by
    // it ascending asks for the *least* similar — a query the language allows
    // and a graph of nearest neighbours does not answer.
    if !matches!(function, Function::VectorCosine | Function::VectorEuclidean) {
        return None;
    }
    let (Some(first), Some(second)) = (arguments.first(), arguments.get(1)) else {
        return None;
    };
    let ExprKind::Path(field) = &first.kind else {
        return None;
    };
    if reads_a_record(second) {
        return None;
    }
    let limit = select.limit?;
    // A `START` skips records the walk still has to find, so it is added to what
    // the walk asks for rather than making the read unservable.
    let wanted = limit.saturating_add(select.start.unwrap_or(0));
    Some(Nearest {
        path: &field.path,
        query: second,
        distance: *function,
        wanted: usize::try_from(wanted).unwrap_or(usize::MAX),
    })
}

/// A read a spatial index could serve nearest-first, when the statement asks for
/// one.
///
/// Recognised rather than requested, the same way the vector shape is: the
/// language has no nearest operator because "the ten closest" is an order and a
/// bound and it already had both.
///
/// Unlike the vector walk this one is **exact**, so it does not ask the caller
/// to accept anything. A best-first traversal ordered by a true floor visits
/// every record that could rank above the ones it holds, so the records it
/// answers with are the records a scan answers with — which is why `APPROXIMATE`
/// is refused here rather than required. That keyword is the vector shape and it
/// is recognised on its own.
///
/// Every other condition below is a way the shape can fail to be the one a walk
/// answers, and each is a scan rather than a guess — the same list
/// [`ordered`] refuses for the same reasons:
///
/// - more than one sort key, or a descending one: a distance orders ascending,
///   and a second key orders records the walk never ranked;
/// - no `LIMIT`, so the read wants every record and a walk has nothing to stop
///   at;
/// - a sort key that is not `geo::distance` on a field and something constant;
/// - `GROUP BY`, which folds the records a walk would have chosen between;
/// - a projection, because the sort runs after it and may name what the
///   projection produced rather than what the index holds;
/// - a `FETCH`, which replaces a reference with the record it names before the
///   sort sees it.
pub(crate) struct Closest<'a> {
    /// The field holding the geometries.
    pub(crate) path: &'a Path,
    /// The position measured from, still an expression.
    pub(crate) query: &'a Expr,
    /// How many records to walk for, `START` included.
    pub(crate) wanted: usize,
}

/// The nearest-first read this statement is, if it is one.
pub(crate) fn closest(select: &Select) -> Option<Closest<'_>> {
    if select.approximate || !select.group.is_empty() || !select.fetch.is_empty() {
        return None;
    }
    if !matches!(select.projection, Projection::All) {
        return None;
    }
    let [ordering] = select.order.as_slice() else {
        return None;
    };
    if ordering.descending {
        return None;
    }
    let ExprKind::Call {
        function,
        arguments,
        ..
    } = &ordering.key.kind
    else {
        return None;
    };
    if !matches!(function, Function::GeoDistance) {
        return None;
    }
    let [first, second] = arguments.as_slice() else {
        return None;
    };
    // Either argument may hold the field. A distance is symmetric, so unlike the
    // relate predicates there is nothing to normalise — but a planner that
    // recognised only `geo::distance(at, here)` would be correct and silently
    // unindexed for `geo::distance(here, at)`, which is an equally ordinary way
    // to write the same question and reports nothing when it is slower.
    let (field, query) = match (&first.kind, &second.kind) {
        (ExprKind::Path(field), _) if !reads_a_record(second) => (field, second),
        (_, ExprKind::Path(field)) if !reads_a_record(first) => (field, first),
        _ => return None,
    };
    let limit = select.limit?;
    // A `START` skips records the walk still has to find, so it is added to what
    // the walk asks for rather than making the read unservable.
    let wanted = limit.saturating_add(select.start.unwrap_or(0));
    Some(Closest {
        path: &field.path,
        query,
        wanted: usize::try_from(wanted).unwrap_or(usize::MAX),
    })
}

/// A bounded ordered read an index could serve.
///
/// # An index is already in the order a sort wants
///
/// The index and the sort use one order — the value system's — so an ordered
/// index read backwards produces its records in the order the statement asked
/// for, and a `LIMIT` stops it. Nothing here is a new order; what is new is
/// reading the one that was already stored instead of throwing it away.
///
/// # Why the direction is not a symmetry, and where the door was
///
/// A sort places **every** record, including those whose key is absent, and the
/// value system puts `none` below every value — but a record with no value has
/// **no index entry** (`index::project` yields nothing for it). Descending, the
/// absences come last, so a bounded read never reaches them while the index
/// fills the bound. Ascending, they come *first*: the records an ascending
/// bounded read answers with are exactly the ones the index does not hold.
///
/// So ascending was refused here until wave 40, when the door this comment
/// named — *a `REQUIRED` field, where there are no absences* — was measured
/// rather than assumed and turned out to be real: `DEFINE FIELD … REQUIRED` is
/// **refused against a table already holding a record without the field**, so
/// the invariant holds at declaration as well as at every write after it.
///
/// The direction therefore **travels on the bound** rather than being decided
/// here, because whether it is servable is a question about the *schema* and
/// this function is a pure function of the statement. Every caller must read
/// [`Bounded::descending`]: `index_serving_order` refuses an ascending bound
/// over a field that is not `REQUIRED`, and the walk under a `WHERE` refuses an
/// ascending bound outright, because it retries past its bound and the entries
/// it would retry over are not the ones an ascending answer needs.
///
/// # Every condition below is a way the answer could change
///
/// Each is a scan rather than a guess, and each is refused here — where the
/// judgement is a pure function of the statement and can be tested without a
/// store:
///
/// - more than one sort key;
/// - a key that is not a plain route into the record — a computed key is not
///   what any index holds, and a `[*]` route denotes several values, which is
///   several entries per record;
/// - no `LIMIT`, so the read wants every record and there is nothing to stop;
/// - `GROUP BY`, which folds the records the order would have chosen between;
/// - a projection, because the sort runs *after* it and may name what the
///   projection produced rather than what the index holds;
/// - a `FETCH`, which replaces a reference with the record it names before the
///   sort sees it — so the key the index holds is not the key that would sort;
/// - `APPROXIMATE`, which is the vector shape and is recognised on its own.
pub(crate) struct Bounded<'a> {
    /// The field the order is over.
    pub(crate) path: &'a Path,
    /// How many records the bound needs, `START` included.
    pub(crate) wanted: usize,
    /// Which way the order runs.
    ///
    /// Carried rather than decided here: whether an **ascending** bound is
    /// servable depends on the field being `REQUIRED`, which is a fact about the
    /// schema and not about the statement. A caller that ignores this field
    /// would hand an ascending bound to a descending walk, so every one reads
    /// it.
    pub(crate) descending: bool,
}

/// The bounded ordered read this statement is, if it is one.
pub(crate) fn ordered(select: &Select) -> Option<Bounded<'_>> {
    if select.approximate || !select.group.is_empty() || !select.fetch.is_empty() {
        return None;
    }
    if !matches!(select.projection, Projection::All) {
        return None;
    }
    let [ordering] = select.order.as_slice() else {
        return None;
    };
    let ExprKind::Path(field) = &ordering.key.kind else {
        return None;
    };
    if field.path.is_several() {
        return None;
    }
    let limit = select.limit?;
    // A `START` passes over records the read still has to find, so it is added
    // to the bound rather than making the read unservable.
    let wanted = limit.saturating_add(select.start.unwrap_or(0));
    Some(Bounded {
        path: &field.path,
        wanted: usize::try_from(wanted).unwrap_or(usize::MAX),
        descending: ordering.descending,
    })
}

/// How many records the source may stop at, when the statement's shape lets a
/// bound reach it at all (ADR-0013 mechanism 1).
///
/// # Why this is a whitelist and never a blacklist
///
/// `START` and `LIMIT` are applied last — after the source, the `FETCH`, the
/// projection and the sort — so a bound handed to the source is only the same
/// answer when nothing in between changes how many records there are. Two
/// clauses do:
///
/// - a **grouping or a fold** turns many records into one, so the limit counts
///   groups and cutting the source cuts the grouping's input instead;
/// - an **ordering the source does not serve** decides which records survive,
///   so the first *n* found and the first *n* in that order are different sets.
///
/// Both failures are a **quietly short answer**: real records, fewer of them,
/// returned with nothing raised. That asymmetry is why the rule is written as
/// the shapes that are allowed rather than the shapes that are not — a clause
/// added to this language later is a missed optimisation under a whitelist and
/// a silent wrong answer under a blacklist.
///
/// `FETCH` is allowed through: it maps one record to one record.
pub(crate) fn bound(select: &Select) -> Option<usize> {
    if !select.group.is_empty() || !select.order.is_empty() {
        return None;
    }
    if let Projection::Values(wanted) = &select.projection
        && crate::aggregate::folds(wanted)
    {
        return None;
    }
    let limit = select.limit?;
    // A `START` passes over records the source still has to produce, so it is
    // part of the bound rather than something applied to it afterwards.
    let wanted = limit.saturating_add(select.start.unwrap_or(0));
    Some(usize::try_from(wanted).unwrap_or(usize::MAX))
}

/// Whether an index's declared distance answers this statement's.
///
/// A graph whose edges were chosen by one measure approximates that measure and
/// no other, so a mismatch is a scan — exact, and reported as such.
pub(crate) const fn answers(declared: VectorDistance, asked: Function) -> bool {
    matches!(
        (declared, asked),
        (VectorDistance::Cosine, Function::VectorCosine)
            | (VectorDistance::Euclidean, Function::VectorEuclidean)
    )
}

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
        }
        Ok(crate::outcome::Outcome::Value(Value::Object(plan)))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use tessari_geo::{Bounds, Cell, Relation, Snapped};
    use tessari_storage::IndexDefinition;
    use tessari_types::{DatabaseId, IndexId, NamespaceId, Path, TableId, Value};

    use super::{Candidate, Rows, Served, Shape, choose};

    fn index(name: &str, unique: bool, search: bool) -> IndexDefinition {
        IndexDefinition {
            id: IndexId::new(1),
            namespace: NamespaceId::new(1),
            database: DatabaseId::new(1),
            table: TableId::new(1),
            name: name.to_owned(),
            fields: vec![Path::field("x")],
            search,
            unique,
            vector: None,
            spatial: false,
        }
    }

    fn candidate(name: &str, shape: Shape, rows: Rows) -> Candidate {
        let served = match shape {
            Shape::Equality => Served::Equality(vec![Value::from("x")]),
            Shape::Prefix => Served::Prefix("x".to_owned()),
            Shape::Terms => Served::Terms(vec!["x".to_owned()]),
            Shape::Range => Served::Range {
                fixed: Vec::new(),
                lower: Some(Value::from("a")),
                upper: Some(Value::from("z")),
            },
            Shape::Region => Served::Region {
                cells: vec![Cell::root()],
                bounds: Bounds::of_position(
                    Snapped::from_units(0, 0).expect("the origin is on the grid"),
                ),
                relation: Relation::Meets,
            },
        };
        Candidate {
            served,
            index: index(
                name,
                shape == Shape::Equality && rows != Rows::Unknown,
                shape == Shape::Terms,
            ),
            rows,
        }
    }

    fn winner(candidates: Vec<Candidate>) -> String {
        choose(candidates).expect("a candidate").index.name
    }

    #[test]
    fn nothing_to_serve_is_the_scan() {
        assert!(choose(Vec::new()).is_none());
    }

    #[test]
    fn a_known_ceiling_beats_an_unknown_one_whichever_was_written_first() {
        // The case the whole module exists for: `email` is unique and selects
        // one record, `city` is not and was written first.
        assert_eq!(
            winner(vec![
                candidate("by_city", Shape::Equality, Rows::Unknown),
                candidate("by_email", Shape::Equality, Rows::AtMost(1)),
            ]),
            "by_email"
        );
        assert_eq!(
            winner(vec![
                candidate("by_email", Shape::Equality, Rows::AtMost(1)),
                candidate("by_city", Shape::Equality, Rows::Unknown),
            ]),
            "by_email"
        );
    }

    #[test]
    fn the_smaller_of_two_known_ceilings_wins() {
        assert_eq!(
            winner(vec![
                candidate("wide", Shape::Terms, Rows::AtMost(900)),
                candidate("narrow", Shape::Terms, Rows::AtMost(3)),
            ]),
            "narrow"
        );
    }

    #[test]
    fn an_ordered_range_ranks_with_a_prefix_and_below_a_value() {
        // Both are ranges that can be the whole table, and neither's size is
        // knowable without doing the read.
        assert_eq!(
            winner(vec![
                candidate("by_range", Shape::Range, Rows::Unknown),
                candidate("by_value", Shape::Equality, Rows::Unknown),
            ]),
            "by_value"
        );
        assert_eq!(
            winner(vec![
                candidate("by_prefix", Shape::Prefix, Rows::Unknown),
                candidate("by_range", Shape::Range, Rows::Unknown),
            ]),
            "by_prefix",
            "ties keep the one written first"
        );
    }

    #[test]
    fn a_value_beats_a_range_when_neither_is_known() {
        // A prefix range can be most of the table — `LIKE 'a%'` — where an
        // equality is bounded by the records holding one value.
        assert_eq!(
            winner(vec![
                candidate("by_prefix", Shape::Prefix, Rows::Unknown),
                candidate("by_value", Shape::Equality, Rows::Unknown),
            ]),
            "by_value"
        );
        assert_eq!(
            winner(vec![
                candidate("by_value", Shape::Equality, Rows::Unknown),
                candidate("by_prefix", Shape::Prefix, Rows::Unknown),
            ]),
            "by_value"
        );
    }

    #[test]
    fn a_known_ceiling_beats_a_range_however_large_the_ceiling_is() {
        // Deliberately: an unknown is unknown, and a term held by nine hundred
        // documents is still a promise where `LIKE 'a%'` is not.
        assert_eq!(
            winner(vec![
                candidate("by_prefix", Shape::Prefix, Rows::Unknown),
                candidate("by_terms", Shape::Terms, Rows::AtMost(900)),
            ]),
            "by_terms"
        );
    }

    #[test]
    fn two_equal_candidates_keep_the_one_written_first() {
        // So that two runs of one statement cannot disagree, and an author can
        // predict the plan from the condition they wrote.
        assert_eq!(
            winner(vec![
                candidate("first", Shape::Equality, Rows::AtMost(1)),
                candidate("second", Shape::Equality, Rows::AtMost(1)),
            ]),
            "first"
        );
        assert_eq!(
            winner(vec![
                candidate("first", Shape::Prefix, Rows::Unknown),
                candidate("second", Shape::Prefix, Rows::Unknown),
            ]),
            "first"
        );
    }

    #[test]
    fn a_ceiling_of_zero_is_the_best_candidate_there_is() {
        // A term nothing holds: the read is empty, and no other candidate can
        // beat producing nothing.
        assert_eq!(
            winner(vec![
                candidate("by_email", Shape::Equality, Rows::AtMost(1)),
                candidate("by_terms", Shape::Terms, Rows::AtMost(0)),
            ]),
            "by_terms"
        );
    }

    // The tests above exercise the ranking on its own. These exercise the other
    // half — that enumeration puts the right ceiling on each kind of candidate —
    // because a perfect rule fed a wrong `Rows` chooses wrongly and quietly.

    use std::sync::Arc;

    use tessari_kv::{KvBackend, MemoryBackend};
    use tessari_ql::{Expr, Source, StatementKind, parse};
    use tessari_storage::{Catalog, Store};

    use crate::search::Searched;
    use crate::session::Session;

    fn store() -> Store {
        let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
        Store::open(backend).expect("a store")
    }

    /// `users` with a unique index on `email`, a secondary one on `city`, and a
    /// search index over an analysed `body`.
    fn ready(store: &Store) -> Session<'_> {
        let mut session = Session::new(store);
        session
            .run(
                "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
                 DEFINE DATABASE orders; USE DATABASE orders;\n\
                 DEFINE ANALYZER simple FILTERS lowercase;\n\
                 DEFINE TABLE users;\n\
                 DEFINE FIELD body ON users TYPE string ANALYZER simple;\n\
                 DEFINE INDEX by_email ON users FIELDS email UNIQUE;\n\
                 DEFINE INDEX by_city ON users FIELDS city;\n\
                 DEFINE INDEX by_name ON users FIELDS name;\n\
                 DEFINE INDEX by_body ON users FIELDS body SEARCH;\n\
                 CREATE users:1 = { email: 'a@x', city: 'london', name: 'ada', body: 'lock' };\n\
                 CREATE users:2 = { email: 'b@x', city: 'london', name: 'anne', body: 'lock' };",
            )
            .expect("a schema");
        session
    }

    /// The `WHERE` of a read, parsed the way a statement parses it.
    ///
    /// Not `parse_expression`: in a value position a bare name is a **table**
    /// reference, and only the condition parser reads one as a route into the
    /// record. Building the condition any other way would test a shape the
    /// language never produces.
    fn condition_of(written: &str) -> Expr {
        let script = parse(&format!("SELECT * FROM users WHERE {written};")).expect("a statement");
        let Some(StatementKind::Select(select)) =
            script.statements.first().map(|held| held.kind.clone())
        else {
            panic!("not a read");
        };
        match select.from {
            Source::Where { condition, .. } => *condition,
            other => panic!("not a filtered read: {other:?}"),
        }
    }

    /// Which index the planner picks for this condition, and on what ceiling.
    fn planned(session: &Session<'_>, store: &Store, written: &str) -> (String, Rows) {
        let condition = condition_of(written);
        let mut transaction = store.begin().expect("a transaction");
        let table = Catalog::new(&mut transaction)
            .table_id(
                tessari_types::NamespaceId::new(1),
                tessari_types::DatabaseId::new(1),
                "users",
            )
            .expect("a lookup")
            .expect("the table");
        let declared = Catalog::new(&mut transaction)
            .indexes_on(table)
            .expect("the indexes");
        let searched = session
            .searched_for(&mut transaction, table, &[&condition])
            .expect("the searched context");
        let offered = session
            .enumerate(&mut transaction, &condition, &declared, &searched)
            .expect("the candidates");
        let chosen = choose(offered).expect("a candidate");
        (chosen.index.name, chosen.rows)
    }

    #[test]
    fn a_unique_equality_is_chosen_over_one_written_before_it() {
        let store = store();
        let session = ready(&store);
        assert_eq!(
            planned(&session, &store, "city = 'london' AND email = 'a@x'"),
            ("by_email".to_owned(), Rows::AtMost(1))
        );
        // And the same the other way round, which is the point: the plan is not
        // a function of where the author put the clause.
        assert_eq!(
            planned(&session, &store, "email = 'a@x' AND city = 'london'"),
            ("by_email".to_owned(), Rows::AtMost(1))
        );
    }

    #[test]
    fn an_equality_is_chosen_over_a_prefix_range_written_before_it() {
        let store = store();
        let session = ready(&store);
        for written in [
            "name LIKE 'a%' AND city = 'london'",
            "city = 'london' AND name LIKE 'a%'",
        ] {
            assert_eq!(
                planned(&session, &store, written).0,
                "by_city",
                "for {written}"
            );
        }
    }

    #[test]
    fn a_term_carries_a_real_ceiling_and_wins_when_it_is_small() {
        // `df` is a cheap exact count, so a search candidate is the one kind of
        // unknown-shaped test that arrives with a number.
        let store = store();
        let session = ready(&store);
        let (name, rows) = planned(&session, &store, "city = 'london' AND body MATCHES 'lock'");
        assert_eq!(name, "by_body");
        assert_eq!(rows, Rows::AtMost(2));

        // A term nothing holds beats everything, because the read is empty.
        let (name, rows) = planned(
            &session,
            &store,
            "email = 'a@x' AND body MATCHES 'unheardof'",
        );
        assert_eq!(name, "by_body");
        assert_eq!(rows, Rows::AtMost(0));
    }

    #[test]
    fn a_condition_no_index_can_serve_offers_nothing() {
        let store = store();
        let session = ready(&store);
        let condition = condition_of("nickname = 'ada'");
        let mut transaction = store.begin().expect("a transaction");
        let declared = Vec::new();
        let offered = session
            .enumerate(
                &mut transaction,
                &condition,
                &declared,
                &Searched::default(),
            )
            .expect("the candidates");
        assert!(offered.is_empty());
        assert!(choose(offered).is_none());
    }
}
