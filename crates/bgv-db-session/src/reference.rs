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
//! is the common case. `docs/bgvql.md` §8 names the four decisions a predicate
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
//! bounds the work at one point read per reference and makes a cycle impossible
//! rather than handled.
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
//! What it is *not* is a batched multi-get. Turning many point reads into one
//! request is a planner decision about how to execute, and is not made here.

use std::collections::BTreeMap;

use bgv_db_ql::FieldPath;
use bgv_db_storage::{RecordAddress, Transaction};
use bgv_db_types::{RecordId, RecordRef, TableId, Value};

use bgv_db_encoding::decode_payload;

use crate::context::Context;
use crate::error::Result;
use crate::session::Session;

impl Session<'_> {
    /// Replace each named reference in each record with the record it names.
    pub(crate) fn follow(
        &self,
        transaction: &mut Transaction<'_>,
        records: &mut [(RecordId, Value)],
        routes: &[FieldPath],
        context: Context,
    ) -> Result<()> {
        let mut seen: BTreeMap<(TableId, RecordId), Option<Value>> = BTreeMap::new();
        for (_, record) in records {
            for route in routes {
                let Some(held) = route.path.resolve_mut(record) else {
                    continue;
                };
                match held {
                    Value::Record(reference) => {
                        if let Some(found) = read(transaction, context, reference, &mut seen)? {
                            *held = found;
                        }
                    }
                    Value::Array(items) => {
                        for item in items {
                            let Value::Record(reference) = item else {
                                continue;
                            };
                            if let Some(found) = read(transaction, context, reference, &mut seen)? {
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
}

/// The record a reference names, or `None` when there is not one.
///
/// A reference carries a table and an id and no tenancy, so it resolves in the
/// read's own database — which is also why a fetch cannot reach across one
/// (ADR-0008).
fn read(
    transaction: &mut Transaction<'_>,
    context: Context,
    reference: &RecordRef,
    seen: &mut BTreeMap<(TableId, RecordId), Option<Value>>,
) -> Result<Option<Value>> {
    let at = (reference.table, reference.id.clone());
    if let Some(held) = seen.get(&at) {
        return Ok(held.clone());
    }
    let address = RecordAddress::new(
        context.namespace,
        context.database,
        reference.table,
        reference.id.clone(),
    );
    let found = match transaction.get(&address)? {
        Some(payload) => Some(decode_payload(&payload)?),
        None => None,
    };
    seen.insert(at, found.clone());
    Ok(found)
}
