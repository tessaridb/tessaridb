//! A condition a shard's leader applies before it sends records (ADR-0097).
//!
//! # The asker's visibility, as data
//!
//! What this session may read of a table is a set of field names
//! ([`Visible`]), so the asker sends it and the leader takes those fields away
//! before the condition sees the record — the order the asker's own read uses.
//! No user is resolved on the leader and nobody is impersonated: the peer asking
//! is entitled to the shard's raw records already, and the narrowing changes
//! only how many of them travel.
//!
//! # Keep anything the asker might keep
//!
//! The asker tests every record it receives again. So the leader drops a record
//! only on the three answers the asker reads as *no* — `false`, `NONE`, `NULL` —
//! and keeps it on anything else, a failure included: the asker then meets the
//! same failure and refuses in its own words. A leader that dropped too much
//! would be a record missing from an answer that says it is whole.

use tessari_encoding::decode_payload;
use tessari_storage::Store;
use tessari_types::{RecordId, Value};

use crate::error::Result;
use crate::evaluate::Scope;
use crate::redact::{Visible, seen};
use crate::session::Session;

/// What a node asks a shard's leader to narrow its records by.
#[derive(Debug, Clone, PartialEq)]
pub struct Pushed {
    /// The fields the asker may read; `None` for all of them.
    pub visible: Visible,
    /// The condition, as [`tessari_ql::portable`] wrote it.
    pub condition: String,
    /// The values its parameters stand for.
    pub parameters: tessari_ql::Parameters,
}

/// The records of `found` that `pushed` keeps, as stored.
///
/// # Errors
///
/// A condition that does not read back, or a store that cannot be read. A
/// record whose payload does not decode is kept, for the asker to refuse.
pub fn keeping(
    store: &Store,
    pushed: &Pushed,
    found: Vec<(RecordId, Vec<u8>)>,
) -> Result<Vec<(RecordId, Vec<u8>)>> {
    let condition = tessari_ql::bound_condition(&pushed.condition, &pushed.parameters)?;
    let session = Session::new(store);
    let mut transaction = store.begin()?;
    let mut kept = Vec::with_capacity(found.len());
    for (id, payload) in found {
        let answered = decode_payload(&payload).map(|record| {
            let record = seen(record, &pushed.visible);
            session.evaluate_in(
                &mut transaction,
                &condition,
                Scope::of(&record).identified(&id),
            )
        });
        let refused = matches!(
            answered,
            Ok(Ok(Value::Bool(false) | Value::None | Value::Null))
        );
        if !refused {
            kept.push((id, payload));
        }
    }
    transaction.rollback();
    Ok(kept)
}

/// What a node asks a shard's leader to rank its records by, and how many of
/// them to send (ADR-0102).
#[derive(Debug, Clone, PartialEq)]
pub struct Ordered {
    /// The fields the asker may read; `None` for all of them.
    pub visible: Visible,
    /// The keys, in the statement's order.
    pub keys: Vec<OrderKey>,
    /// How many records the asker's answer can take from one shard — the
    /// statement's `LIMIT` plus its `START`.
    pub most: u64,
}

/// One key of an [`Ordered`].
#[derive(Debug, Clone, PartialEq)]
pub struct OrderKey {
    /// The key, as [`tessari_ql::portable`] wrote it.
    pub key: String,
    /// The values its parameters stand for.
    pub parameters: tessari_ql::Parameters,
    /// Whether the order is reversed.
    pub descending: bool,
}

/// The records `ordered` puts first, out of every page `pages` hands over, in
/// identity order.
///
/// Ranked by the same collector and comparator the asker's `ORDER BY` uses,
/// over the record with the asker's hidden fields removed, so the first
/// `most` here are the first `most` the asker would rank of this shard. Ties
/// fall to the identity, which is what makes the asker's merge of every
/// shard's first `most` the whole table's.
///
/// A record whose payload or key does not evaluate is kept beside the ranked
/// ones, for the asker to meet the same failure and refuse in its own words.
///
/// # Errors
///
/// A key that does not read back, a page that could not be read, or a store
/// that cannot be read.
pub fn leading(
    store: &Store,
    ordered: &Ordered,
    mut pages: impl FnMut() -> Result<Option<Vec<(RecordId, Vec<u8>)>>>,
) -> Result<Vec<(RecordId, Vec<u8>)>> {
    let order = ordered
        .keys
        .iter()
        .map(|key| {
            Ok(tessari_ql::Ordering {
                key: tessari_ql::bound_condition(&key.key, &key.parameters)?,
                descending: key.descending,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let most = usize::try_from(ordered.most).unwrap_or(usize::MAX);
    let session = Session::new(store);
    let mut transaction = store.begin()?;
    let mut topmost = crate::shape::Topmost::keeping(&order, Some(most));
    let mut unranked = Vec::new();
    while let Some(page) = pages()? {
        for (id, payload) in page {
            let Ok(record) = decode_payload(&payload) else {
                unranked.push((id, payload));
                continue;
            };
            let record = seen(record, &ordered.visible);
            let keys: Result<Vec<Value>> = order
                .iter()
                .map(|key| {
                    session.evaluate_in(
                        &mut transaction,
                        &key.key,
                        Scope::of(&record).identified(&id),
                    )
                })
                .collect();
            match keys {
                Ok(keys) => topmost.offer(keys, id, Value::Bytes(payload)),
                Err(_) => unranked.push((id, payload)),
            }
        }
    }
    transaction.rollback();
    let mut kept: Vec<(RecordId, Vec<u8>)> = topmost
        .finish()
        .into_iter()
        .filter_map(|(id, payload)| match payload {
            Value::Bytes(payload) => Some((id, payload)),
            _ => None,
        })
        .collect();
    kept.extend(unranked);
    kept.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(kept)
}
