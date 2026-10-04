mod scored;

use tessari_ql::{Approximation, Expr, ExprKind, Function, Projection, Select};
use tessari_storage::VectorDistance;
use tessari_types::Path;

use super::reads::reads_a_record;
use scored::shadowed;
pub(crate) use scored::{Scored, bound, scored};

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

/// How many candidates a quantized index is walked for, per record a read keeps.
///
/// The codes choose who is tried and the records' full vectors decide the order
/// — the read's own ordering stage computes the exact distance — so the walk
/// hands over more candidates than the bound and the exact order keeps the best.
///
/// Eight, measured rather than borrowed: on 20 000 clustered 32-dimensional
/// vectors (`benchmarks/2026-10-03-macos-aarch64-vector-quantized.md`) four gave
/// 93.6 % of the exact ten and eight 95.6 %, at the same walk p50 (6.1 against
/// 6.2 ms) — the walk's cost is reading the graph, not resolving the candidates.
pub(crate) const RESCORED: usize = 8;

/// How many records a walk is asked for: the bound, widened on a quantized
/// index so the full vectors have something to choose between.
pub(crate) fn walked_for(wanted: usize, quantized: bool) -> usize {
    if quantized {
        wanted.saturating_mul(RESCORED)
    } else {
        wanted
    }
}

/// The nearest-neighbour read this statement is, if it is one.
pub(crate) fn nearest(select: &Select) -> Option<Nearest<'_>> {
    // A read keeping the newest record per key must see every record before
    // it keeps any, so no walk that answers early and no bound may serve it.
    if select.latest.is_some() {
        return None;
    }
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
/// - a projection that answers under the geometry's **own name** with something
///   else, because the sort runs after it and would then measure a field the
///   index does not hold. Every other projection keeps the walk: the ordering
///   stage lays the source record beneath the projection so a key can reach a
///   field the projection dropped (`consume::reach_past`), and that overlay was
///   built for this statement (Q-143). The blanket refusal that stood here until
///   wave 195 was [`ordered`]'s reason applied to a read that does not share it
///   (Q-390);
/// - a `FETCH`, which replaces a reference with the record it names before the
///   sort sees it.
pub(crate) struct Closest<'a> {
    /// The ordering expression itself, measured on each record the walk takes
    /// so the order is the scan's to the last digit.
    pub(crate) key: &'a Expr,
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
    // A read keeping the newest record per key must see every record before
    // it keeps any, so no walk that answers early and no bound may serve it.
    if select.latest.is_some() {
        return None;
    }
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
    // Asked here rather than at the top, because the name that may be shadowed
    // is the one the ordering measures and that is not known until the field is.
    if shadowed(&select.projection, field.path.root()) {
        return None;
    }
    let limit = select.limit?;
    // A `START` skips records the walk still has to find, so it is added to what
    // the walk asks for rather than making the read unservable.
    let wanted = limit.saturating_add(select.start.unwrap_or(0));
    Some(Closest {
        key: &ordering.key,
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
    // A read keeping the newest record per key must see every record before
    // it keeps any, so no walk that answers early and no bound may serve it.
    if select.latest.is_some() {
        return None;
    }
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
