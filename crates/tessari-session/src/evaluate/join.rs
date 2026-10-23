//! Joining two sources on their keys.

use std::collections::{BTreeMap, BTreeSet};

use tessari_ql::{Expr, JoinSide, Outer, Select, Source};
use tessari_storage::Transaction;
use tessari_types::{RecordId, Value};

use crate::budget::Deadline;
use crate::condition::boolean;
use crate::error::{Error, Result};
use crate::noticed::Noticed;
use crate::outcome::AccessPath;
use crate::plan::Plan;
use crate::search::Searched;
use crate::session::Session;

use super::{
    Joined, Part, Reporting, Scope, ceiling_reached, collect_by_key, listed, ordered_index_on,
    shown,
};

/// The join clause a read names: its two sides, the key each is matched on,
/// and the condition the matched pairs must also meet.
pub(super) struct JoinClause<'a> {
    pub(super) left: &'a JoinSide,
    pub(super) right: &'a JoinSide,
    pub(super) left_key: &'a tessari_ql::FieldPath,
    pub(super) right_key: &'a tessari_ql::FieldPath,
    /// `LEFT` or `FULL`; `None` for the inner join (ADR-0134 D4).
    pub(super) outer: Option<Outer>,
    pub(super) condition: Option<&'a Expr>,
}

impl Session<'_> {
    /// Two tables matched on a value neither of them stores a pointer for.
    ///
    /// # The map is ordered, and a hash map here would be a silent bug
    ///
    /// [`Value`] derives `Hash` structurally, but its **equality is not
    /// structural**: `Number`'s `PartialEq` is defined as `cmp() == Equal`, so
    /// `3` and `3.0` are equal — deliberately, because a comparison that
    /// disagreed with the order its own index is stored in is the failure this
    /// store keeps refusing.
    ///
    /// Which means `Value` breaks the `Hash`/`Eq` contract: two equal values can
    /// hash differently. A `HashMap` keyed by one would put `3` and `3.0` in
    /// different buckets and the join would **miss matches with no error at
    /// all**. A `BTreeMap` uses `Ord`, which agrees with equality exactly here,
    /// so the join matches what `=` matches — which is the requirement, since a
    /// join is spelled with the same operator.
    ///
    /// The index path below re-tests for a related reason: an index normalises
    /// its encoding, so a lookup can offer candidates the condition would not
    /// accept. Re-testing them is the rule every other index read in this store
    /// already follows — an index narrows and never answers.
    ///
    /// # Which side is read and which is probed
    ///
    /// Rule-based, because the store keeps no row counts and SGB.T4 already
    /// refused a cost model over statistics it would have to invent. An index on
    /// the right side's key means the left side drives and each of its records
    /// probes that index; otherwise the right side is read once into the map and
    /// the left side probes memory. Either way the work is `n + m` rather than
    /// `n × m`, and what happened is reported through [`AccessPath`].
    ///
    /// # An empty answer that a type mistake explains is refused
    ///
    /// `no rows` is the honest answer to a join over data that happens not to
    /// match, and it is also what a join answers when one side stores an
    /// identity as text and the other stores it as a reference. Those two are
    /// indistinguishable to whoever reads the answer and only one of them is a
    /// mistake, so the store separates them: when the answer is empty and the
    /// two sides' key kinds are both non-empty and share nothing, the read
    /// fails with [`Error::JoinKeysDiffer`] instead of answering.
    ///
    /// The rule is deliberately not "refuse as soon as one compared pair
    /// differs". Records here carry no declared type, so one stray value among a
    /// thousand would refuse a join that works — trading a silent wrong answer
    /// for a loud wrong refusal. Making a single mismatched pair *visible*
    /// without failing the read is the note channel's job and belongs with it.
    pub(super) async fn join(
        &self,
        transaction: &mut Transaction<'_>,
        select: &Select,
        clause: JoinClause<'_>,
        reporting: Reporting<'_>,
        within: Option<Deadline>,
    ) -> Result<Joined> {
        let JoinClause {
            left,
            right,
            left_key,
            right_key,
            outer,
            condition,
        } = clause;
        let left_name = left.name().to_owned();
        let right_name = right.name().to_owned();

        // The right side takes one of two shapes. A table carrying an ordered
        // index on the key is **probed**, one left record at a time; anything
        // else is read once into an ordered map. A materialised read is always
        // the second: it has no index of its own, and building one for a single
        // statement would cost more than the map it replaces.
        //
        // Each side is redacted by its own grant — a join is two reads and
        // neither borrows the other's permission. A side that is a read applied
        // its own on the way through.
        let mut probed = None;
        let mut built: BTreeMap<Value, Vec<(RecordId, Value)>> = BTreeMap::new();
        // A full join's right records, until one is matched.
        let mut unmatched: Vec<(RecordId, Value)> = Vec::new();
        let mut matched_right: BTreeSet<RecordId> = BTreeSet::new();
        // A far side on a split table this node holds only part of is gathered
        // once the near side has said which keys it needs (G057 C2).
        let mut far_gathered = None;
        match right {
            JoinSide::Table { table, .. } => {
                let (context, id) = self.resolve_table(transaction, table).await?;
                let visible = self.visible_in(transaction, id).await?;
                // A full join answers every right record, so it reads them all:
                // never probed through an index, and refused on a node holding
                // only part of them.
                if outer == Some(Outer::Full) {
                    self.refuse_reading_a_part(transaction, id, Part::Whole)
                        .await?;
                }
                if self.missing(transaction, id, Part::Whole).await?.is_some() {
                    far_gathered = Some((context, id, visible));
                } else {
                    let index = match outer {
                        Some(Outer::Full) => None,
                        _ => ordered_index_on(transaction, id, right_key).await?,
                    };
                    match index {
                        Some(index) => probed = Some((index, visible, context, id)),
                        None => {
                            let found = transaction
                                .scan_table(context.namespace, context.database, id)
                                .await?;
                            let found = self.records_of(found, &visible)?;
                            if outer == Some(Outer::Full) {
                                unmatched.clone_from(&found);
                            }
                            collect_by_key(&mut built, found, right_key);
                        }
                    }
                }
            }
            JoinSide::Read { read, .. } => {
                let answered = Box::pin(self.read(transaction, read, within, None)).await?;
                reporting.collected.extend(answered.notes);
                reporting
                    .collected
                    .extend(ceiling_reached(read, answered.records.len()));
                if outer == Some(Outer::Full) {
                    unmatched.clone_from(&answered.records);
                }
                collect_by_key(&mut built, answered.records, right_key);
            }
            JoinSide::Joined(join) => {
                let (records, _) = Box::pin(self.joined(
                    transaction,
                    select,
                    join,
                    Reporting {
                        collected: &mut *reporting.collected,
                        noticed: reporting.noticed,
                    },
                    within,
                ))
                .await?;
                if outer == Some(Outer::Full) {
                    unmatched.clone_from(&records);
                }
                collect_by_key(&mut built, records, right_key);
            }
        }

        // A read has no table to resolve an analyzer against, so a scored
        // expression over a joined subquery falls back to the default context
        // rather than borrowing the other side's.
        let (driving, searched) = match left {
            JoinSide::Table { table, .. } => {
                let (context, id) = self.resolve_table(transaction, table).await?;
                let visible = self.visible_in(transaction, id).await?;
                let searched = self.searched_for(transaction, id, &shown(select)).await?;
                let found = self
                    .whole_table(
                        transaction,
                        (context.namespace, context.database),
                        id,
                        reporting.collected,
                    )
                    .await?;
                (self.records_of(found, &visible)?, searched)
            }
            JoinSide::Read { read, .. } => {
                let answered = Box::pin(self.read(transaction, read, within, None)).await?;
                reporting.collected.extend(answered.notes);
                reporting
                    .collected
                    .extend(ceiling_reached(read, answered.records.len()));
                (answered.records, Searched::default())
            }
            JoinSide::Joined(join) => {
                Box::pin(self.joined(
                    transaction,
                    select,
                    join,
                    Reporting {
                        collected: &mut *reporting.collected,
                        noticed: reporting.noticed,
                    },
                    within,
                ))
                .await?
            }
        };

        if let Some((context, id, visible)) = far_gathered {
            let keys: BTreeSet<Value> = driving
                .iter()
                .filter_map(|(_, record)| left_key.path.resolve(record).cloned())
                .collect();
            // No near key, no match: nothing of the far side is needed.
            if !keys.is_empty() {
                let found = self
                    .gathered_by_keys(
                        transaction,
                        (context.namespace, context.database),
                        id,
                        (right_key, keys.into_iter().collect()),
                        &visible,
                        reporting.collected,
                    )
                    .await?;
                collect_by_key(&mut built, self.records_of(found, &visible)?, right_key);
            }
        }

        let mut rows = Vec::new();
        let mut left_kinds = BTreeSet::new();
        let mut matched = false;
        for (id, record) in driving {
            // A left record with nothing at the key matches nothing: `NONE` is a
            // value and the right side would have to carry it to match, which is
            // what an inner join means. An outer join keeps it alone.
            let Some(key) = left_key.path.resolve(&record).cloned() else {
                if outer.is_some() {
                    let row = filed(left, &left_name, record, None);
                    self.keep_joined(
                        transaction,
                        (id, row),
                        condition,
                        (&searched, reporting.noticed),
                        &mut rows,
                    )
                    .await?;
                }
                continue;
            };
            left_kinds.insert(key.type_name());
            let matches: Vec<(RecordId, Value)> = match &probed {
                Some((index, visible, _, _)) => {
                    let offered = transaction
                        .records_by_index(index, core::slice::from_ref(&key))
                        .await?;
                    self.records_of(offered, visible)?
                        .into_iter()
                        .filter(|(_, held)| right_key.path.resolve(held) == Some(&key))
                        .collect()
                }
                None => built.get(&key).cloned().unwrap_or_default(),
            };
            if matches.is_empty() && outer.is_some() {
                // Nothing on the right matched, so the row holds the left record
                // alone — its right side is absent, `NONE`, never `NULL`.
                let row = filed(left, &left_name, record, None);
                self.keep_joined(
                    transaction,
                    (id, row),
                    condition,
                    (&searched, reporting.noticed),
                    &mut rows,
                )
                .await?;
                continue;
            }
            for (far_id, far) in matches {
                matched = true;
                if outer == Some(Outer::Full) {
                    matched_right.insert(far_id);
                }
                let row = filed(
                    left,
                    &left_name,
                    record.clone(),
                    Some((right, &right_name, far)),
                );
                // The left record's id. A row is not a record, and two rows from
                // one left record carry one id — stated in `Source::Join` rather
                // than left to be discovered.
                self.keep_joined(
                    transaction,
                    (id.clone(), row),
                    condition,
                    (&searched, reporting.noticed),
                    &mut rows,
                )
                .await?;
            }
        }
        // A full join then answers each right record nothing matched, alone in
        // its row under its own identity.
        for (far_id, far) in unmatched {
            if matched_right.contains(&far_id) {
                continue;
            }
            let row = filed(right, &right_name, far, None);
            self.keep_joined(
                transaction,
                (far_id, row),
                condition,
                (&searched, reporting.noticed),
                &mut rows,
            )
            .await?;
        }
        // Only here, and only on the answer that was about to lie. A join that
        // produced a row matched a value, and two values that are equal are of
        // one kind, so the sets overlap and this cannot fire; a join that
        // produced nothing is the one whose emptiness needs explaining.
        if !matched {
            let right_kinds = match &probed {
                // The index path never read the right side, so learning what it
                // holds costs a scan. It is paid once, after an empty answer,
                // and never by a join that worked.
                Some((_, visible, context, id)) => {
                    let found = transaction
                        .scan_table(context.namespace, context.database, *id)
                        .await?;
                    self.records_of(found, visible)?
                        .iter()
                        .filter_map(|(_, record)| right_key.path.resolve(record))
                        .map(Value::type_name)
                        .collect()
                }
                None => built.keys().map(Value::type_name).collect::<BTreeSet<_>>(),
            };
            // Both sides must have held something: a join over an empty table
            // has no kinds to reconcile and answers nothing for the ordinary
            // reason.
            if !left_kinds.is_empty()
                && !right_kinds.is_empty()
                && left_kinds.is_disjoint(&right_kinds)
            {
                return Err(Error::JoinKeysDiffer {
                    left_key: left_key.path.to_string(),
                    left_kinds: listed(&left_kinds),
                    right_key: right_key.path.to_string(),
                    right_kinds: listed(&right_kinds),
                    span: select.span,
                });
            }
        }
        // `join`, whichever way the sides were read: neither side's own path is
        // how the joined answer was reached, and reporting one of them named half
        // a read. What is worth naming is the index the right side was **probed**
        // through, because that is the difference between a probe per left record
        // and a map of the whole right table — and `EXPLAIN` names it from the
        // same `ordered_index_on`.
        let plan = Plan {
            index: probed.map(|(index, ..)| index.name),
            ..Plan::new(AccessPath::Join)
        };
        Ok((rows, plan, searched))
    }

    /// The rows of the join written so far, as a side of the next one
    /// (ADR-0134 D14).
    async fn joined(
        &self,
        transaction: &mut Transaction<'_>,
        select: &Select,
        join: &Source,
        reporting: Reporting<'_>,
        within: Option<Deadline>,
    ) -> Result<(Vec<(RecordId, Value)>, Searched)> {
        let Source::Join {
            left,
            right,
            left_key,
            right_key,
            outer,
            condition,
            ..
        } = join
        else {
            return Ok((Vec::new(), Searched::default()));
        };
        let (rows, _, searched) = self
            .join(
                transaction,
                select,
                JoinClause {
                    left,
                    right,
                    left_key,
                    right_key,
                    outer: *outer,
                    condition: condition.as_deref(),
                },
                reporting,
                within,
            )
            .await?;
        Ok((rows, searched))
    }

    /// A joined row kept when the read's `WHERE` holds for it, or has none.
    async fn keep_joined(
        &self,
        transaction: &mut Transaction<'_>,
        (id, row): (RecordId, Value),
        condition: Option<&Expr>,
        (searched, noticed): (&Searched, &Noticed),
        rows: &mut Vec<(RecordId, Value)>,
    ) -> Result<()> {
        if let Some(condition) = condition {
            let held = self
                .evaluate_in(
                    transaction,
                    condition,
                    Scope::searching(&row, searched).noticing(noticed),
                )
                .await?;
            if !boolean(&held, condition.span)? {
                return Ok(());
            }
        }
        rows.push((id, row));
        Ok(())
    }
}

/// One row: each side's record filed under its name, or — for the join so far
/// — the sides it already holds, so the row stays one flat object.
fn filed(
    side: &JoinSide,
    name: &str,
    record: Value,
    other: Option<(&JoinSide, &str, Value)>,
) -> Value {
    let mut row = BTreeMap::new();
    file(&mut row, side, name, record);
    if let Some((side, name, record)) = other {
        file(&mut row, side, name, record);
    }
    Value::Object(row)
}

fn file(row: &mut BTreeMap<String, Value>, side: &JoinSide, name: &str, record: Value) {
    match (side, record) {
        (JoinSide::Joined(_), Value::Object(held)) => row.extend(held),
        (_, record) => {
            row.insert(name.to_owned(), record);
        }
    }
}
