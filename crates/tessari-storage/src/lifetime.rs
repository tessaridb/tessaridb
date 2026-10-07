//! The instant a record of a table that declares expiry carries, settled at
//! commit (ADR-0122 A3).
//!
//! # Where, and why there
//!
//! Called once per commit attempt under the write gate, beside the topic's
//! admission, against the committed state that attempt builds on. That makes it
//! the one place every write path passes — a statement, `/kv`, an event, a
//! consumer, a restore — so no surface can forget the rule, and a concurrent
//! writer cannot slip between the read of the held instant and this write: a
//! second writer of the record is the ordinary write–write conflict.
//!
//! A follower's apply never comes here. The leader's commit settled the instant
//! and the replica applies the bytes, so two nodes cannot reach two answers.
//!
//! # The rule
//!
//! For a present write to a table whose declaration is in force or retired:
//!
//! - an instant the write already carries stands (`EXPIRE <when>`);
//! - an instant cleared on purpose stays cleared (`EXPIRE NONE`, `PERSIST`);
//! - a plain write over a record that is still answered keeps the instant that
//!   record carries — so an edit never makes an expiring record permanent, and
//!   never extends it;
//! - a plain write that creates the record — nothing answered at that address —
//!   gets the declared lifetime from now, when one is in force.
//!
//! A table that declares nothing is not touched, which is what keeps the
//! key-value verbs' semantics — a plain `SET` clears an instant — on every table
//! that did not opt in.

use std::collections::{BTreeMap, BTreeSet};

use tessari_encoding::{LogRecord, RecordValue};
use tessari_types::{DatabaseId, NamespaceId, TableId};

use crate::catalog::{Catalog, TableExpiry};
use crate::error::Result;
use crate::store::Store;
use crate::transaction::{Lifetime, RecordAddress};

/// Settle the instant of every write `record` makes to a table that declares
/// expiry; `None` when nothing needed changing.
///
/// # Errors
///
/// A backend or catalog error.
pub(crate) fn admit(
    store: &Store,
    record: &LogRecord,
    now: u64,
    lifetimes: &BTreeMap<RecordAddress, Lifetime>,
) -> Result<Option<LogRecord>> {
    let mut view = store.begin_local()?;
    let declared = declared_in(&mut view, record)?;
    if declared.is_empty() {
        return Ok(None);
    }
    let mut mutations = record.mutations().to_vec();
    let mut stamped = false;
    for mutation in &mut mutations {
        let table = (mutation.namespace, mutation.database, mutation.table);
        let Some(expire) = declared.get(&table) else {
            continue;
        };
        if !matches!(mutation.value.value(), RecordValue::Present(_))
            || mutation.value.expires().is_some()
        {
            continue;
        }
        let address = RecordAddress::new(table.0, table.1, table.2, mutation.id.clone());
        let at = match lifetimes.get(&address) {
            Some(Lifetime::Cleared) => continue,
            Some(Lifetime::Carried(at)) => Some(*at),
            None => match view.read_newest_stamped(&address)? {
                // Still answered: its instant, if it has one, stands.
                Some((_, held)) if !held.value().is_tombstone() && !held.is_expired_at(now) => {
                    held.expires()
                }
                // Nothing is answered here, so this write creates the record.
                _ => expire
                    .default_lifetime()
                    .map(|lifetime| now.saturating_add(millis(lifetime))),
            },
        };
        if let Some(at) = at {
            mutation.value = mutation.value.clone().expiring(at);
            stamped = true;
        }
    }
    if !stamped {
        return Ok(None);
    }
    let mut carried = LogRecord::at(record.epoch(), mutations);
    if let Some(order) = record.order() {
        carried.set_order(order);
    }
    Ok(Some(carried))
}

/// The instant [`admit`] will settle for this transaction's own present write
/// at `address` that carries none of its own, as the transaction sees it now:
/// what `TTL`, `PERSIST` and `INCR` answer before the commit (Q-953).
///
/// The same branches as [`admit`], read at this transaction's snapshot rather
/// than the committed state the commit builds on; a second writer of the
/// record between the two is the ordinary write–write conflict, so the commit
/// cannot settle another instant for a record it lands.
///
/// # Errors
///
/// A backend or catalog error.
pub(crate) fn pending(
    transaction: &mut crate::transaction::Transaction<'_>,
    address: &RecordAddress,
    lifetime: Option<Lifetime>,
    now: u64,
) -> Result<Option<u64>> {
    let Some(definition) = Catalog::new(transaction).table(address.table)? else {
        return Ok(None);
    };
    if definition.namespace != address.namespace || definition.database != address.database {
        return Ok(None);
    }
    let Some(expire) = definition.expire else {
        return Ok(None);
    };
    Ok(match lifetime {
        Some(Lifetime::Cleared) => None,
        Some(Lifetime::Carried(at)) => Some(at),
        None => match transaction.read_stamped_at(address)? {
            Some(held) if !held.value().is_tombstone() && !held.is_expired_at(now) => {
                held.expires()
            }
            _ => expire
                .default_lifetime()
                .map(|lifetime| now.saturating_add(millis(lifetime))),
        },
    }
    .filter(|at| *at > now))
}

/// The tables `record` writes that declare expiry, by tenancy and id.
fn declared_in(
    view: &mut crate::transaction::Transaction<'_>,
    record: &LogRecord,
) -> Result<BTreeMap<(NamespaceId, DatabaseId, TableId), TableExpiry>> {
    let mut found = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for mutation in record.mutations() {
        let table = (mutation.namespace, mutation.database, mutation.table);
        if !seen.insert(table) {
            continue;
        }
        let Some(definition) = Catalog::new(view).table(mutation.table)? else {
            continue;
        };
        if definition.namespace != mutation.namespace || definition.database != mutation.database {
            continue;
        }
        if let Some(expire) = definition.expire {
            found.insert(table, expire);
        }
    }
    Ok(found)
}

/// A lifetime in whole milliseconds; one too long for the clock never ends.
fn millis(span: tessari_types::Duration) -> u64 {
    u64::try_from(span.seconds())
        .ok()
        .and_then(|seconds| seconds.checked_mul(1000))
        .and_then(|whole| whole.checked_add(u64::from(span.nanos() / 1_000_000)))
        .unwrap_or(u64::MAX)
}
