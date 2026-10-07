//! A rollup row's sketches, kept beside it (ADR-0122 C5).
//!
//! The row answers each sketch column with its estimate, so every read path
//! reads a number. The state the estimate came from is kept beside the row,
//! written in the same transaction, under a key no `sum` state can take: one
//! byte the sums never begin with, then a digest of the window, the key and the
//! column. An insert merges the new record into that state; a row whose state
//! is missing is recomputed from the raw window instead, which is exact too.

use sha2::{Digest, Sha256};
use tessari_encoding::encode_payload;
use tessari_storage::{RollupDeclaration, RollupFold, Transaction};
use tessari_types::{Number, Value};

use super::Row;
use crate::error::Result;

/// The byte a sketch's state key begins with, which a sum's — a bare
/// sixteen-byte digest — is never one byte longer than.
const MARKER: u8 = b's';

/// Whether a fold is a sketch.
pub(crate) const fn is_sketch(fold: RollupFold) -> bool {
    matches!(
        fold,
        RollupFold::ApproxDistinct | RollupFold::ApproxQuantile
    )
}

/// The bytes one column's sketch for one row is kept under.
pub(crate) fn state_key(window: i64, key: &Value, column: &str) -> Vec<u8> {
    let named = Value::Array(vec![
        Value::Number(Number::Integer(window)),
        key.clone(),
        Value::from(column),
    ]);
    let digest = Sha256::digest(encode_payload(&named).into_bytes());
    let mut bytes = Vec::with_capacity(17);
    bytes.push(MARKER);
    bytes.extend_from_slice(digest.get(..16).unwrap_or_default());
    bytes
}

impl Row {
    /// Merge each sketch column's kept state into this row; `false` when one
    /// is missing or not a sketch, and the row must be recomputed instead.
    pub(super) fn take_sketches(
        &mut self,
        transaction: &mut Transaction<'_>,
        rollup: &RollupDeclaration,
    ) -> Result<bool> {
        for compute in rollup
            .computes
            .iter()
            .filter(|compute| is_sketch(compute.fold))
        {
            let key = state_key(self.window, &self.key, &compute.name);
            let Some(state) = transaction.rollup_state(rollup.table, &key)? else {
                return Ok(false);
            };
            let Some(accumulator) = self.values.get_mut(&compute.name) else {
                return Ok(false);
            };
            if !accumulator.merge(&state)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Keep each sketch column's state beside the row.
    pub(super) fn keep_sketches(
        &self,
        transaction: &mut Transaction<'_>,
        rollup: &RollupDeclaration,
    ) {
        for compute in rollup
            .computes
            .iter()
            .filter(|compute| is_sketch(compute.fold))
        {
            if let Some(state) = self.values.get(&compute.name).and_then(|held| held.state()) {
                let key = state_key(self.window, &self.key, &compute.name);
                transaction.put_rollup_state(rollup.table, &key, &state);
            }
        }
    }
}

/// Remove the sketches kept beside a row that is gone.
pub(super) fn forget_sketches(
    transaction: &mut Transaction<'_>,
    rollup: &RollupDeclaration,
    window: i64,
    key: &Value,
) {
    for compute in rollup
        .computes
        .iter()
        .filter(|compute| is_sketch(compute.fold))
    {
        transaction.forget_rollup_state(rollup.table, &state_key(window, key, &compute.name));
    }
}
