//! Following a record reference — reading the record a field names.
//!
//! # Why this is the join this data model has
//!
//! A relational join exists because a foreign key is a *value* that has to be
//! matched against another table's key. Here a reference is not a value that
//! resembles an address — it **is** one: a `RecordRef` carries a table and an
//! id, and it is the same thing an edge is built from. There is nothing to
//! match, so following one is a point read rather than a search.
//!
//! That is why `FETCH` exists and a general join does not yet: this is the shape
//! a join takes when the relationship is *stored* rather than recomputed, and it
//! is the common case. `docs/tessariql.md` §8 names the four decisions a predicate
//! join would still need.
//!
//! # Four rules, each of which is a decision
//!
//! **A reference to a record that is gone stays a reference.** The record may
//! have been deleted and the field still holds its name — the one piece of
//! information the caller has. `NONE` would throw that away and make "deleted"
//! indistinguishable from "empty". It also gives the answer an unambiguous
//! reading: an object means the record was there, a reference means it was not.
//! A traversal answers a dangling endpoint the same way, because that is a state
//! of the data and not a failure of the query.
//!
//! **One level.** The fetched record's own references stay references. That
//! bounds the work at the references the answer already holds — one ask,
//! whatever their number — and makes a cycle impossible rather than handled.
//!
//! **An array of references is followed element by element**, because a list of
//! references is how a to-many relation is stored here; an element that is not a
//! reference is left alone, the same way a field that is not one is.
//!
//! **A route reaching nothing is not an error** — the missing-field rule, one
//! level down, as everywhere else in this language.
//!
//! # What it costs, and the one saving that is free
//!
//! One point read per *distinct* reference across the whole read. Distinct,
//! because a read resolves at one snapshot, and two reads of one address at one
//! snapshot must answer the same thing — so remembering the first is not a cache
//! that can go stale, it is the same read written once. That matters for the
//! shape this feature is for: a hundred posts by three authors is three reads
//! rather than a hundred.
//!
//! It **is** one request. The distinct set is knowable from the records already
//! in hand, so it is gathered before anything is read and asked for in a single
//! `get_each` — one ask for a fetch reaching any number of tables, because each
//! address is asked for as its own bounded range. That is a change to *when* the
//! records are asked for and not to *which*: the set is the same set the memo
//! accumulated one reference at a time, so the answer is unchanged by
//! construction rather than by assertion.

use std::collections::{BTreeMap, BTreeSet};

use tessari_ql::FieldPath;
use tessari_storage::{RecordAddress, Transaction};
use tessari_types::{RecordId, RecordRef, TableId, Value};

use crate::context::Context;
use crate::error::Result;
use crate::session::Session;

impl Session<'_> {
    /// Replace each named reference in each record with the record it names.
    ///
    /// Three passes, and only the middle one touches the store. Gathering first
    /// is what turns "one point read per distinct reference" into one ask: the
    /// set to fetch is knowable from the records already in hand, so nothing is
    /// read until all of it is known.
    ///
    /// The set is the same set the memo used to accumulate one reference at a
    /// time, which is why the answer cannot change. Distinctness is not a saving
    /// here, it is a correctness property already relied on — two reads of one
    /// address at one snapshot must answer the same thing — so batching the
    /// distinct set asks for exactly what was going to be asked for anyway,
    /// rather than for a batch's worth of whatever happens to be nearby.
    pub(crate) fn follow(
        &self,
        transaction: &mut Transaction<'_>,
        records: &mut [(RecordId, Value)],
        routes: &[FieldPath],
        context: Context,
    ) -> Result<()> {
        let wanted = referenced_in(records, routes);
        if wanted.is_empty() {
            return Ok(());
        }
        let seen = self.resolve_each(transaction, context, &wanted)?;
        for (_, record) in records {
            for route in routes {
                let Some(held) = route.path.resolve_mut(record) else {
                    continue;
                };
                match held {
                    Value::Record(reference) => {
                        if let Some(found) = resolved(&seen, reference) {
                            *held = found;
                        }
                    }
                    Value::Array(items) => {
                        for item in items {
                            let Value::Record(reference) = item else {
                                continue;
                            };
                            if let Some(found) = resolved(&seen, reference) {
                                *item = found;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    /// Read every referenced record, in one ask, redacted by its own table.
    ///
    /// **The record a reference lands on is redacted by that table's grant.** A
    /// reference is an address into a *different* table, and following one is not
    /// a way to read what that table's grant refuses. Visibility is resolved once
    /// per table rather than once per reference, which it already was.
    ///
    /// The addresses travel together whatever tables they name: each is asked for
    /// as its own bounded range, so a fetch reaching three tables is still one
    /// ask. A reference carries a table and an id and no tenancy, so every one of
    /// them resolves in the read's own database — which is also why a fetch
    /// cannot reach across one (ADR-0008).
    fn resolve_each(
        &self,
        transaction: &mut Transaction<'_>,
        context: Context,
        wanted: &[(TableId, RecordId)],
    ) -> Result<BTreeMap<(TableId, RecordId), Value>> {
        let mut visible: BTreeMap<TableId, crate::redact::Visible> = BTreeMap::new();
        // A reference into a table this node holds only part of, landing in a
        // part it lacks, would resolve to nothing and read as a record that is
        // not there (G031 S3.3, Q-775).
        for (table, id) in wanted {
            self.refuse_reading_a_part(transaction, *table, crate::evaluate::Part::Record(id))?;
        }
        for (table, _) in wanted {
            if !visible.contains_key(table) {
                let held = self.visible_in(transaction, *table)?;
                visible.insert(*table, held);
            }
        }
        let addresses: Vec<RecordAddress> = wanted
            .iter()
            .map(|(table, id)| {
                RecordAddress::new(context.namespace, context.database, *table, id.clone())
            })
            .collect();
        let payloads = transaction.get_each(&addresses)?;

        let mut found = BTreeMap::new();
        for ((table, id), payload) in wanted.iter().zip(payloads) {
            let Some(payload) = payload else { continue };
            let Some(allowed) = visible.get(table) else {
                continue;
            };
            found.insert((*table, id.clone()), self.record_of(&payload, allowed)?);
        }
        Ok(found)
    }
}

/// Every distinct record a fetch would follow, in a stable order.
///
/// A pure walk over what is already in hand: it reads no store, which is the
/// whole point of doing it before anything is read. **Distinct** because a read
/// resolves at one snapshot and two reads of one address there must answer the
/// same thing — so asking once is not a cache that can go stale, it is the same
/// read written once. A hundred posts by three authors is three.
fn referenced_in(records: &[(RecordId, Value)], routes: &[FieldPath]) -> Vec<(TableId, RecordId)> {
    let mut wanted: BTreeSet<(TableId, RecordId)> = BTreeSet::new();
    for (_, record) in records {
        for route in routes {
            let Some(held) = route.path.resolve(record) else {
                continue;
            };
            match held {
                Value::Record(reference) => {
                    wanted.insert((reference.table, reference.id.clone()));
                }
                Value::Array(items) => {
                    for item in items {
                        if let Value::Record(reference) = item {
                            wanted.insert((reference.table, reference.id.clone()));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    wanted.into_iter().collect()
}

/// The record a reference names, if the resolution found one.
///
/// **A reference to a record that is gone stays a reference.** The record may
/// have been deleted and the field still holds its name — the one piece of
/// information the caller has. `NONE` would throw that away and make "deleted"
/// indistinguishable from "empty".
fn resolved(seen: &BTreeMap<(TableId, RecordId), Value>, reference: &RecordRef) -> Option<Value> {
    seen.get(&(reference.table, reference.id.clone())).cloned()
}
