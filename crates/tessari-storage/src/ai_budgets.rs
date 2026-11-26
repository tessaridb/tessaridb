//! The `ai::` call budgets: one token bucket per provider, per node, in
//! memory (G102 C8).
//!
//! A budget is deliberately not a catalog fact: it bounds what THIS node
//! spends, the way the per-surface running ceilings do, and a replica's
//! spend is its own. Exhaustion refuses rather than queues — a queued
//! provider call inside a read is a stalled read, and the caller's retry is
//! the honest queue.

use std::time::{Duration, Instant};

use dashmap::DashMap;

/// One provider's token bucket.
#[derive(Debug)]
struct Bucket {
    /// Tokens available now.
    tokens: f64,
    /// When the bucket was last refilled.
    at: Instant,
}

/// Every provider's bucket on this node, shared across the store's handles.
#[derive(Debug, Default)]
pub struct AiBudgets {
    held: DashMap<String, Bucket>,
}

/// A declared budget as the bucket's fraction arithmetic holds it. A budget
/// past `u32::MAX` calls a span is not one anybody declared, so the clamp is
/// stated rather than a silent `as`.
fn whole(budget: u64) -> f64 {
    f64::from(u32::try_from(budget.max(1)).unwrap_or(u32::MAX))
}

impl AiBudgets {
    /// Spend one token of `provider`'s budget, refilling first; `false` when
    /// the bucket is empty — the refusal is the caller's to name.
    pub fn spend(&self, provider: &str, budget: u64, span: Duration) -> bool {
        let span_seconds = span.as_secs_f64().max(1.0);
        let rate = whole(budget) / span_seconds;
        let mut entry = self.held.entry(provider.to_owned()).or_insert(Bucket {
            tokens: whole(budget),
            at: Instant::now(),
        });
        let elapsed = entry.at.elapsed().as_secs_f64();
        entry.tokens = (entry.tokens + elapsed * rate).min(whole(budget));
        entry.at = Instant::now();
        if entry.tokens >= 1.0 {
            entry.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}
