//! The other sources that read a split table whole — a join's sides and a
//! `FETCH` — gathered rather than refused (G057 C2, ADR-0083).
//!
//! What travels is still stored records, redacted here by the caller's own
//! visibility, so a join or a fetch on a partial holder answers what the whole
//! node answers and hides what it hides. A join's far side is narrowed on the
//! leaders to the keys the near side holds — a runtime filter, sent as a
//! `CONTAINS`-style membership the leader tests after taking away the fields
//! this session may not read — so a hidden key field matches nothing there
//! exactly as it matches nothing here.

use tessari_ql::{BinaryOp, Expr, ExprKind, FieldPath};
use tessari_storage::Transaction;
use tessari_types::{DatabaseId, NamespaceId, RecordId, TableId, Value};

use super::Stored;
use crate::error::Result;
use crate::evaluate::Part;
use crate::outcome::Note;
use crate::redact::Visible;
use crate::session::Session;

impl Session<'_> {
    /// Every stored record of table `id` — gathered from the leaders of the
    /// shards this node lacks, else scanned here — in identity order.
    ///
    /// Refuses `NotHeldHere` where gathering is withheld (a transaction,
    /// `VERSION`, a node told of no gatherer), as every gathered read does.
    pub(crate) fn whole_table(
        &self,
        transaction: &mut Transaction<'_>,
        (namespace, database): (NamespaceId, DatabaseId),
        id: TableId,
        notes: &mut Vec<Note>,
    ) -> Result<Stored> {
        match self.gather_a_part(transaction, id, Part::Whole, None, None, None)? {
            Some((found, note)) => {
                noted(notes, note);
                Ok(found)
            }
            None => Ok(transaction.scan_table(namespace, database, id)?),
        }
    }

    /// The stored records of table `id` whose `key` is one of `keys`, as far
    /// as this node can tell under `visible` — gathered with the membership
    /// pushed to the leaders, so only the records a join can match travel.
    ///
    /// Asked only of a table this node lacks part of; a record the leader keeps
    /// is matched again by the join here, so leniency there costs only bytes.
    pub(crate) fn gathered_by_keys(
        &self,
        transaction: &mut Transaction<'_>,
        place: (NamespaceId, DatabaseId),
        id: TableId,
        (key, keys): (&FieldPath, Vec<Value>),
        visible: &Visible,
        notes: &mut Vec<Note>,
    ) -> Result<Stored> {
        let span = key.span;
        let membership = Expr {
            kind: ExprKind::Binary {
                op: BinaryOp::In,
                left: Box::new(Expr {
                    kind: ExprKind::Path(key.clone()),
                    span,
                }),
                right: Box::new(Expr {
                    kind: ExprKind::Literal(Value::Array(keys)),
                    span,
                }),
            },
            span,
        };
        let Some((condition, parameters)) = tessari_ql::portable(&membership) else {
            return self.whole_table(transaction, place, id, notes);
        };
        let pushed = crate::Pushed {
            visible: visible.clone(),
            condition,
            parameters,
        };
        match self.gather_a_part(transaction, id, Part::Whole, Some(&pushed), None, None)? {
            Some((found, note)) => {
                noted(notes, note);
                Ok(found)
            }
            None => Ok(transaction.scan_table(place.0, place.1, id)?),
        }
    }

    /// The stored record `record` of table `id` gathered from its shard's
    /// leader, or `None` when this node holds that record's shard.
    pub(crate) fn gathered_record(
        &self,
        transaction: &mut Transaction<'_>,
        id: TableId,
        record: &RecordId,
        notes: &mut Vec<Note>,
    ) -> Result<Option<Stored>> {
        Ok(
            match self.gather_a_part(transaction, id, Part::Record(record), None, None, None)? {
                Some((found, note)) => {
                    noted(notes, note);
                    Some(found)
                }
                None => None,
            },
        )
    }
}

/// Add `note` unless the answer already carries it — a read and its fetch can
/// gather the same table.
fn noted(notes: &mut Vec<Note>, note: Note) {
    if !notes.contains(&note) {
        notes.push(note);
    }
}
