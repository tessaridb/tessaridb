//! The columnar seam (G082 SG3): when a scan fallback qualifies, the read
//! runs through the projection instead of the row walk.
//!
//! # The gate is the whole safety story (worksheet §6.1)
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
use tessari_ql::{BinaryOp, Expr, ExprKind};
use tessari_storage::{Catalog, ColumnarCompare, ColumnarPredicate, Transaction};
use tessari_types::{TableId, Value};

use crate::condition::boolean;
use crate::consume::Consumer;
use crate::context::Context;
use crate::error::Result;
use crate::redact::Visible;
use crate::session::Session;

use super::{Scope, Testing};

/// A read the columnar gate qualified: the predicates it runs with and the
/// partition lane it pins to, when the condition fixes the partition key to
/// one constant (worksheet §6.3 — a read naming one partition reads one
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
            if matches!(value, Value::Null | Value::None) {
                // Null semantics are the row engine's third-valued logic;
                // the scan only narrows on non-null constants.
                return Ok(None);
            }
            let Some(field) = fields.iter().find(|field| field.name == name) else {
                return Ok(None);
            };
            if !field_visible(&visible, &name) {
                return Ok(None);
            }
            predicates.push(ColumnarPredicate {
                family: ColumnFamily::Id(field.id),
                compare: match op {
                    BinaryOp::Equal => ColumnarCompare::Equal,
                    BinaryOp::NotEqual => ColumnarCompare::NotEqual,
                    BinaryOp::Less => ColumnarCompare::Less,
                    BinaryOp::LessOrEqual => ColumnarCompare::LessOrEqual,
                    BinaryOp::Greater => ColumnarCompare::Greater,
                    BinaryOp::GreaterOrEqual => ColumnarCompare::GreaterOrEqual,
                    // gather_conjuncts only yields the six comparisons.
                    _ => return Ok(None),
                },
                value: value.clone(),
            });
            // An equality on the partition key names one lane: the scan
            // reads that lane alone (worksheet §6.3). The constant rides
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

/// Whether every field this session may not read excludes `name` — a
/// grant-hidden field is never pruned on.
fn field_visible(visible: &Visible, name: &str) -> bool {
    visible.as_ref().is_none_or(|fields| fields.contains(name))
}

/// Collect the condition's top-level `AND` conjuncts when every one is a
/// plain comparison between one root field and one literal. `false` means
/// the condition is richer than the scan's grammar — the caller routes to
/// the row walk.
fn gather_conjuncts(condition: &Expr, into: &mut Vec<(String, BinaryOp, Value)>) -> bool {
    match &condition.kind {
        ExprKind::And(left, right) => gather_conjuncts(left, into) && gather_conjuncts(right, into),
        ExprKind::Binary { left, right, op } => {
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
