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

use core::cmp::Ordering;

use bgv_db_ql::{BinaryOp, Expr, ExprKind, Function, Projectable, Projected, Select};
use bgv_db_storage::{IndexDefinition, Transaction, VectorDistance};
use bgv_db_types::{Path, Value};

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
    /// The value the index is looked up by.
    Equality(Value),
    /// The literal prefix the range starts at.
    Prefix(String),
    /// The terms whose postings are intersected.
    Terms(Vec<String>),
}

impl Served {
    /// Which shape this is.
    pub(crate) const fn shape(&self) -> Shape {
        match self {
            Self::Equality(_) => Shape::Equality,
            Self::Prefix(_) => Shape::Prefix,
            Self::Terms(_) => Shape::Terms,
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
fn better(candidate: &Candidate, than: &Candidate) -> bool {
    match candidate.rows.rank(than.rows) {
        Ordering::Less => true,
        Ordering::Greater => false,
        Ordering::Equal => candidate.served.shape() < than.served.shape(),
    }
}

impl Session<'_> {
    /// Every conjunct one of these indexes could serve, with what it promises.
    ///
    /// A bound is evaluated **here**, once, and carried on the candidate — both
    /// because ranking a search candidate needs it, and because evaluating it
    /// again in the executor would let a `time::now()` in a filter mean two
    /// different instants inside one statement.
    pub(crate) fn enumerate(
        &self,
        transaction: &mut Transaction<'_>,
        condition: &Expr,
        declared: &[IndexDefinition],
        searched: &Searched,
    ) -> Result<Vec<Candidate>> {
        let mut offered = Vec::new();
        for seek in seekable(condition) {
            // A right-hand side that reads the record is not a constant, so it
            // cannot be a bound; `seekable` has already excluded those.
            let bound = self.evaluate(transaction, seek.value)?;
            let Some(index) = declared
                .iter()
                .find(|index| index.fields.as_slice() == core::slice::from_ref(seek.path))
            else {
                continue;
            };
            // An ordered index answers an equality and a prefix; it cannot answer
            // a term, and a search index cannot answer either of the other two.
            // Asking the wrong one would return the wrong rows rather than none.
            let (served, rows) = match seek.shape {
                Shape::Equality if !index.search => (
                    Served::Equality(bound),
                    // A unique index holds one entry per value, so an equality on
                    // one produces at most one record. That is the only ceiling
                    // in this function that costs nothing at all to know.
                    if index.unique {
                        Rows::AtMost(1)
                    } else {
                        Rows::Unknown
                    },
                ),
                Shape::Prefix if !index.search => {
                    let Value::String(pattern) = &bound else {
                        continue;
                    };
                    let Some(prefix) = literal_prefix(pattern) else {
                        continue;
                    };
                    (Served::Prefix(prefix), Rows::Unknown)
                }
                Shape::Terms if index.search => {
                    let (Value::String(query), Some(analyzer)) =
                        (&bound, searched.analyzer(seek.path))
                    else {
                        continue;
                    };
                    let terms = analyzer.terms(query);
                    if terms.is_empty() {
                        continue;
                    }
                    // The intersection of the postings cannot be larger than the
                    // smallest of them, and a document frequency is a count of
                    // keys rather than a set of decoded ids — so this ceiling is
                    // real and cheap. Were it expensive, a search candidate would
                    // have to rank by shape like the others.
                    let mut smallest = u64::MAX;
                    for term in &terms {
                        let held = transaction.document_frequency(index, term)?;
                        smallest = smallest.min(held);
                    }
                    (Served::Terms(terms), Rows::AtMost(smallest))
                }
                _ => continue,
            };
            offered.push(Candidate {
                served,
                index: index.clone(),
                rows,
            });
        }
        Ok(offered)
    }
}

/// One conjunct an index could narrow with.
struct Seek<'a> {
    path: &'a Path,
    value: &'a Expr,
    shape: Shape,
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
            if reads_a_record(right) {
                return Vec::new();
            }
            let shape = match op {
                BinaryOp::Equal => Shape::Equality,
                BinaryOp::Like => Shape::Prefix,
                BinaryOp::Matches => Shape::Terms,
                // An ordered index can serve `<` and `>` as a bounded range, and
                // this does not build it: that needs a bounded scan on
                // `Transaction` and an equivalence test of its own. Reported as
                // a scan until it does, rather than served as a guess.
                _ => return Vec::new(),
            };
            vec![Seek {
                path: &field.path,
                value: right,
                shape,
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
        ExprKind::Literal(_)
        | ExprKind::Table(_)
        | ExprKind::Record(_)
        | ExprKind::Get(_)
        | ExprKind::Select(_) => false,
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
            let value = match &projected.value {
                Projectable::Value(expr) => Projectable::Value(self.folded(transaction, expr)?),
                other => other.clone(),
            };
            held.push(Projected {
                value,
                name: projected.name.clone(),
            });
        }
        Ok(held)
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

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use bgv_db_storage::IndexDefinition;
    use bgv_db_types::{DatabaseId, IndexId, NamespaceId, Path, TableId, Value};

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
        }
    }

    fn candidate(name: &str, shape: Shape, rows: Rows) -> Candidate {
        let served = match shape {
            Shape::Equality => Served::Equality(Value::from("x")),
            Shape::Prefix => Served::Prefix("x".to_owned()),
            Shape::Terms => Served::Terms(vec!["x".to_owned()]),
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

    use bgv_db_kv::{KvBackend, MemoryBackend};
    use bgv_db_ql::{Expr, Source, StatementKind, parse};
    use bgv_db_storage::{Catalog, Store};

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
                bgv_db_types::NamespaceId::new(1),
                bgv_db_types::DatabaseId::new(1),
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
