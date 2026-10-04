//! A rollup row's sums kept exact from one insert to the next (ADR-0114,
//! Q-927).
//!
//! A row stores each `sum` rounded. Re-entering that rounded number as the one
//! value folded so far makes a run of inserts a running rounded total, which is
//! not the exact sum a recomputation of the window answers. So beside the row
//! the exact state of its sums is kept — for the window most recently written
//! for its key — and an insert into that window folds from the state. An
//! insert into any other window that already has a row cannot, and the row is
//! recomputed from the raw window instead: exact as well, and paid only when a
//! key's writes move to another window that already holds a row, which in a
//! series is late data.

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};
use tessari_encoding::encode_payload;
use tessari_ql::{Aggregate, Span};
use tessari_storage::{RollupDeclaration, RollupFold};
use tessari_types::{Number, Value};

use super::Row;
use crate::accumulate::Accumulator;
use crate::error::Result;

const WINDOW: &str = "window";
const KEY: &str = "key";
const SUMS: &str = "sums";

/// Whether a rollup computes any `sum`.
pub(super) fn has_sums(rollup: &RollupDeclaration) -> bool {
    rollup
        .computes
        .iter()
        .any(|compute| compute.fold == RollupFold::Sum)
}

/// The bytes a key's state is kept under: a digest of the key, which the state
/// also holds and is checked against.
pub(super) fn state_key(key: &Value) -> Vec<u8> {
    Sha256::digest(encode_payload(key).into_bytes())
        .get(..16)
        .map(<[u8]>::to_vec)
        .unwrap_or_default()
}

impl Row {
    /// The exact state of this row's sums, to keep beside it. A sum whose state
    /// cannot travel exactly — one that overflowed — is left out, and a row
    /// missing one is recomputed rather than folded into.
    pub(super) fn sums_state(&self, rollup: &RollupDeclaration) -> Value {
        let mut sums = BTreeMap::new();
        for compute in &rollup.computes {
            if compute.fold != RollupFold::Sum {
                continue;
            }
            if let Some(state) = self.values.get(&compute.name).and_then(Accumulator::state) {
                sums.insert(compute.name.clone(), state);
            }
        }
        Value::Object(BTreeMap::from([
            (
                WINDOW.to_owned(),
                Value::Number(Number::Integer(self.window)),
            ),
            (KEY.to_owned(), self.key.clone()),
            (SUMS.to_owned(), Value::Object(sums)),
        ]))
    }

    /// Replace this row's sums with the exact ones `kept` holds for its window
    /// and key; `false` when it holds none for them, and the row must be
    /// recomputed from its raw window.
    ///
    /// # Errors
    ///
    /// Whatever folding a kept state in raises.
    pub(super) fn take_sums(
        &mut self,
        rollup: &RollupDeclaration,
        kept: Option<&Value>,
    ) -> Result<bool> {
        let Some(Value::Object(kept)) = kept else {
            return Ok(false);
        };
        if kept.get(WINDOW) != Some(&Value::Number(Number::Integer(self.window)))
            || kept.get(KEY) != Some(&self.key)
        {
            return Ok(false);
        }
        let Some(Value::Object(sums)) = kept.get(SUMS) else {
            return Ok(false);
        };
        for compute in &rollup.computes {
            if compute.fold != RollupFold::Sum {
                continue;
            }
            let Some(state) = sums.get(&compute.name) else {
                return Ok(false);
            };
            let mut exact = Accumulator::for_aggregate(Aggregate::Sum, Span::new(0, 0));
            if !exact.merge(state)? {
                return Ok(false);
            }
            self.values.insert(compute.name.clone(), exact);
        }
        Ok(true)
    }
}
