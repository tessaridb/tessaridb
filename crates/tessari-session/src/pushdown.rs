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
