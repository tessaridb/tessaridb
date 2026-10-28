//! Keys a caller presents with one request, for vaults in keyed custody (ADR-0136).
//!
//! # Why a task-local and not a field of the transaction
//!
//! A request runs on one task from the first statement to the answer, and a statement opens transactions in many
//! places. Scoping the keys to the request's task reaches every one of them without threading a parameter through
//! each, and it fails in the safe direction: work moved to another task sees no key and answers `Sealed`, never a
//! key it was not given. The keys are dropped — and cleared, `SecretBytes` zeroizing on drop — the moment the
//! request's future completes, whatever it answered, so nothing in this process holds them between requests.

use std::collections::HashMap;
use std::future::Future;

use tessari_vault::SecretBytes;

use crate::error::Result;

tokio::task_local! {
    static PRESENTED: Presented;
}

/// The keys one request presented, by the scope of the vault each one opens.
#[derive(Default)]
pub struct Presented(HashMap<Vec<u8>, SecretBytes>);

impl Presented {
    /// No keys.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// Present `key` for the vault whose scope is `scope` (`vault_key_scope`).
    pub fn insert(&mut self, scope: Vec<u8>, key: SecretBytes) {
        self.0.insert(scope, key);
    }

    /// Whether no key was presented.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Written by hand: how many keys, never which bytes.
impl std::fmt::Debug for Presented {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Presented({} keys)", self.0.len())
    }
}

/// Run `work` with `keys` presented to every keyed vault it opens, and drop them when it ends.
pub async fn presenting<F: Future>(keys: Presented, work: F) -> F::Output {
    PRESENTED.scope(keys, work).await
}

/// Do something with the key presented for `scope`, or answer `None` when this task presented none for it.
pub(super) fn with_presented<T>(
    scope: &[u8],
    act: impl FnOnce(&SecretBytes) -> Result<T>,
) -> Option<Result<T>> {
    PRESENTED
        .try_with(|presented| presented.0.get(scope).map(act))
        .ok()
        .flatten()
}
