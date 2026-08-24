//! Turning stored bytes into a record this session is allowed to see.
//!
//! # Why this is a method and not a free function
//!
//! Thirteen places used to decode a record. A redaction *remembered* at thirteen
//! call sites is a redaction *forgotten* at one of them, and the one it is
//! forgotten at is the one that matters. So the free function is gone and every
//! site names the table its record came from — a caller cannot obtain a record
//! value without this having had its chance, and the compiler is what says so.
//!
//! # Unreadable to the evaluator, not to the printer
//!
//! The field is removed **before** anything looks at the record, which is the
//! whole difference between a permission and a redaction:
//!
//! ```text
//! SELECT count(*) FROM staff WHERE salary > 100000;
//! ```
//!
//! That statement never shows `salary` and asks about it precisely. Editing the
//! *answer* leaves the count intact, so the field would be readable one bit at a
//! time by anybody willing to bisect. Removing it from the record instead makes
//! the path resolve to `NONE`, the comparison false by the missing-field rule the
//! language already has, and the count zero — and the projection then omits it
//! for the same reason rather than for a second one.
//!
//! An index cannot get around it either, and needs no rule of its own: candidates
//! an index offers are re-tested against the whole condition, and the record they
//! are re-tested against is this one.

use std::collections::BTreeSet;

use tessari_encoding::decode_payload;
use tessari_storage::{Transaction, Verb};
use tessari_types::{RecordId, TableId, Value};

use crate::error::Result;
use crate::session::Session;

/// The fields of one table this session may read.
///
/// `None` means all of them — no grants at all, or a grant that named none,
/// which is the same rule the table grant has one level up: what is named is the
/// whole story, and naming nothing is no restriction.
pub type Visible = Option<BTreeSet<String>>;

impl Session<'_> {
    /// What this session may read of a table's records.
    ///
    /// Resolved once per read rather than once per record: which fields a grant
    /// names is a property of the catalog, and the catalog does not change under
    /// a read.
    pub(crate) fn visible_in(
        &self,
        transaction: &mut Transaction<'_>,
        table: TableId,
    ) -> Result<Visible> {
        let Some(user) = self.identity.user() else {
            return Ok(None);
        };
        let grants = tessari_storage::Catalog::new(transaction).grants_for(user.id)?;
        let Some(grant) = grants
            .iter()
            .find(|grant| grant.table == table && grant.verbs.contains(&Verb::Read))
        else {
            // Either the user is not grant-governed, in which case their role
            // decides and nothing is hidden, or they are and this table was
            // refused outright by `within_grants` before a record was reached.
            return Ok(None);
        };
        if grant.fields.is_empty() {
            return Ok(None);
        }
        Ok(Some(grant.fields.iter().cloned().collect()))
    }

    /// What this session may read of a table's records, over its own store.
    ///
    /// The same question [`Session::visible_in`] answers, for a caller that has
    /// no transaction because it is not running a statement — the change feed
    /// being the one, and the surface where forgetting this means pushing a
    /// field nobody granted.
    ///
    /// # Errors
    ///
    /// Returns an error when the catalog cannot be read.
    pub fn visible(&self, store: &tessari_storage::Store, table: TableId) -> Result<Visible> {
        let mut transaction = store.begin()?;
        let held = self.visible_in(&mut transaction, table)?;
        transaction.rollback();
        Ok(held)
    }

    /// One record, as this session may see it.
    ///
    /// # Errors
    ///
    /// Returns a decoding failure when the payload cannot be read.
    pub(crate) fn record_of(&self, payload: &[u8], visible: &Visible) -> Result<Value> {
        Ok(seen(decode_payload(payload)?, visible))
    }

    /// Every record, as this session may see them.
    ///
    /// # Errors
    ///
    /// Returns a decoding failure when a payload cannot be read.
    pub(crate) fn records_of(
        &self,
        found: Vec<(RecordId, Vec<u8>)>,
        visible: &Visible,
    ) -> Result<Vec<(RecordId, Value)>> {
        let mut records = Vec::with_capacity(found.len());
        for (id, payload) in found {
            records.push((id, self.record_of(&payload, visible)?));
        }
        Ok(records)
    }
}

/// A record with everything this session may not read taken out.
///
/// Top-level fields only. A grant names a field of a table, and a route into a
/// nested object is a different thing to name — one this milestone does not take,
/// so that a half-answer does not look like a whole one.
///
/// Public because the change feed pushes records that never passed through a
/// session's read path, and a second implementation of "what does this user see"
/// is a second answer waiting to disagree with this one.
#[must_use]
pub fn seen(value: Value, visible: &Visible) -> Value {
    let Some(allowed) = visible else {
        return value;
    };
    let Value::Object(fields) = value else {
        // A key-value record holds a bare value with no field to name, so a
        // field grant has nothing to say about it and the table grant already
        // decided whether it may be read at all.
        return value;
    };
    Value::Object(
        fields
            .into_iter()
            .filter(|(name, _)| allowed.contains(name))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use std::collections::{BTreeMap, BTreeSet};

    use tessari_types::Value;

    use super::seen;

    fn record() -> Value {
        Value::Object(BTreeMap::from([
            ("name".to_owned(), Value::from("ada")),
            ("salary".to_owned(), Value::from(120_000_i64)),
        ]))
    }

    #[test]
    fn naming_nothing_is_no_restriction() {
        assert_eq!(seen(record(), &None), record());
    }

    #[test]
    fn what_is_not_named_is_not_there_at_all() {
        // Not emptied, not nulled — absent, so a path reaching it evaluates to
        // `NONE` and every comparison the language has is false against it.
        let allowed = Some(BTreeSet::from(["name".to_owned()]));
        let Value::Object(held) = seen(record(), &allowed) else {
            panic!("not an object");
        };
        assert_eq!(held.len(), 1);
        assert!(!held.contains_key("salary"));
    }

    #[test]
    fn a_bare_value_has_no_field_to_name() {
        let allowed = Some(BTreeSet::from(["name".to_owned()]));
        assert_eq!(seen(Value::from(7_i64), &allowed), Value::from(7_i64));
    }
}
