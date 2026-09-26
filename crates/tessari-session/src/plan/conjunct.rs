use tessari_geo::Relation;
use tessari_ql::{BinaryOp, Expr, ExprKind, Function, Span};
use tessari_types::Path;

use super::reads::reads_a_record;

/// One `geo::` conjunct a spatial index could narrow with.
///
/// Its own walk rather than a fifth variant of [`Comparison`], because a
/// geometric relation is not a comparison: it has no [`BinaryOp`], and giving
/// [`Seek`] an optional one would put a branch in every arm that reads it which
/// no input can reach. The store's own rule about `Served` applies here too — a
/// shape that cannot exist without its argument is a sum, not a struct of
/// optional fields.
pub(super) struct Regional<'a> {
    pub(super) path: &'a Path,
    /// The other argument, which must be a constant.
    pub(super) query: &'a Expr,
    /// Which box test this predicate's semantics permit, with the record's
    /// argument already normalised into first position.
    pub(super) relation: Relation,
    /// For a radius read, the distance the query's box is widened by before the
    /// box test — `geo::distance(at, Q) < r` is served as "boxes meeting `Q`'s
    /// box widened by `r`", and the distance itself is re-tested on what that
    /// finds. `None` for a relate predicate, whose box is the query's own.
    pub(super) widened_by: Option<&'a Expr>,
}

/// The `geo::` conjuncts of a condition a spatial index could serve.
///
/// Walks `AND` only, exactly as [`seekable`] does and for the same reasons:
/// under `OR` neither side alone narrows the answer, and under `NOT` an index
/// that finds the matching records finds precisely the wrong set.
pub(super) fn regional(condition: &Expr) -> Vec<Regional<'_>> {
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
                widened_by: None,
            }]
        }
        ExprKind::Binary { op, left, right } => radius(*op, left, right).into_iter().collect(),
        _ => Vec::new(),
    }
}

/// A radius read — `geo::distance(at, Q) < r` — as a regional conjunct.
///
/// Both strict and non-strict bounds, written either way round (`r > …` is the
/// same question), and the distance with the record's field as either argument,
/// since a distance is symmetric. A lower bound (`> r`) is **not** here: it is
/// the complement of a disc, and like `geo::disjoint` it has no box holding it.
///
/// The radius must be a constant of the statement; a radius read from the
/// record would differ per candidate and name no box at all.
fn radius<'a>(op: BinaryOp, left: &'a Expr, right: &'a Expr) -> Option<Regional<'a>> {
    let (measured, bound) = match op {
        BinaryOp::Less | BinaryOp::LessOrEqual => (left, right),
        BinaryOp::Greater | BinaryOp::GreaterOrEqual => (right, left),
        _ => return None,
    };
    if reads_a_record(bound) {
        return None;
    }
    let ExprKind::Call {
        function: Function::GeoDistance,
        arguments,
        ..
    } = &measured.kind
    else {
        return None;
    };
    let [one, other] = arguments.as_slice() else {
        return None;
    };
    let (field, query) = match (&one.kind, &other.kind) {
        (ExprKind::Path(field), _) if !reads_a_record(other) => (field, other),
        (_, ExprKind::Path(field)) if !reads_a_record(one) => (field, one),
        _ => return None,
    };
    Some(Regional {
        path: &field.path,
        query,
        relation: Relation::Meets,
        widened_by: Some(bound),
    })
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
///
/// # Every function is named, and there is no catch-all
///
/// There used to be a `_ => None`, and it read as the safe default in the same
/// way `Needs::of`'s did. It is safe in the sense that a predicate falling
/// through it is answered by the scan, which is exact — and unsafe in the sense
/// that nothing says so. `geo::touches` was added and the compiler named only
/// the one exhaustive match in the crate; had it not been wired here by hand it
/// would have taken the scan on every read, correctly and silently, with no
/// test asserting the gap and no build reporting it.
///
/// So the match is exhaustive. Adding a function to the language will not
/// compile until somebody says whether a box test is a superset of it, and
/// answering `None` is then a recorded decision rather than an omission.
const fn relation_of(function: Function, field_first: bool) -> Option<Relation> {
    match function {
        // Symmetric: which argument is the record does not change the question.
        // `touches` shares the `intersects` test because two shapes that touch
        // share a position, so their boxes meet — a superset, which is the only
        // property a filter relation has to have.
        Function::GeoIntersects | Function::GeoTouches => Some(Relation::Meets),
        Function::GeoEquals => Some(Relation::Same),
        Function::GeoWithin | Function::GeoCoveredBy => {
            if field_first {
                Some(Relation::Inside)
            } else {
                Some(Relation::Around)
            }
        }
        Function::GeoContains | Function::GeoCovers => {
            if field_first {
                Some(Relation::Around)
            } else {
                Some(Relation::Inside)
            }
        }
        // `geo::disjoint` for the reason above. The other two geometric
        // functions answer a **number** rather than a relation, so there is no
        // predicate for a box test to be a superset of; `geo::distance` is
        // served nearest-first instead, which is a different access path
        // entirely.
        Function::GeoDisjoint | Function::GeoDistance | Function::GeoArea | Function::GeoCell => {
            None
        }
        // Nothing else is a predicate over two shapes.
        Function::StringLen
        | Function::StringLower
        | Function::StringUpper
        | Function::StringTrim
        | Function::StringConcat
        | Function::StringSplit
        | Function::StringSlice
        | Function::StringLines
        | Function::StringReplace
        | Function::MathSqrt
        | Function::MathPow
        | Function::ArrayLen
        | Function::ArrayFirst
        | Function::ArrayLast
        | Function::ObjectKeys
        | Function::ObjectValues
        | Function::ObjectLen
        | Function::ArrayDistinct
        | Function::ArraySort
        | Function::ArrayReverse
        | Function::ArrayFlatten
        | Function::ArrayJoin
        | Function::ArraySlice
        | Function::MathAbs
        | Function::MathFloor
        | Function::MathCeil
        | Function::MathRound
        | Function::TimeNow
        | Function::TimeYear
        | Function::TimeMonth
        | Function::TimeDay
        | Function::TimeHour
        | Function::TimeMinute
        | Function::TimeSecond
        | Function::TimeUnix
        | Function::TimeFromUnix
        | Function::RandUuid
        | Function::CryptoSha256
        | Function::CryptoSha512
        | Function::TimeBucket
        | Function::TypeOf
        | Function::TypeBool
        | Function::TypeInt
        | Function::TypeFloat
        | Function::TypeString
        | Function::TypeDatetime
        | Function::TypeUuid
        | Function::VectorCosine
        | Function::VectorEuclidean
        | Function::VectorDot
        | Function::SearchScore
        | Function::SearchRanks
        | Function::CryptoMd5
        | Function::CryptoSha1
        | Function::EncodingBase64
        | Function::EncodingBase64Decode
        | Function::EncodingHex
        | Function::EncodingHexDecode
        | Function::StringStartsWith
        | Function::StringEndsWith
        | Function::StringContains
        | Function::StringIndexOf
        | Function::StringReverse
        | Function::StringTrimStart
        | Function::StringTrimEnd
        | Function::MathMin
        | Function::MathMax
        | Function::MathSign
        | Function::MathTrunc
        | Function::MathLn
        | Function::MathExp
        | Function::ArrayConcat
        | Function::ArrayAppend
        | Function::ArrayIndexOf
        | Function::ArrayMin
        | Function::ArrayMax
        | Function::ArraySum
        | Function::ObjectEntries
        | Function::ObjectHas
        | Function::ObjectMerge
        | Function::SearchHighlight => None,
    }
}

/// Which comparison a conjunct is, of the four an index can be asked about.
///
/// Its own type rather than the wider `Shape` a candidate ends up with, because
/// a [`Seek`] is built from a [`BinaryOp`] and there is no operator a region is
/// written with — a geometric relation is a call, gathered by [`regional`].
/// Carrying the wider type meant the enumeration matched an arm no input could
/// reach, and an unreachable arm is a branch no test can exercise and therefore
/// none can keep right. Here the fifth case is not written rather than written
/// and skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Comparison {
    /// `<path> = <constant>`
    Equality,
    /// `<path> LIKE '<literal>%'`
    Prefix,
    /// `<path> MATCHES '<text>'`
    Terms,
    /// `<path> MATCHES PREFIX '<text>'`
    ///
    /// Separate from [`Comparison::Terms`] because it is served by a different
    /// structure: the term dictionary is walked to find which words the query
    /// reaches, and only then are their posting lists read.
    PrefixTerms,
    /// `<path> MATCHES FUZZY '<text>'`
    ///
    /// Served by the same structure as [`Comparison::PrefixTerms`] and kept
    /// apart from it because the walk is not the same walk: it reads every term
    /// sharing the query's mandatory prefix and keeps only those inside the edit
    /// budget, so what it reads and what it returns are two numbers rather than
    /// one.
    FuzzyTerms,
    /// `<path> < <constant>`, and the other three orderings.
    Range,
}

/// One conjunct an index could narrow with.
pub(super) struct Seek<'a> {
    pub(super) path: &'a Path,
    pub(super) value: &'a Expr,
    pub(super) comparison: Comparison,
    /// Which comparison it was, which a range needs and the others do not: the
    /// direction and whether the end is inclusive both live here.
    pub(super) op: BinaryOp,
    /// Where this clause is in the statement.
    ///
    /// Carried so that a candidate can say **which** conjunct it answers, and
    /// the read can then ask whether that conjunct is the whole condition. A
    /// span is the identity the walk already has: `seekable` descends into
    /// `AND`, so a clause under one has a span strictly inside the condition's
    /// and a lone clause has the condition's own.
    pub(super) span: Span,
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
pub(super) fn seekable(condition: &Expr) -> Vec<Seek<'_>> {
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
            let comparison = match op {
                BinaryOp::Equal => Comparison::Equality,
                BinaryOp::Like => Comparison::Prefix,
                BinaryOp::Matches => Comparison::Terms,
                BinaryOp::MatchesPrefix => Comparison::PrefixTerms,
                BinaryOp::MatchesFuzzy => Comparison::FuzzyTerms,
                // The four orderings are a bounded scan over the ordered index,
                // which is safe because byte order **is** value order
                // (`docs/key-grammar.md` §1).
                BinaryOp::Less | BinaryOp::LessOrEqual => Comparison::Range,
                BinaryOp::Greater | BinaryOp::GreaterOrEqual => Comparison::Range,
                _ => return Vec::new(),
            };
            vec![Seek {
                path: &field.path,
                value: right,
                comparison,
                op: *op,
                span: condition.span,
            }]
        }
        _ => Vec::new(),
    }
}
