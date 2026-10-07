//! A grouped view kept a group at a time (ADR-0109 D2 as amended, Q-908).
//!
//! A change can only move the groups its record **left** and **joined**, so a
//! batch recomputes those and no other. Which group a record left is not in the
//! change — the feed carries no old value — and is read from the view's
//! membership map, which every batch keeps in step with the rows it writes.
//!
//! A group is recomputed by the read's own fold over that group's members, the
//! same `grouped` the read answers with, so its row is what the read answers
//! for that group at the batch's snapshot. A group no record is left in loses
//! its row.
//!
//! # The row's identity is the group
//!
//! A row is stored under its group key's order-preserving encoding — the bytes
//! an index would hold for those values — so a view read answers its rows in
//! the read's own group order, and a row keeps its identity while groups come
//! and go around it.

use std::collections::{BTreeMap, BTreeSet};

use tessari_encoding::{IndexValues, encode_payload};
use tessari_storage::{RecordAddress, Transaction};
use tessari_types::{RecordId, Value};

use super::Kept;
use crate::condition::boolean;
use crate::error::{Error, Result};
use crate::evaluate::Scope;
use crate::search::Searched;
use crate::session::Session;

impl Session<'_> {
    /// Bring the groups the changed records left and joined current.
    pub(super) fn recompute_groups(
        &self,
        transaction: &mut Transaction<'_>,
        kept: &Kept,
        changed: &BTreeSet<RecordId>,
    ) -> Result<()> {
        let searched = self.searched_of(transaction, kept)?;
        let mut affected: BTreeSet<Vec<u8>> = BTreeSet::new();
        for id in changed {
            let left = transaction.view_group_of(kept.view, id)?;
            let joined = match transaction.get(&self.source_row(kept, id))? {
                Some(payload) => match self.passing(transaction, kept, id, &payload, &searched)? {
                    Some(record) => Some(self.group_key(transaction, kept, &record)?),
                    None => None,
                },
                None => None,
            };
            if left != joined {
                transaction.move_view_member(kept.view, id, left.as_deref(), joined.as_deref());
            }
            affected.extend(left);
            affected.extend(joined);
        }
        for group in affected {
            // Every member that changed was re-filed above, and a member that
            // did not change still holds what put it in this group.
            let mut members = Vec::new();
            for id in transaction.view_group_members(kept.view, &group)? {
                if let Some(payload) = transaction.get(&self.source_row(kept, &id))? {
                    members.push((id, self.record_of(&payload, &None)?));
                }
            }
            self.write_group(transaction, kept, group, members)?;
        }
        Ok(())
    }

    /// Replace every row and the whole membership map from the source.
    pub(super) fn recompute_groups_whole(
        &self,
        transaction: &mut Transaction<'_>,
        kept: &Kept,
    ) -> Result<()> {
        let (namespace, database) = (kept.context.namespace, kept.context.database);
        for (id, _) in transaction.scan_table(namespace, database, kept.view)? {
            transaction.delete(self.row(kept, id));
        }
        transaction.forget_view_members(kept.view)?;
        let searched = self.searched_of(transaction, kept)?;
        let mut groups: BTreeMap<Vec<u8>, Vec<(RecordId, Value)>> = BTreeMap::new();
        for (id, payload) in transaction.scan_table(namespace, database, kept.source)? {
            let Some(record) = self.passing(transaction, kept, &id, &payload, &searched)? else {
                continue;
            };
            let group = self.group_key(transaction, kept, &record)?;
            transaction.move_view_member(kept.view, &id, None, Some(&group));
            groups.entry(group).or_default().push((id, record));
        }
        for (group, members) in groups {
            self.write_group(transaction, kept, group, members)?;
        }
        Ok(())
    }

    /// Write one group's row from its members, or remove it when it has none.
    fn write_group(
        &self,
        transaction: &mut Transaction<'_>,
        kept: &Kept,
        group: Vec<u8>,
        members: Vec<(RecordId, Value)>,
    ) -> Result<()> {
        let select = &kept.understood.select;
        let (mut rows, ..) = self.grouped(
            transaction,
            members,
            select.projection.written(),
            &select.group,
            None,
        )?;
        let row = self.row(kept, RecordId::Bytes(group));
        match (rows.pop(), rows.is_empty()) {
            (None, _) => transaction.delete(row),
            (Some((_, value)), true) => transaction.put(row, encode_payload(&value).into_bytes()),
            // Two groups under one key: values the read tells apart that their
            // encoding does not. Refused rather than kept as one row.
            (Some(_), false) => {
                return Err(Error::MaterializedShape {
                    what: "group by values its stored key cannot tell apart",
                    span: kept.understood.source.span,
                });
            }
        }
        Ok(())
    }

    /// The group a record is in: its group key, evaluated as the read's fold
    /// evaluates it, in the order-preserving encoding.
    fn group_key(
        &self,
        transaction: &mut Transaction<'_>,
        kept: &Kept,
        record: &Value,
    ) -> Result<Vec<u8>> {
        let group = &kept.understood.select.group;
        let mut key = Vec::with_capacity(group.len());
        for held in group {
            key.push(self.evaluate_in(transaction, held, Scope::of(record))?);
        }
        Ok(IndexValues::of(&key).as_slice().to_vec())
    }

    /// The record, decoded, when it passes the view's condition.
    fn passing(
        &self,
        transaction: &mut Transaction<'_>,
        kept: &Kept,
        id: &RecordId,
        payload: &[u8],
        searched: &Searched,
    ) -> Result<Option<Value>> {
        let record = self.record_of(payload, &None)?;
        if let Some(condition) = &kept.understood.condition {
            let value = self.evaluate_in(
                transaction,
                condition,
                Scope::searching(&record, searched).identified(id),
            )?;
            if !boolean(&value, condition.span)? {
                return Ok(None);
            }
        }
        Ok(Some(record))
    }

    /// What the condition's searches resolve to, once per batch.
    fn searched_of(&self, transaction: &mut Transaction<'_>, kept: &Kept) -> Result<Searched> {
        match &kept.understood.condition {
            Some(condition) => self.searched_for(transaction, kept.source, &[condition]),
            None => Ok(Searched::default()),
        }
    }

    fn source_row(&self, kept: &Kept, id: &RecordId) -> RecordAddress {
        RecordAddress::new(
            kept.context.namespace,
            kept.context.database,
            kept.source,
            id.clone(),
        )
    }
}
