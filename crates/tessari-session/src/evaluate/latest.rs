//! `LATEST BY <field>`: the newest record per value of a field, on a series
//! (ADR-0088 §3).
//!
//! # Two paths, one answer
//!
//! Over a whole table with an index leading on the field, the index walk takes
//! one seek per value ([`Transaction::newest_per_value`]). Everywhere else — a
//! condition, no index, a transaction that has written the table — the source
//! reads what it reads and [`Session::newest_per_key`] keeps the newest per key.
//! The reduction runs on both paths, so the walk can only make the answer
//! cheaper, never different: over records that are already one per key it
//! changes nothing.
//!
//! # Why only on a series
//!
//! "Newest" is a fact the key has to carry. A series' identity is its time, so
//! the greatest identity per key is the newest record; any other table's
//! identity is a counter or a name, and the same rule would answer the record
//! with the largest id and call it the latest.

use std::collections::BTreeMap;

use tessari_ql::{FieldPath, Select, Source};
use tessari_storage::{Catalog, TableKind, Transaction};
use tessari_types::{RecordId, TableId, Value};

use super::Walked;
use crate::context::Context;
use crate::error::{Error, Result};
use crate::session::Session;

impl Session<'_> {
    /// Refuse `LATEST BY` over anything but a series, and beside a grouping.
    ///
    /// # Errors
    ///
    /// [`Error::LatestNeedsSeries`] and [`Error::LatestBesideGroup`].
    pub(super) fn check_latest(
        &self,
        transaction: &mut Transaction<'_>,
        select: &Select,
        latest: &FieldPath,
    ) -> Result<()> {
        if !select.group.is_empty() {
            return Err(Error::LatestBesideGroup { span: latest.span });
        }
        let table = match &select.from {
            Source::Table(table) | Source::Where { table, .. } | Source::Range { table, .. } => {
                table
            }
            _ => return Err(Error::LatestNeedsSeries { span: latest.span }),
        };
        let (_, id) = self.resolve_table(transaction, table)?;
        let series = Catalog::new(transaction)
            .table(id)?
            .is_some_and(|found| matches!(found.kind, TableKind::Series(_)));
        if series {
            Ok(())
        } else {
            Err(Error::LatestNeedsSeries { span: latest.span })
        }
    }

    /// The newest record per value of `latest`, read from an index leading on
    /// it — or [`Walked::NotServed`] where no such index may answer.
    pub(super) fn walk_latest(
        &self,
        transaction: &mut Transaction<'_>,
        context: Context,
        table: TableId,
        latest: &FieldPath,
    ) -> Result<Walked> {
        // The same admission an ordered walk has, for the same reason: the
        // entry's position is the answer, so the entries have to be current and
        // the field visible to this caller.
        let Some((index, visible)) =
            self.index_serving_order(transaction, context, table, &latest.path, true)?
        else {
            return Ok(Walked::NotServed);
        };
        let rows = transaction.newest_per_value(&index)?;
        Ok(Walked::Served {
            found: self.records_of(rows, &visible)?,
            index: index.name,
        })
    }

    /// Keep, per value of `latest`, the record with the greatest identity —
    /// the newest on a series — least value first. A record without the field
    /// has no key and is not answered.
    ///
    pub(super) fn newest_per_key(
        records: Vec<(RecordId, Value)>,
        latest: &FieldPath,
    ) -> Vec<(RecordId, Value)> {
        let mut newest: BTreeMap<Value, (RecordId, Value)> = BTreeMap::new();
        for (id, record) in records {
            let Some(value) = latest.path.resolve(&record).cloned() else {
                continue;
            };
            match newest.get(&value) {
                Some((held, _)) if *held >= id => {}
                _ => {
                    newest.insert(value, (id, record));
                }
            }
        }
        newest.into_values().collect()
    }
}
