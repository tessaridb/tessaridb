//! The exact state of a rollup's float sums, kept beside its rows (ADR-0114,
//! Q-927).
//!
//! A rollup row holds each `sum` as a rounded number, and a rounded total
//! re-entered as one value is no longer the exact sum the read path answers —
//! one insert at a time, a run of floats drifts from the total a recomputation
//! of the same window gives. So the exact state is kept as well: one system row
//! per rollup and key, holding the state of the window most recently written
//! for that key. The session decides what the state means and when it applies;
//! this module only keeps it.
//!
//! It lives in the system tenancy, written in the transaction that writes the
//! row, so the two never disagree and a reader of the rollup never sees it. One
//! row per key rather than per window bounds it by the rollup's keys, not by
//! time, and `DROP ROLLUP` takes it.
//!
//! The identity is `rollup table · key`, the key being whatever bytes the
//! caller names a key by.

use tessari_encoding::{KeyWriter, decode_payload, encode_payload};
use tessari_types::{RecordId, TableId, Value};

use crate::catalog::system::{self, ROLLUP_STATES};
use crate::error::Result;
use crate::transaction::Transaction;

fn rollup_prefix(rollup: TableId) -> Vec<u8> {
    let mut writer = KeyWriter::new();
    writer.put_u32(rollup.get());
    writer.finish()
}

fn state_id(rollup: TableId, key: &[u8]) -> RecordId {
    let mut id = rollup_prefix(rollup);
    id.extend_from_slice(key);
    RecordId::Bytes(id)
}

impl Transaction<'_> {
    /// The state kept for one key of a rollup, if any.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot be read or the state cannot be
    /// decoded.
    pub fn rollup_state(&self, rollup: TableId, key: &[u8]) -> Result<Option<Value>> {
        let address = system::address(ROLLUP_STATES, state_id(rollup, key));
        Ok(match self.get(&address)? {
            Some(payload) => Some(decode_payload(&payload)?),
            None => None,
        })
    }

    /// Keep `state` for one key of a rollup, replacing what was kept.
    pub fn put_rollup_state(&mut self, rollup: TableId, key: &[u8], state: &Value) {
        self.put(
            system::address(ROLLUP_STATES, state_id(rollup, key)),
            encode_payload(state).into_bytes(),
        );
    }

    /// Remove every state a rollup has.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot be read.
    pub fn forget_rollup_states(&mut self, rollup: TableId) -> Result<()> {
        for (id, _) in self.system_rows_prefixed(ROLLUP_STATES, &rollup_prefix(rollup))? {
            self.delete(system::address(ROLLUP_STATES, id));
        }
        Ok(())
    }
}
