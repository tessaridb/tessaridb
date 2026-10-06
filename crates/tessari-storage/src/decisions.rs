//! Who answers a reader that meets an intent its own copy cannot decide
//! (ADR-0112 D13d).
//!
//! The caller of a transaction across leaders is told once a majority holds
//! the decision, before every participant has resolved its intents (D13c). A
//! node whose copy of the record does not hold the decision yet would read
//! such a transaction as invisible after its caller was told it committed —
//! so a reader asks the record's leader. Storage does not speak to peers; the
//! node installs the asker, and a store with none answers from its own copy.

use std::sync::{Arc, OnceLock};

use tessari_encoding::{TransactionId, TransactionRecord};
use tessari_types::Reach;

use crate::store::Store;

/// Asks the leader of a transaction's record range for its decision.
pub trait Decisions: core::fmt::Debug + Send + Sync {
    /// The record as `coordinator`'s leader holds it once a majority holds its
    /// decision, or `None` when it cannot say — not reached, or the record
    /// undecided. Never aborts a live transaction: a reader asking is not a
    /// participant giving up.
    fn decided(&self, transaction: TransactionId, coordinator: Reach) -> Option<TransactionRecord>;
}

/// The asker a store's readers use, installed once by the node.
pub(crate) type Installed = Arc<OnceLock<Arc<dyn Decisions>>>;

impl Store {
    /// Answer readers' questions about undecided intents with `decisions`.
    /// The first installation stands; a node installs one for its life.
    pub fn answer_decisions_with(&self, decisions: Arc<dyn Decisions>) {
        drop(self.decisions.set(decisions));
    }

    /// `transaction`'s record as its leader answers it, if anybody can.
    pub(crate) fn asked_decision(
        &self,
        transaction: TransactionId,
        coordinator: Reach,
    ) -> Option<TransactionRecord> {
        let asker = self.decisions.get()?;
        let began = std::time::Instant::now();
        let decided = asker.decided(transaction, coordinator);
        // For the per-phase attribution of a commit across leaders (Q-931):
        // a read that waits on another node is time nobody sees otherwise.
        let elapsed_us = u64::try_from(began.elapsed().as_micros()).unwrap_or(u64::MAX);
        tracing::debug!(
            known = decided.is_some(),
            elapsed_us,
            "a reader asked a cross-leader record's leader"
        );
        decided
    }
}
