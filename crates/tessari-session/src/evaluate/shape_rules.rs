//! Rules read off a SELECT's shape: what bounds it, what it streams, what it asserts.

use std::collections::BTreeMap;

use tessari_ql::{Expr, FieldPath, Projection, Select, Source, Using};
use tessari_storage::{BUILD_VERSION, Store};
use tessari_types::{Path, RecordId, Step, Value};

use crate::aggregate::folds;
use crate::budget::Budget;
use crate::error::{Error, Result};
use crate::outcome::{AccessPath, Note};
use crate::plan::Plan;

/// The note a materialised read owes, when it reached the ceiling it stated.
///
/// A materialised source and a materialised join side are the same case seen
/// twice — the outer statement asks its question of whatever the inner read
/// handed over, and a prefix of an answer and a whole one are the same shape. A
/// top-level read filling its own `LIMIT` is *not* this: there is no outer
/// question for it to have misled, and the caller wrote the bound and can see
/// how many records came back.
pub(crate) fn ceiling_reached(read: &Select, held: usize) -> Option<Note> {
    let ceiling = read.limit?;
    u64::try_from(held)
        .is_ok_and(|held| held >= ceiling)
        .then_some(Note::SubqueryCeiling { rows: ceiling })
}

/// Whether the read did what the statement said it expected.
///
/// A refusal and never a router: nothing here reaches the planner, and a read
/// with no `USING` is not touched. It is compared against the plan the read
/// **took**, not the one the planner chose, which is the whole difference — an
/// ordered index that could not fill the bound hands the read to the scan, and
/// an assertion checked against the intention would pass in exactly the case it
/// was written to catch.
///
/// The cost of a refused statement is the read it already did. That is the
/// honest semantics and not an oversight: the assertion is about what happened,
/// so it cannot be settled before anything has. Refusing early where the
/// planner's own choice already contradicts the assertion is a real improvement
/// and a separate one (Q-203), because the planner may name a path the read then
/// falls back from.
/// `SPLIT ON <route>` — one record per element of the array the route reaches.
///
/// # What each shape at the route means
///
/// **An array** is the case the clause is for: one record per element, each
/// carrying the element where the array stood, so `GROUP BY tags` after a
/// `SPLIT ON tags` groups by a tag. The identity is carried unchanged onto every
/// row, so an answer may hold one id more than once — which is what "one row per
/// element" means and is why the clause is written rather than implied.
///
/// **An empty array** answers with no rows at all. Zero elements, zero rows: any
/// other rule would make the count depend on a special case, and a read that
/// asked for a row per tag over a record with no tags asked for nothing.
///
/// **Anything else — an absence, a scalar, an object — passes through once,
/// unchanged.** An array says "these are the elements"; an absence says nothing
/// about elements at all, so it is not an empty one. In a store where a field's
/// kind is per record rather than per table, the alternative is a read that
/// refuses because one record out of ten thousand holds a string.
///
/// # The budget
///
/// This is a stage of the read in the sense [`Budget::stage`] means, and it is
/// the one stage that can produce *more* records than it consumed — so it is
/// counted, or a held read could pass its ceiling here after honouring it above.
pub(crate) fn opened(
    records: Vec<(RecordId, Value)>,
    route: &Path,
    budget: &mut Budget,
) -> Result<Vec<(RecordId, Value)>> {
    budget.stage();
    let mut opened = Vec::with_capacity(records.len());
    for (id, record) in records {
        let Some(Value::Array(items)) = route.resolve(&record).cloned() else {
            budget.spend()?;
            opened.push((id, record));
            continue;
        };
        for item in items {
            budget.spend()?;
            let mut row = record.clone();
            if let Some(slot) = route.resolve_mut(&mut row) {
                *slot = item;
            }
            opened.push((id.clone(), row));
        }
    }
    Ok(opened)
}

/// `ONLY` is an assertion about how many records answer, and this is where it is
/// tested.
///
/// After the bound, so `FROM ONLY users LIMIT 1` is the author saying which one
/// they want rather than a contradiction.
///
/// **None passes, more than one refuses**, and the two are not the same mistake.
/// `ONLY` asserts *at most* one, so an absence is a legitimate answer to a
/// question about one thing — refusing it would make
/// `SELECT * FROM ONLY users:99 ?? {}` unsayable, and that is the shape `??`
/// exists for. More than one falsifies what the author wrote, and it refuses
/// rather than answering with the first: the records found are already correct,
/// so a prefix of them costs nothing and looks exactly like success.
/// Whether this read's cursor is served by seeking rather than by walking.
///
/// True for exactly one shape, and the reason is the keyspace rather than a
/// preference: a record's key is its table prefix followed by its identity, so a
/// read that answers in the store's own order can begin at a position in that
/// keyspace. Both halves are load-bearing.
///
/// An `ORDER BY` breaks it because the answer's order is then the key the author
/// named, and a record sorting before the anchor by that key may sort after it
/// by identity — so seeking would drop records the page is owed.
///
/// A source other than a plain table breaks it because its records do not come
/// from that keyspace in that order: a condition may be served by an index, a
/// walk arrives along edges, a join and a materialised read build their rows.
/// Each of those is walked and says so.
pub(crate) fn sought(select: &Select) -> bool {
    select.after.is_some() && select.order.is_empty() && matches!(select.from, Source::Table(_))
}

pub(crate) fn alone(select: &Select, records: &[(RecordId, Value)]) -> Result<()> {
    let Some(span) = select.only else {
        return Ok(());
    };
    if records.len() <= 1 {
        return Ok(());
    }
    Err(Error::NotAlone {
        found: records.len(),
        span,
    })
}

pub(crate) fn asserted(select: &Select, plan: &Plan) -> Result<()> {
    match &select.using {
        None => Ok(()),
        Some(Using::Path(named)) => {
            let Some(wanted) = AccessPath::named(&named.text) else {
                return Err(Error::NoSuchAccessPath {
                    named: named.text.clone(),
                    known: AccessPath::known(),
                    span: named.span,
                });
            };
            if wanted == plan.access {
                return Ok(());
            }
            Err(Error::PathNotTaken {
                expected: wanted.name().to_owned(),
                took: plan.access.name().to_owned(),
                span: named.span,
            })
        }
        Some(Using::Index(named)) => {
            if plan.index.as_deref() == Some(named.text.as_str()) {
                return Ok(());
            }
            Err(Error::IndexNotUsed {
                expected: named.text.clone(),
                // Named rather than described, because "used `by_city`" is what
                // an author has to see to know what went wrong; "no index" is
                // the other thing that can be true and reads as a sentence in
                // the same slot.
                took: plan
                    .index
                    .clone()
                    .map_or_else(|| "no index".to_owned(), |index| format!("`{index}`")),
                span: named.span,
            })
        }
    }
}

/// The table a source names, for the plan that reports it.
///
/// A traversal, a join and a materialised source name none: each reaches records
/// from more than one place, or from a read rather than a table.
pub(crate) fn table_named(source: &Source) -> Option<&str> {
    match source {
        Source::Table(table) | Source::Where { table, .. } | Source::Range { table, .. } => {
            Some(table.name.text.as_str())
        }
        Source::Record(target) => Some(target.table.name.text.as_str()),
        Source::Node | Source::Traverse { .. } | Source::Join { .. } | Source::Subquery { .. } => {
            None
        }
    }
}

/// This node, as the one record `$node` answers.
///
/// The id sits beside the value rather than inside it, which is where a record's
/// id sits everywhere else in this store — so a caller reads it the same way it
/// reads any other answer, and no projection has to learn a special field.
pub(crate) fn node_row(store: &Store) -> Result<(RecordId, Value)> {
    let identity = store.node_identity()?;
    let mut fields = BTreeMap::new();
    // The effective role, matching `INFO FOR NODE` — both are reports of the
    // same fact, and a reader comparing them is entitled to one answer.
    fields.insert(
        "roles".to_owned(),
        Value::Array(
            store
                .effective_roles()?
                .names()
                .into_iter()
                .map(Value::from)
                .collect(),
        ),
    );
    // The one field here that moves. A caller asking what a node is running is
    // asking the same question an upgrade asks, and this is where both look.
    fields.insert(
        "version".to_owned(),
        Value::from(identity.version.to_string().as_str()),
    );
    // Beside it rather than instead of it, because the two answer different
    // questions. `version` is the stored, ordered form an upgrade compares;
    // `build` is what this binary actually is, pre-release suffix included. On
    // a final release they read the same, which is the point — the difference
    // only appears when there is one.
    fields.insert("build".to_owned(), Value::from(BUILD_VERSION));
    fields.insert(
        "endpoints".to_owned(),
        Value::Array(
            identity
                .endpoints
                .iter()
                .map(|endpoint| Value::from(endpoint.as_str()))
                .collect(),
        ),
    );
    Ok((identity.record_id(), Value::Object(fields)))
}

/// Whether the stages left between the source and the answer are all per-record.
///
/// Two are not, and both keep the collecting path: a `FETCH` batches every
/// reference into one ask, which needs every record in hand before the first one
/// is resolved (G004 C9, and ADR-0014 decided that criterion wins); and a
/// grouping folds many records into one.
///
/// A read with no `ORDER BY` keeps it too, and that one is not a barrier — it is
/// that the ordering stage is where the saving lives, and with no key it would
/// order by record id instead, which is a different answer from the one a scan
/// gives.
pub(crate) fn streams(select: &Select) -> bool {
    // `SPLIT` joins `FETCH` on the barrier side rather than becoming a stage of
    // the streaming path: it changes how many records there are, and the
    // ordering stage below it keeps only as many as the bound can still reach.
    // Streamed, the two would decide that together — the sort discarding rows
    // the split had not produced yet.
    // A fused order projects after it orders — its ranks are what
    // `search::ranks()` answers — so it takes the collecting path, where the two
    // stages can be put in that order.
    select.fetch.is_empty()
        && select.split.is_none()
        && !select.order.is_empty()
        && select.fusion.is_none()
        && select.latest.is_none()
        && !groups(select)
}

/// Whether the read folds many records into one.
///
/// Named once and asked twice — by the test above and by the projection stage —
/// because the two must not drift apart. A grouping routed to the streaming path
/// would be a fold evaluated against one record at a time, which is the one
/// thing a fold is not.
pub(crate) fn groups(select: &Select) -> bool {
    match &select.projection {
        Projection::All => false,
        Projection::Values { values, .. } => folds(values) || !select.group.is_empty(),
    }
}

/// How many records a collecting read may stop at, when it may stop at all.
///
/// A `LIMIT` bounds the **answer**. It becomes a bound on the **source** exactly
/// when the records the source produces are, in order, the records the answer
/// holds — and this is the read that has no ordering stage between the two, so
/// for it that is a question about the statement's shape and nothing else.
///
/// Stated as a whitelist, for ADR-0013's reason: a blacklist makes every clause
/// somebody adds later a silent short answer until they remember this function.
/// Each condition names a way the two sets differ. An `ORDER BY` decides which
/// records the answer holds after the source has produced them. An `AFTER`
/// cursor drops records at the front, so a count taken here is not the page's.
/// A `SPLIT` changes how many records there are. A grouping or a fold makes the
/// bound count groups, and stopping the source would cut a group's input instead
/// of the answer. A `FETCH` is excluded because it holds the whole set to batch
/// its references, which is what put this read on the collecting path to begin
/// with.
///
/// Where none of those hold, the answer is a prefix of what the source produced,
/// the rest of the table cannot change it, and reading it cost 83.3 ms to answer
/// with one record found third of a hundred thousand (Q-72).
pub(crate) fn held_bound(select: &Select) -> Option<usize> {
    if !select.order.is_empty()
        || select.after.is_some()
        || !select.fetch.is_empty()
        || select.split.is_some()
        || select.latest.is_some()
        || groups(select)
    {
        return None;
    }
    order_bound(select)
}

/// How many records the ordering stage may keep.
///
/// The start is added because `bounded` skips before it truncates, so a record
/// the start will discard still has to survive the sort to be discarded from the
/// right place.
pub(crate) fn order_bound(select: &Select) -> Option<usize> {
    select.limit.map(|limit| {
        usize::try_from(limit.saturating_add(select.start.unwrap_or(0))).unwrap_or(usize::MAX)
    })
}

/// Whether a route names this field at the top of the record.
///
/// A route with steps below it names something *inside* the field, so the field
/// itself stays — which is why the deeper case is handled after the copy rather
/// than by filtering it out here.
pub(crate) fn omits(omit: &[FieldPath], name: &str) -> bool {
    omit.iter()
        .any(|route| route.path.steps().is_empty() && route.path.root() == name)
}

/// Remove what a route names from inside an already-copied record.
///
/// Silent where the route reaches nothing: a record that does not hold the field
/// already answers without it, and there is nothing for an error to tell anyone.
pub(crate) fn omit_within(fields: &mut BTreeMap<String, Value>, route: &Path) {
    let Some((Step::Field(last), above)) = route.steps().split_last() else {
        return;
    };
    let mut held = Value::Object(std::mem::take(fields));
    let holder = Path::new(route.root().to_owned(), above.to_vec());
    if let Some(Value::Object(inside)) = holder.resolve_mut(&mut held) {
        inside.remove(last);
    }
    if let Value::Object(back) = held {
        *fields = back;
    }
}

/// The expressions a read evaluates besides its condition: what it projects,
/// and what it orders by.
///
/// A fold is left out. `search::score` inside one would be scoring the group
/// rather than the record, which is a different question and is not this one.
pub(crate) fn shown(select: &Select) -> Vec<&Expr> {
    let mut found = Vec::new();
    if let Projection::Values { values: wanted, .. } = &select.projection {
        for projected in wanted {
            found.push(&projected.value);
        }
    }
    for ordering in &select.order {
        found.push(&ordering.key);
    }
    found
}

/// The order a gathered read may hand the shards' leaders, so each sends only
/// its first `LIMIT + START` (ADR-0102) — or `None`, and the records travel.
///
/// A whitelist for [`held_bound`]'s reason. Each refusal names a way a leader's
/// first `n` could miss a record the answer holds: a `FUSE` ranks against every
/// record; an `AFTER` cursor, a `FETCH`, a `SPLIT`, a `LATEST` or a grouping
/// changes which records are ranked or how many there are; a key that reads
/// more than the record cannot be evaluated there; and a key reading a name the
/// projection produces reads a value only this node makes — save a field
/// projected under its own name, which is the field.
///
/// Whether the `WHERE` travels too is the caller's question: only then does the
/// leader rank exactly the records this node keeps.
pub(crate) fn travelling_order(
    select: &Select,
    visible: &crate::redact::Visible,
) -> Option<crate::Ordered> {
    if select.order.is_empty()
        || select.fusion.is_some()
        || select.after.is_some()
        || !select.fetch.is_empty()
        || select.split.is_some()
        || select.latest.is_some()
        || groups(select)
    {
        return None;
    }
    let most = u64::try_from(order_bound(select)?).unwrap_or(u64::MAX);
    let produced: std::collections::BTreeSet<&str> = select
        .projection
        .written()
        .iter()
        .filter(|projected| !names_its_own_field(projected))
        .map(|projected| projected.name.text.as_str())
        .collect();
    let mut keys = Vec::with_capacity(select.order.len());
    for ordering in &select.order {
        let mut read = std::collections::BTreeSet::new();
        crate::plan::roots_read(&ordering.key, &mut read);
        if read.iter().any(|root| produced.contains(root.as_str())) {
            return None;
        }
        let (key, parameters) = tessari_ql::portable(&ordering.key)?;
        keys.push(crate::OrderKey {
            key,
            parameters,
            descending: ordering.descending,
        });
    }
    Some(crate::Ordered {
        visible: visible.clone(),
        keys,
        most,
    })
}

/// Whether a projection is a field under its own name, which a key reads the
/// same on either side.
fn names_its_own_field(projected: &tessari_ql::Projected) -> bool {
    matches!(
        &projected.value.kind,
        tessari_ql::ExprKind::Path(path)
            if path.path.steps().is_empty() && path.path.root() == projected.name.text
    )
}
