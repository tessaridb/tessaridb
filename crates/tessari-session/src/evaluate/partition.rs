//! A read that names one partition, confined to its identities (ADR-0096 D3).
//!
//! A partitioned record's identity begins with its field's value and a `:`, and
//! the value holds no `:`, so every record of one partition sorts between
//! `'<value>:'` and `'<value>;'` — the character after `:` — and nothing else
//! does. A condition fixing the field therefore reads that span and no other;
//! the condition is still tested on every record the span holds.
//!
//! Asked by the read and by `EXPLAIN` from the same functions, so the plan a
//! read reports and the one `EXPLAIN` predicts cannot disagree.

use tessari_ql::{Expr, ExprKind};
use tessari_storage::{Catalog, Transaction};
use tessari_types::{BinaryOp, Path, RecordId, TableId, Value};

use crate::error::Result;
use crate::evaluate::Part;
use crate::session::Session;

impl Session<'_> {
    /// The span of identities `condition` confines a read of table `id` to,
    /// when the table is partitioned and the condition fixes the partition.
    pub(crate) fn partition_span(
        &self,
        transaction: &mut Transaction<'_>,
        id: TableId,
        condition: &Expr,
    ) -> Result<Option<(RecordId, RecordId)>> {
        let Some(definition) = Catalog::new(transaction).table(id)? else {
            return Ok(None);
        };
        Ok(definition
            .partition
            .as_deref()
            .and_then(|field| fixed(condition, &Path::field(field)))
            .map(|value| {
                (
                    RecordId::Text(format!("{value}:")),
                    RecordId::Text(format!("{value};")),
                )
            }))
    }

    /// The shards of table `id` that `part` touches, in key order, or `None`
    /// for a table that is not split.
    pub(crate) fn shards_touched(
        &self,
        transaction: &mut Transaction<'_>,
        id: TableId,
        part: Part<'_>,
    ) -> Result<Option<Vec<u32>>> {
        let Some(definition) = Catalog::new(transaction).table(id)? else {
            return Ok(None);
        };
        Ok(definition.shards.map(|map| {
            map.spans()
                .filter(|span| crate::gather::window_of(span, part).is_some())
                .map(|span| span.id.get())
                .collect()
        }))
    }
}

/// The text a conjunct of `condition` fixes `field` to, when one does — and
/// one that could be a partition: a value holding a `:` names none.
fn fixed<'a>(condition: &'a Expr, field: &Path) -> Option<&'a str> {
    match &condition.kind {
        ExprKind::And(left, right) => fixed(left, field).or_else(|| fixed(right, field)),
        ExprKind::Binary {
            op: BinaryOp::Equal,
            left,
            right,
        } => match (&left.kind, &right.kind) {
            (ExprKind::Path(path), ExprKind::Literal(Value::String(value)))
            | (ExprKind::Literal(Value::String(value)), ExprKind::Path(path))
                if path.path == *field && !value.contains(':') =>
            {
                Some(value.as_str())
            }
            _ => None,
        },
        _ => None,
    }
}
