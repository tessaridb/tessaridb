use super::*;

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
    /// The record the page resumes after, when it is not the first page.
    pub(crate) after: Option<&'a tessari_ql::RecordTarget>,
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
pub(super) fn shadowed(projection: &Projection, root: &str) -> bool {
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
    // A read keeping the newest record per key must see every record before
    // it keeps any, so no walk that answers early and no bound may serve it.
    if select.latest.is_some() {
        return None;
    }
    // A cursor is allowed: the walk resumes below the anchor's score
    // (ADR-0100 D1.9), and the ordering stage applies the cursor exactly as it
    // does to a scan's records.
    if select.approximate.is_some() || !select.group.is_empty() || !select.fetch.is_empty() {
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
        after: select.after.as_deref(),
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
    // A read keeping the newest record per key must see every record before
    // it keeps any, so no walk that answers early and no bound may serve it.
    if select.latest.is_some() {
        return None;
    }
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
