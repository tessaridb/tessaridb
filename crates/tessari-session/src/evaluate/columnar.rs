//! The columnar seam : when a scan fallback qualifies, the read
//! runs through the projection instead of the row walk.
//!
//! # The gate is the whole safety story (the design record)
//!
//! The columnar path answers through an auxiliary structure, so it is taken
//! only where every doubt has been retired in the row path's favour, and a
//! single `None` from [`Session::columnar_qualifies`] leaves the existing
//! row walk as the answer:
//!
//! - the table's definition carries the column storage kind;
//! - the condition is a top-level `AND` chain of plain comparisons, each
//!   `field <op> constant` (either way around) with `<op>` one of `=`, `!=`,
//!   `<`, `<=`, `>`, `>=`, and every field **declared** on a schemafull
//!   table — anything richer is what the row engine is for;
//! - every field the condition reads is one this session may read: a
//!   grant-hidden field is never pruned on, because pruning would answer
//!   membership questions about a field the row path would redact;
//! - the transaction is at the tail for the table and holds no writes on
//!   it — the projection describes committed state, so these are the same
//!   two refusals the vector gate makes (`evaluate/candidates.rs`);
//! - schemaless columnar tables route to the row walk at v1 (their families
//!   are name-keyed and exist only for fields the table has seen);
//! - `!=` routes to the row walk: a record whose field is null or absent
//!   satisfies it under the row engine's semantics, and the projection
//!   holds no row for such a record, so candidates could never name it.
//!   (The storage scan keeps the operator — over the rows it holds it is
//!   exact — the seam simply cannot use it for this one.)
//!
//! `EXPLAIN` asks the same gate, so the plan it reports is the plan the
//! read takes — one function decides, both callers call it.
//!
//! # Exactness: candidates, then the row
//!
//! The scan hands up candidates. Each one is confirmed against the record
//! at the reader's own snapshot — tombstones, expiry, and anything the
//! projection does not hold are the row's word, never the chunk's — and the
//! whole condition is re-evaluated on the redacted record, exactly as the
//! row walk would. What reaches the consumer is what the walk would have
//! yielded, in the walk's key order; only how it was reached changed.

use tessari_encoding::ColumnFamily;
use tessari_ql::{BinaryOp, Expr, ExprKind, Select, Source};
use tessari_storage::{Catalog, ColumnarCompare, ColumnarPredicate, ColumnarTest, Transaction};
use tessari_types::{RecordId, TableId, Value};

use crate::condition::boolean;
use crate::consume::Consumer;
use crate::context::Context;
use crate::error::Result;
use crate::redact::Visible;
use crate::session::Session;

use super::{Scope, Testing};

/// A read the columnar gate qualified: the predicates it runs with and the
/// partition lane it pins to, when the condition fixes the partition key to
/// one constant (the design record — a read naming one partition reads one
/// lane).
pub(crate) struct ColumnarPlan {
    /// The per-family predicates the scan prunes on.
    pub(crate) predicates: Vec<ColumnarPredicate>,
    /// The lane the partition key's equality names, when it names one.
    pub(crate) pin: Option<u64>,
}

impl Session<'_> {
    /// Whether this read may run through the columnar projection, and the
    /// predicates it would run with. `None` is the common answer — every
    /// row table, every condition the shape check declines, every hidden
    /// field, every stale or writing transaction — and it is always safe.
    pub(crate) async fn columnar_qualifies(
        &self,
        transaction: &mut Transaction<'_>,
        context: Context,
        table: TableId,
        condition: &Expr,
    ) -> Result<Option<ColumnarPlan>> {
        let Some(definition) = Catalog::new(transaction)
            .table(table)
            .await?
            .filter(|found| {
                found.namespace == context.namespace && found.database == context.database
            })
        else {
            return Ok(None);
        };
        if !matches!(
            definition.storage_kind,
            Some(tessari_storage::StorageKind::Column)
        ) {
            return Ok(None);
        }
        let fields = Catalog::new(transaction)
            .fields_on(context.namespace, context.database, table)
            .await?;
        if fields.is_empty() {
            // Schemaless columnar tables project name-keyed families for
            // the fields a record carries; the conservative call routes
            // them to the row walk until the equivalence suite says more.
            return Ok(None);
        }

        let mut conjuncts: Vec<(String, BinaryOp, Value)> = Vec::new();
        if !gather_conjuncts(condition, &mut conjuncts) {
            return Ok(None);
        }
        if conjuncts
            .iter()
            .any(|(_, op, _)| matches!(op, BinaryOp::NotEqual))
        {
            return Ok(None);
        }
        let visible = self.visible_in(transaction, table).await?;
        let mut predicates = Vec::with_capacity(conjuncts.len());
        let mut pin: Option<u64> = None;
        for (name, op, value) in conjuncts {
            let Some(field) = fields.iter().find(|field| field.name == name) else {
                return Ok(None);
            };
            if !field_visible(&visible, &name) {
                return Ok(None);
            }
            let family = ColumnFamily::Id(field.id);
            let test = match op {
                BinaryOp::In => {
                    // A bounded list of non-null constants is one IN probe of
                    // the projection; anything else about the list (a null
                    // member, a bound above the scan's budget) is the row
                    // walk's job — the gate declines and the walk answers
                    // exactly, as it always did.
                    let Value::Array(members) = &value else {
                        return Ok(None);
                    };
                    if members.is_empty()
                        || members.len() > tessari_constants::COLUMNAR_IN_MAX
                        || members
                            .iter()
                            .any(|member| matches!(member, Value::Null | Value::None))
                    {
                        return Ok(None);
                    }
                    ColumnarTest::In {
                        values: members.clone(),
                    }
                }
                BinaryOp::Equal
                | BinaryOp::NotEqual
                | BinaryOp::Less
                | BinaryOp::LessOrEqual
                | BinaryOp::Greater
                | BinaryOp::GreaterOrEqual => {
                    if matches!(value, Value::Null | Value::None) {
                        // Null semantics are the row engine's third-valued
                        // logic; the scan only narrows on non-null constants.
                        return Ok(None);
                    }
                    let compare = match op {
                        BinaryOp::Equal => ColumnarCompare::Equal,
                        BinaryOp::NotEqual => ColumnarCompare::NotEqual,
                        BinaryOp::Less => ColumnarCompare::Less,
                        BinaryOp::LessOrEqual => ColumnarCompare::LessOrEqual,
                        BinaryOp::Greater => ColumnarCompare::Greater,
                        BinaryOp::GreaterOrEqual => ColumnarCompare::GreaterOrEqual,
                        // gather_conjuncts only yields the seven comparisons.
                        _ => return Ok(None),
                    };
                    ColumnarTest::Compare {
                        compare,
                        value: value.clone(),
                    }
                }
                // gather_conjuncts only yields the comparisons above.
                _ => return Ok(None),
            };
            predicates.push(ColumnarPredicate {
                family: family.clone(),
                test,
            });
            // An equality on the partition key names one lane: the scan
            // reads that lane alone (the design record). The constant rides
            // the predicates too — the row authority re-evaluates it, as
            // it does everything the scan narrows on.
            if op == BinaryOp::Equal
                && let Some(partitioning) = definition.partitions.as_ref()
                && partitioning.by == name
            {
                let payload = tessari_encoding::encode_payload(&value).into_bytes();
                let hashed = u64::from(tessari_encoding::murmur2(&payload) & 0x7fff_ffff);
                pin = Some(
                    hashed
                        .checked_rem(u64::from(partitioning.count))
                        .unwrap_or(0),
                );
            }
        }
        if predicates.is_empty() {
            return Ok(None);
        }
        // The projection describes committed state: a transaction behind
        // the tail or holding its own writes on the table reads the row
        // walk, which merges both into what this reader may see.
        if !transaction.indexes_are_current_for(table).await?
            || transaction.writes_in(context.namespace, context.database, table)
        {
            return Ok(None);
        }
        Ok(Some(ColumnarPlan { predicates, pin }))
    }

    /// Run a qualifying read through the columnar projection: scan for
    /// candidates, then confirm each against the row at this snapshot,
    /// re-evaluate the whole condition on the redacted record, and hand
    /// surviving records to the consumer in the row walk's key order.
    pub(super) async fn scan_columnar(
        &self,
        transaction: &mut Transaction<'_>,
        context: Context,
        table: TableId,
        testing: Testing<'_>,
        plan: &ColumnarPlan,
        consumer: &mut impl Consumer,
    ) -> Result<tessari_storage::ColumnarScanReport> {
        let Testing {
            condition,
            searched,
            noticed,
        } = testing;
        let (mut candidates, report) = transaction
            .columnar_scan_partitioned(
                context.namespace,
                context.database,
                table,
                &plan.predicates,
                true,
                plan.pin,
            )
            .await?;
        // The row walk yields in record-key order; the scan owes the
        // consumer the same contract. Decorate once rather than re-encode
        // in every comparison.
        candidates.sort_by_cached_key(tessari_encoding::encode_record_id);
        let visible = self.visible_in(transaction, table).await?;
        let rows = self.rows_in(transaction, table).await?;
        for id in candidates {
            let address = tessari_storage::RecordAddress::new(
                context.namespace,
                context.database,
                table,
                id.clone(),
            );
            // The row authority: a candidate whose record is tombstoned or
            // expired at this snapshot resolves to nothing, and a record
            // the projection only partially describes is read whole here.
            let Some(payload) = transaction.get(&address).await? else {
                continue;
            };
            let record = self.record_of(&payload, &visible)?;
            // The row authority applies here exactly as it does on the row
            // walk: a candidate the guards do not reach is not confirmed.
            let Some(record) = self
                .permitted(transaction, &address.id, record, &visible, &rows)
                .await?
            else {
                continue;
            };
            let held = self
                .evaluate_in(
                    transaction,
                    condition,
                    Scope::searching(&record, searched)
                        .identified(&id)
                        .noticing(noticed),
                )
                .await?;
            if !boolean(&held, condition.span)? {
                continue;
            }
            if consumer.take(transaction, id, record).await?.is_break() {
                break;
            }
        }
        Ok(report)
    }
}

impl Session<'_> {
    /// The groups a qualifying grouped read folds partition-locally : a `GROUP BY` on the partition key, with only
    /// mergeable folds over declared fields, folds per lane over the
    /// projection; the final merge combines the lanes' partials — or `None`
    /// for every other grouped read, which prepares its source as before.
    ///
    /// The gate is deliberately narrow (v1): no `WHERE` (the records path
    /// serves it), one grouping key and it the partition field, folds that
    /// merge exactly and carry no `at`, no `EXPIRE` on the table (the
    /// projection cannot know what expired — at the tail, with no held
    /// writes, the lane rows are the authority for the declared fields), and
    /// the same two snapshot refusals the scan gate makes.
    pub(crate) async fn prepare_columnar_grouped(
        &self,
        transaction: &mut Transaction<'_>,
        select: &Select,
    ) -> Result<Option<(crate::aggregate::Groups, crate::plan::Plan)>> {
        if crate::reduce::reduce_of(select).is_none() {
            return Ok(None);
        }
        let Source::Table(table) = &select.from else {
            return Ok(None);
        };
        // A resolution that refuses (a name the view rewrite did not reach,
        // an unknown table) is not this gate's verdict: the read's ordinary
        // path raises it where it always did.
        let Ok((context, id)) = self.resolve_table(transaction, table).await else {
            return Ok(None);
        };
        let Some(shape) = self
            .columnar_grouped_shape(transaction, context, id, select)
            .await?
        else {
            return Ok(None);
        };
        // The projection describes committed state: same two refusals as the
        // scan gate.
        if !transaction.indexes_are_current_for(id).await?
            || transaction.writes_in(context.namespace, context.database, id)
        {
            return Ok(None);
        }
        let fields = Catalog::new(transaction)
            .fields_on(context.namespace, context.database, id)
            .await?;
        let overs: Vec<(String, ColumnFamily)> = shape
            .overs
            .iter()
            .map(|(name, field_id)| (name.clone(), ColumnFamily::Id(*field_id)))
            .collect();
        let Some(key_field) = fields.iter().find(|field| field.name == shape.key) else {
            return Ok(None);
        };
        let partitioning_count = shape.lanes;
        let key_name = shape.key.clone();
        let key_rows_family = ColumnFamily::Id(key_field.id);
        let _ = &key_rows_family;

        let count = partitioning_count;
        let occurrences = crate::aggregate::occurrences(select.projection.written());
        let mut groups = crate::aggregate::Groups::new();
        for lane in 0..count {
            let key_rows: std::collections::BTreeMap<RecordId, Value> = transaction
                .columnar_lane(
                    context.namespace,
                    context.database,
                    id,
                    &key_rows_family,
                    lane,
                )
                .await?
                .into_iter()
                .collect();
            let mut over_rows = Vec::with_capacity(overs.len());
            for (name, family) in &overs {
                let rows: std::collections::BTreeMap<RecordId, Value> = transaction
                    .columnar_lane(context.namespace, context.database, id, family, lane)
                    .await?
                    .into_iter()
                    .collect();
                over_rows.push((name.as_str(), rows));
            }
            // The lane's records are the union of the families' rows: a
            // record whose key is null routed here but holds no key-family
            // row, and it still groups.
            let mut ids: std::collections::BTreeSet<RecordId> = key_rows.keys().cloned().collect();
            for (_, rows) in &over_rows {
                ids.extend(rows.keys().cloned());
            }
            let mut records = Vec::with_capacity(ids.len());
            for record_id in ids {
                let mut object = std::collections::BTreeMap::new();
                // The row authority decides whenever the projection is
                // silent or ambiguous about the key: no row covers a null
                // OR absent key (the design record — two different facts, grouped
                // differently), and a null key row is how a delete or an
                // update-to-null records the value's departure  —
                // which may be a tombstone the lane must not count at all.
                let needs_authority = key_rows
                    .get(&record_id)
                    .is_none_or(|value| matches!(value, Value::Null | Value::None));
                if needs_authority {
                    let address = tessari_storage::RecordAddress::new(
                        context.namespace,
                        context.database,
                        id,
                        record_id.clone(),
                    );
                    let held = match transaction.get(&address).await? {
                        Some(payload) => match tessari_encoding::decode_payload(&payload) {
                            Ok(Value::Object(held)) => Some(held),
                            _ => None,
                        },
                        // Tombstoned at this snapshot: the lane rows its
                        // delete left are not a record, and the group-by
                        // must not count it.
                        None => continue,
                    };
                    if let Some(held) = held
                        && let Some(value) = held.get(&key_name)
                    {
                        object.insert(key_name.clone(), value.clone());
                    }
                } else if let Some(value) = key_rows.get(&record_id) {
                    object.insert(key_name.clone(), value.clone());
                }
                for (name, rows) in &over_rows {
                    if let Some(value) = rows.get(&record_id) {
                        object.insert((*name).to_owned(), value.clone());
                    }
                }
                records.push((record_id, Value::Object(object)));
            }
            let mut lane_groups = crate::aggregate::Groups::new();
            self.fold_into(
                transaction,
                &mut lane_groups,
                records,
                &occurrences,
                (&select.group, None),
            )
            .await?;
            if count == 1 {
                groups = lane_groups;
                break;
            }
            // The lane's groups travel as partials; the merge is the final
            // merge. A fold that cannot travel means the lanes cannot merge
            // exactly — the records path answers.
            let mut partials = Vec::with_capacity(lane_groups.len());
            let mut travelable = true;
            for (key, (first, holding)) in lane_groups {
                let mut states = Vec::new();
                for accumulator in holding.iter().flatten() {
                    let Some(state) = accumulator.state() else {
                        travelable = false;
                        break;
                    };
                    states.push(state);
                }
                if !travelable {
                    break;
                }
                partials.push(crate::reduce::Partial { key, first, states });
            }
            if !travelable
                || !crate::aggregate::merge_partials(&mut groups, &occurrences, partials)?
            {
                return Ok(None);
            }
        }
        let plan = crate::plan::Plan::new(crate::outcome::AccessPath::Columnar)
            .over_lanes(count)
            .on(table.name.text.as_str());
        Ok(Some((groups, plan)))
    }

    /// The pure shape of a partition-local group-by, without the snapshot
    /// gates — what `EXPLAIN` asks, so the plan it reports is the plan the
    /// read takes. `None` declines, always safely.
    pub(crate) async fn columnar_grouped_shape(
        &self,
        transaction: &mut Transaction<'_>,
        context: Context,
        id: TableId,
        select: &Select,
    ) -> Result<Option<ColumnarGroupedShape>> {
        if crate::reduce::reduce_of(select).is_none() {
            return Ok(None);
        }
        let Some(definition) = Catalog::new(transaction).table(id).await?.filter(|found| {
            found.namespace == context.namespace && found.database == context.database
        }) else {
            return Ok(None);
        };
        if !matches!(
            definition.storage_kind,
            Some(tessari_storage::StorageKind::Column)
        ) || definition.expire.is_some()
        {
            return Ok(None);
        }
        let Some(partitioning) = &definition.partitions else {
            return Ok(None);
        };
        // One grouping key, and it the partition field.
        let [key] = select.group.as_slice() else {
            return Ok(None);
        };
        let Some(key_name) = root_field(key) else {
            return Ok(None);
        };
        if partitioning.by != key_name {
            return Ok(None);
        }
        let fields = Catalog::new(transaction)
            .fields_on(context.namespace, context.database, id)
            .await?;
        if !fields.iter().any(|field| field.name == key_name) {
            return Ok(None);
        }
        let visible = self.visible_in(transaction, id).await?;
        if !field_visible(&visible, &key_name) {
            return Ok(None);
        }
        // Every fold occurrence must merge exactly over a root field this
        // session may read; `at` (counter folds) is the one shape v1 leaves
        // to the records path.
        let mut overs: Vec<(String, tessari_types::FieldId)> = Vec::new();
        for held in crate::aggregate::occurrences(select.projection.written())
            .iter()
            .flatten()
        {
            let ExprKind::Fold { over, at, .. } = &held.kind else {
                continue;
            };
            if at.is_some() {
                return Ok(None);
            }
            if let Some(over) = over
                && let Some(name) = root_field(over)
            {
                let Some(field) = fields.iter().find(|field| field.name == name) else {
                    return Ok(None);
                };
                if !field_visible(&visible, &name) {
                    return Ok(None);
                }
                overs.push((name, field.id));
            }
        }
        Ok(Some(ColumnarGroupedShape {
            lanes: u64::from(partitioning.count),
            key: key_name,
            overs,
        }))
    }
}

/// What [`Session::columnar_grouped_shape`] resolved: the lane count, the
/// grouping field, and the fields the folds read.
pub(crate) struct ColumnarGroupedShape {
    lanes: u64,
    key: String,
    overs: Vec<(String, tessari_types::FieldId)>,
}

/// Whether every field this session may not read excludes `name` — a
/// grant-hidden field is never pruned on.
fn field_visible(visible: &Visible, name: &str) -> bool {
    visible.as_ref().is_none_or(|fields| fields.contains(name))
}

/// Collect the condition's top-level `AND` conjuncts when every one is a
/// plain comparison between one root field and one literal, or one root
/// field and a literal array (`field IN […]`). `false` means the condition
/// is richer than the scan's grammar — the caller routes to the row walk.
fn gather_conjuncts(condition: &Expr, into: &mut Vec<(String, BinaryOp, Value)>) -> bool {
    match &condition.kind {
        ExprKind::And(left, right) => gather_conjuncts(left, into) && gather_conjuncts(right, into),
        ExprKind::Binary { left, right, op } => {
            if *op == BinaryOp::In {
                // Only `field IN <array literal>` qualifies: the reversed
                // `array IN field` asks whether the array is an element of
                // the field's value — a different question the scan cannot
                // narrow, and the row engine answers it.
                if let (Some(name), Some(values)) = (root_field(left), array_literals(right)) {
                    into.push((name, BinaryOp::In, Value::Array(values)));
                    return true;
                }
                return false;
            }
            let Some(compare) = plain_comparison(*op) else {
                return false;
            };
            if let (Some(name), Some(value)) = (root_field(left), literal(right)) {
                into.push((name, compare, value));
                true
            } else if let (Some(value), Some(name)) = (literal(left), root_field(right)) {
                into.push((name, mirror(compare), value));
                true
            } else {
                false
            }
        }
        // A single-clause condition is its own AND chain of one.
        _ => false,
    }
}

/// The comparisons the scan can prune on; everything else declines.
const fn plain_comparison(op: BinaryOp) -> Option<BinaryOp> {
    match op {
        BinaryOp::Equal
        | BinaryOp::NotEqual
        | BinaryOp::Less
        | BinaryOp::LessOrEqual
        | BinaryOp::Greater
        | BinaryOp::GreaterOrEqual => Some(op),
        _ => None,
    }
}

/// The same comparison from the other side of the operator.
const fn mirror(op: BinaryOp) -> BinaryOp {
    match op {
        BinaryOp::Less => BinaryOp::Greater,
        BinaryOp::LessOrEqual => BinaryOp::GreaterOrEqual,
        BinaryOp::Greater => BinaryOp::Less,
        BinaryOp::GreaterOrEqual => BinaryOp::LessOrEqual,
        _ => op,
    }
}

/// The field name a root-only path reads, nothing else.
fn root_field(expr: &Expr) -> Option<String> {
    match &expr.kind {
        ExprKind::Path(field) if field.path.steps().is_empty() => {
            Some(field.path.root().to_owned())
        }
        _ => None,
    }
}

/// A literal constant the predicate binds.
fn literal(expr: &Expr) -> Option<Value> {
    match &expr.kind {
        ExprKind::Literal(value) => Some(value.clone()),
        _ => None,
    }
}

/// An array literal of literal elements, as one bound value. A non-literal
/// element (anything the bind stage did not collapse to a constant)
/// disqualifies the whole list.
fn array_literals(expr: &Expr) -> Option<Vec<Value>> {
    match &expr.kind {
        ExprKind::Array(elements) => elements.iter().map(literal).collect(),
        _ => None,
    }
}
