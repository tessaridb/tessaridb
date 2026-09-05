use tessari_ql::{Approximation, Expr, ExprKind, Function, Projection, Select};
use tessari_storage::VectorDistance;
use tessari_types::Path;

use super::reads::reads_a_record;

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
    /// What the read is willing to spend, or `None` for the engine's own budget.
    pub(crate) effort: Option<usize>,
}

/// The nearest-neighbour read this statement is, if it is one.
pub(crate) fn nearest(select: &Select) -> Option<Nearest<'_>> {
    let (Some(asked), true, false) = (select.approximate, select.group.is_empty(), resumes(select))
    else {
        return None;
    };
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
        effort: match asked {
            Approximation::Default => None,
            Approximation::Effort(candidates) => Some(candidates),
        },
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
    ///
    /// No budget beside it, and deliberately: this traversal is **exact**, so
    /// there is nothing to trade. `EFFORT` belongs to the one read in this
    /// language that answers approximately.
    pub(crate) wanted: usize,
}

/// The nearest-first read this statement is, if it is one.
pub(crate) fn closest(select: &Select) -> Option<Closest<'_>> {
    if select.approximate.is_some()
        || !select.group.is_empty()
        || !select.fetch.is_empty()
        || resumes(select)
    {
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

/// Whether a cursor makes this statement's bound reach the wrong records.
///
/// Every walk below stops when it has `wanted` records, counted from the
/// **front** of the order. A cursor asks for the records after a position, which
/// is the other end: the first `wanted` an ordered walk finds are exactly the
/// ones a resumed page has already handed back, so the walk would fill its bound
/// with them and answer with nothing.
///
/// So a resumed read declines every bounded walk and reaches its records the one
/// way that cannot be cut short. The seek that makes a cursor cheap lives where
/// the order **is** the store's own — see `Transaction::records_after` — and a
/// read that named an order of its own is walked and says so
/// (`Note::CursorWalked`).
fn resumes(select: &Select) -> bool {
    select.after.is_some()
}

/// The bounded ordered read this statement is, if it is one.
pub(crate) fn ordered(select: &Select) -> Option<Bounded<'_>> {
    if select.approximate.is_some()
        || !select.group.is_empty()
        || !select.fetch.is_empty()
        || resumes(select)
    {
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

/// A bounded **scored** read: the best `k` records for a search query.
///
/// # Why this is a sibling of [`ordered`] and not a widening of it
///
/// [`Bounded`] carries a `path`, and [`ordered`] refuses a computed sort key
/// because "a computed key is not what any index holds". That reason is correct
/// and is left standing: `search::score` is the one computed key an index *does*
/// hold. Admitting a `Call` into the type written to refuse one would leave that
/// doc block contradicted by the code beneath it, so this recognizer is its own
/// function returning its own shape.
///
/// The refusals below are **re-stated rather than inherited** from [`ordered`].
/// Sharing them would couple the two so that relaxing one silently relaxes the
/// other, and they are refused here for reasons of their own. The projection is
/// what that separation was for: [`ordered`] refuses every one and this
/// recognizer refuses only the shadowing ones, for the reason [`shadowed`]
/// gives.
///
/// # Why only descending
///
/// [`Bounded`] carries its direction because whether an ascending bound is
/// servable is a question about the *schema*. Here it is a question about the
/// read: a scored bound keeps the `k` **highest**-scoring records, and pruning
/// works by refusing any term that cannot beat the weakest of them. Ascending
/// asks for the `k` worst matches, which no bound of that shape reaches, and the
/// records scoring lowest are overwhelmingly the ones holding none of the query's
/// terms — which the index does not post at all. So ascending is refused
/// outright rather than carried.
pub(crate) struct Scored<'a> {
    /// The field being searched.
    ///
    /// The query is deliberately not carried with it. Resolving a searched field
    /// already evaluates the query once per read and keeps what it found — the
    /// analysed terms and the collection they are weighed against — so a second
    /// copy here would be a second chance for the walk and the score to disagree
    /// about what was asked.
    pub(crate) field: &'a Path,
    /// How many records the bound needs, `START` included.
    pub(crate) wanted: usize,
}

/// Whether this projection puts something else under the name a score reads.
///
/// [`ordered`] refuses every projection, because a path key reads the *answer's*
/// names and an alias shadowing a field is exactly what `SELECT address.city AS
/// home … ORDER BY home` is for. A score reads a name too — the field it
/// measures — so the same shadow is writable here: `SELECT decoy AS body …
/// ORDER BY search::score(body, 'x')` names a field the answer carries and the
/// index does not hold.
///
/// It is the only shape that has to be refused, and the reason is where the
/// number comes from. A score over an index whose postings carry their payload
/// is computed from those postings and the record's identity, so the record's
/// text is not read at all and a projection has nothing in it to change. Where
/// the record *is* read — an index written before postings had payloads — the
/// ordering stage lays the source record beneath the projection precisely so a
/// key can still reach a field the projection dropped (`consume::reach_past`,
/// Q-143). What that overlay does not undo is a name the projection **offers**:
/// there the projection wins, by design.
///
/// So a projection is admitted unless one of its written names is the root of
/// the field being scored **and** answers with something other than that field.
/// Writing the field out under its own name — `SELECT body, search::score(body,
/// 'x') AS score`, which is how a caller gets the text it is about to
/// highlight — shadows it with itself and changes nothing; refusing that would
/// reproduce this question one step over, where adding the searched field to the
/// answer silently costs the read its bound.
///
/// Q-384, measured in `tests/ranking.rs` rather than argued: a projection that
/// shadows the searched field, and one that drops it, both answer in the indexed
/// field's order.
fn shadowed(projection: &Projection, root: &str) -> bool {
    projection.written().iter().any(|one| {
        one.name.text == root
            && !matches!(
                &one.value.kind,
                ExprKind::Path(field)
                    if field.path.root() == root && field.path.steps().is_empty()
            )
    })
}

/// The bounded scored read this statement is, if it is one.
pub(crate) fn scored(select: &Select) -> Option<Scored<'_>> {
    if select.approximate.is_some()
        || !select.group.is_empty()
        || !select.fetch.is_empty()
        || resumes(select)
    {
        return None;
    }
    let [ordering] = select.order.as_slice() else {
        return None;
    };
    if !ordering.descending {
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
    if !matches!(function, Function::SearchScore) {
        return None;
    }
    // Fixed order, and unlike `geo::distance` there is nothing symmetric to
    // normalise: scoring a record's field against a query is not the same
    // question as scoring the query against the field.
    let [first, query] = arguments.as_slice() else {
        return None;
    };
    let ExprKind::Path(field) = &first.kind else {
        return None;
    };
    if field.path.is_several() {
        return None;
    }
    if shadowed(&select.projection, field.path.root()) {
        return None;
    }
    // A query that reads the record being scored would make the collection the
    // score is measured against depend on the record — a different query per
    // record, and no term set to bound.
    if reads_a_record(query) {
        return None;
    }
    let limit = select.limit?;
    // A `START` passes over records the read still has to find, so it is added to
    // the bound rather than making the read unservable.
    let wanted = limit.saturating_add(select.start.unwrap_or(0));
    Some(Scored {
        field: &field.path,
        wanted: usize::try_from(wanted).unwrap_or(usize::MAX),
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
    if let Projection::Values { values: wanted, .. } = &select.projection
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
