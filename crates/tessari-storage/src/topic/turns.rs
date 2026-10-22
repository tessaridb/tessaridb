//! Which partition a split topic's next keyless message takes, on this node
//! (ADR-0131).
//!
//! A turn is a spread, not an order: nothing reads it back, so it is counted in
//! memory per process for the reason the public rate is — a count in the log
//! would charge every append a second write. A restart starts again at the
//! first partition, and two nodes each take their own turns; both only move
//! which partition a message lands in, never whether it lands.

use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;

use tessari_types::TableId;

/// The next turn of each split topic, on this node.
#[derive(Debug, Default)]
pub(crate) struct PartitionTurns {
    next: DashMap<TableId, AtomicU64>,
}

impl PartitionTurns {
    /// The turn the next keyless message of `topic` takes, counting from 0.
    pub(crate) fn take(&self, topic: TableId) -> u64 {
        // Read-modify-write through the entry, so the shard guard is held for
        // the increment alone; the count publishes nothing else, so `Relaxed`.
        self.next
            .entry(topic)
            .or_default()
            .fetch_add(1, Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use tessari_types::TableId;

    use super::PartitionTurns;

    #[test]
    fn each_topic_takes_its_own_turns_from_zero() {
        let turns = PartitionTurns::default();
        let (seven, nine) = (TableId::new(7), TableId::new(9));
        assert_eq!(
            [
                turns.take(seven),
                turns.take(seven),
                turns.take(nine),
                turns.take(seven)
            ],
            [0, 1, 0, 2]
        );
    }
}
