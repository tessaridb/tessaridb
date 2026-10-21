//! Idempotent producers: an identity per producer, and the batches it sent.
//!
//! A producer retries a batch whose answer it did not hear. Its identity and
//! the batch's sequence say whether this node already wrote it, so a retry
//! answers the offset it took the first time instead of writing it again. The
//! last five batches per producer and partition are kept, which is what a
//! client may have in flight at once.
//!
//! Held in node memory: across a restart a producer is unknown again and its
//! next batch is taken at whatever sequence it carries — the window in which a
//! retry could repeat is the restart, and the client re-initialises on the
//! errors it then meets.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicI64, Ordering};

use dashmap::DashMap;

/// Batches remembered per producer and partition.
const REMEMBERED: usize = 5;

/// One batch a producer wrote: its first and last sequence and its offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Written {
    first: i32,
    last: i32,
    base_offset: i64,
}

#[derive(Debug, Default)]
struct Recent {
    epoch: i16,
    batches: VecDeque<Written>,
}

/// What a producer's next batch is, against what it sent before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Sequenced {
    /// Not written yet: write it.
    Fresh,
    /// Written already, at this offset: answer that.
    Repeated(i64),
    /// Written already, too long ago to say at which offset.
    Duplicate,
    /// A gap: a batch before it never arrived.
    OutOfOrder,
}

/// Every producer this node has handed an identity to.
#[derive(Debug)]
pub(crate) struct Producers {
    next: AtomicI64,
    recent: DashMap<(i64, String, i32), Recent>,
}

impl Producers {
    pub(crate) fn new() -> Self {
        // Started from the clock, so an identity handed out after a restart is
        // not one a producer from before it still holds.
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| i64::try_from(since.as_millis()).unwrap_or(0));
        Self {
            next: AtomicI64::new(millis.saturating_mul(1024)),
            recent: DashMap::new(),
        }
    }

    /// A new producer identity.
    pub(crate) fn issue(&self) -> i64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }

    /// What a batch from `producer` at `epoch` starting at `first` is.
    pub(crate) fn sequenced(&self, key: &(i64, String, i32), epoch: i16, first: i32) -> Sequenced {
        let Some(recent) = self.recent.get(key) else {
            return Sequenced::Fresh;
        };
        if epoch > recent.epoch {
            return Sequenced::Fresh;
        }
        if let Some(written) = recent.batches.iter().find(|written| written.first == first) {
            return Sequenced::Repeated(written.base_offset);
        }
        let Some(latest) = recent.batches.back() else {
            return Sequenced::Fresh;
        };
        let expected = latest.last.checked_add(1).unwrap_or(0);
        if first == expected {
            Sequenced::Fresh
        } else if first < expected {
            Sequenced::Duplicate
        } else {
            Sequenced::OutOfOrder
        }
    }

    /// Remember a batch written at `base_offset`.
    pub(crate) fn written(
        &self,
        key: (i64, String, i32),
        epoch: i16,
        first: i32,
        count: i32,
        base_offset: i64,
    ) {
        let last = first
            .checked_add(count.saturating_sub(1))
            .unwrap_or(i32::MAX);
        let mut recent = self.recent.entry(key).or_default();
        if epoch > recent.epoch {
            recent.epoch = epoch;
            recent.batches.clear();
        }
        recent.batches.push_back(Written {
            first,
            last,
            base_offset,
        });
        while recent.batches.len() > REMEMBERED {
            recent.batches.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_retried_batch_answers_its_first_offset_and_a_gap_is_refused() {
        let producers = Producers::new();
        let key = (producers.issue(), "a.b.t".to_owned(), 0);
        assert_eq!(producers.sequenced(&key, 0, 0), Sequenced::Fresh);
        producers.written(key.clone(), 0, 0, 3, 10);
        assert_eq!(producers.sequenced(&key, 0, 0), Sequenced::Repeated(10));
        assert_eq!(producers.sequenced(&key, 0, 3), Sequenced::Fresh);
        assert_eq!(producers.sequenced(&key, 0, 5), Sequenced::OutOfOrder);
        for batch in 1..7 {
            producers.written(key.clone(), 0, batch * 3, 3, 10 + i64::from(batch) * 3);
        }
        assert_eq!(
            producers.sequenced(&key, 0, 0),
            Sequenced::Duplicate,
            "out of the window"
        );
        assert_eq!(
            producers.sequenced(&key, 1, 0),
            Sequenced::Fresh,
            "a new epoch starts over"
        );
    }
}
