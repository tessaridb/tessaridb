//! Per-pool counters this process counts at the admission gate (SG3 W3).
//!
//! Process-local for [`crate::tally`]'s reason: a count that outlived the
//! process that observed it would be a claim nobody can check. Keyed by the
//! names the operator wrote — the `INFO FOR POOL` rows and the `/metrics`
//! labels read human names, and the session holds the names at the gate, so
//! ids would buy a lookup nothing here.
//!
//! Counters, not state: nothing reads them to decide anything, so they are
//! relaxed atomics the way the cluster tally's are.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;

/// The counts one pool holds on this process.
#[derive(Debug, Default)]
struct PoolCounters {
    /// Matching scripts running right now.
    running: AtomicU64,
    /// Matching scripts refused `PoolAtCapacity` since this process opened
    /// the store.
    refused: AtomicU64,
}

/// The pools this process has counted at the gate, keyed by the
/// (namespace, pool) names the rows were declared under.
#[derive(Debug, Default)]
pub struct PoolTally {
    counters: DashMap<(String, String), PoolCounters>,
}

/// One pool's counts as [`PoolTally::entries`] hands them over, sorted by
/// (namespace, pool) so an `INFO` answer and a metrics render read the same
/// order twice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolReading {
    /// The namespace the pool is declared in.
    pub namespace: String,
    /// The pool's name, unique within the namespace.
    pub pool: String,
    /// Matching scripts running right now on this process.
    pub running: u64,
    /// Matching scripts refused since this process opened the store.
    pub refused: u64,
}

/// A slot the gate took for one running script; the running count returns
/// when the slot drops, whichever path the script ends by.
#[derive(Debug)]
pub struct PoolSlot {
    tally: Arc<PoolTally>,
    key: (String, String),
}

impl PoolTally {
    /// Take one of the pool's slots. `None` when the pool is at capacity,
    /// with the refusal counted — the `INFO FOR POOL` row and the
    /// `tessari_pool_refused_total` series are how an operator watches the
    /// gate bite, and a refusal they cannot see is a bound that does not
    /// exist.
    pub fn take(
        self: &Arc<Self>,
        namespace: &str,
        pool: &str,
        max_concurrent: u64,
    ) -> Option<PoolSlot> {
        let key = (namespace.to_owned(), pool.to_owned());
        // One shard acquisition for the read-modify-write: a take is decided
        // atomically with the count it changes.
        let held = self.counters.entry(key.clone()).or_default();
        if held.running.load(Ordering::Relaxed) >= max_concurrent {
            held.refused.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        held.running.fetch_add(1, Ordering::Relaxed);
        drop(held);
        Some(PoolSlot {
            tally: Arc::clone(self),
            key,
        })
    }

    /// Every pool counted here, sorted by (namespace, pool).
    pub fn entries(&self) -> Vec<PoolReading> {
        let mut held: Vec<_> = self
            .counters
            .iter()
            .map(|entry| {
                let (namespace, pool) = entry.key();
                PoolReading {
                    namespace: namespace.clone(),
                    pool: pool.clone(),
                    running: entry.running.load(Ordering::Relaxed),
                    refused: entry.refused.load(Ordering::Relaxed),
                }
            })
            .collect();
        // Iteration order is not stable (the guard discipline's reason), so
        // the snapshot sorts — a deterministic render, not a consensus input.
        held.sort_by(|a, b| (&a.namespace, &a.pool).cmp(&(&b.namespace, &b.pool)));
        held
    }
}

impl Drop for PoolSlot {
    fn drop(&mut self) {
        if let Some(held) = self.tally.counters.get(&self.key) {
            held.running.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PoolReading, PoolTally};
    use std::sync::Arc;

    fn tally() -> Arc<PoolTally> {
        Arc::new(PoolTally::default())
    }

    #[test]
    fn the_take_within_the_limit_holds_and_the_next_is_refused_and_counted() {
        let tally = tally();
        let slot = tally.take("prod", "web", 1).expect("the pool's only slot");
        assert!(
            tally.take("prod", "web", 1).is_none(),
            "a second take at capacity held"
        );
        assert_eq!(
            tally.entries(),
            [PoolReading {
                namespace: "prod".to_owned(),
                pool: "web".to_owned(),
                running: 1,
                refused: 1,
            }],
        );
        drop(slot);
    }

    #[test]
    fn a_released_slot_admits_the_next_take_and_keeps_the_refusal_count() {
        let tally = tally();
        let held = tally.take("prod", "web", 1).expect("the pool's only slot");
        assert!(tally.take("prod", "web", 1).is_none());
        drop(held);
        let next = tally
            .take("prod", "web", 1)
            .expect("the released slot admitted the next take");
        assert_eq!(
            tally.entries(),
            [PoolReading {
                namespace: "prod".to_owned(),
                pool: "web".to_owned(),
                running: 1,
                refused: 1,
            }],
        );
        drop(next);
    }

    #[test]
    fn pools_count_apart_and_the_snapshot_sorts_by_namespace_then_pool() {
        let tally = tally();
        let prod_web = tally.take("prod", "web", 4).expect("prod.web");
        let prod_api = tally.take("prod", "api", 4).expect("prod.api");
        let stage_web = tally.take("stage", "web", 4).expect("stage.web");
        assert!(
            tally.take("prod", "api", 1).is_none(),
            "a take past one pool's own limit was counted on another pool"
        );
        assert_eq!(
            tally.entries(),
            [
                PoolReading {
                    namespace: "prod".to_owned(),
                    pool: "api".to_owned(),
                    running: 1,
                    refused: 1,
                },
                PoolReading {
                    namespace: "prod".to_owned(),
                    pool: "web".to_owned(),
                    running: 1,
                    refused: 0,
                },
                PoolReading {
                    namespace: "stage".to_owned(),
                    pool: "web".to_owned(),
                    running: 1,
                    refused: 0,
                },
            ],
        );
        drop(prod_web);
        drop(prod_api);
        drop(stage_web);
    }
}
