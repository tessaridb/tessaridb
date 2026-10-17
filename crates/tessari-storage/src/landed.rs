//! The newest version this store has landed, held in memory so that beginning a
//! transaction does not read it from the engine.
//!
//! # Contract
//!
//! A monotonic register over the store's version counter.
//!
//! - **Sequential specification.** [`Landed::publish`] raises the held version to
//!   `max(held, version)`; [`Landed::version`] answers the held version.
//! - **Linearizable**, at the `fetch_max` of `publish` and the load of
//!   `version`. Both are **wait-free** where the target has an atomic maximum
//!   (aarch64 with LSE) and lock-free where it is a compare-and-swap loop.
//! - **Never ahead of the engine.** A version is published only after the batch
//!   that wrote it has landed, so a transaction that begins at the held version
//!   reads nothing that has not landed. It may lag the engine between a landing
//!   and its publish; a snapshot taken in that window is simply older, which is
//!   what a transaction that began a moment earlier would have had.
//! - **Published before the committer is answered**, so a caller's next
//!   statement begins at or above its own write.
//!
//! # Ordering
//!
//! - `publish`: `Release`. It follows the engine's apply of the batch and pairs
//!   with the `Acquire` in `version`, so a reader that sees a version also sees
//!   every write of the batches up to it.
//! - `version`: `Acquire`, paired with the `Release` above.
//!
//! Nothing else is published through this value, and there is no store-to-load
//! handshake with another location.
//!
//! # What still reads the engine
//!
//! Every comparison that needs the exact tail (whether an index is current,
//! the bound of a historical read, the version a commit takes) keeps reading
//! the engine through [`crate::Store::committed_version`]. Only the snapshot a
//! transaction begins at comes from here.

use std::sync::atomic::{AtomicU64, Ordering};

use tessari_types::Sequence;

/// The newest version landed, on a cache line of its own: every transaction
/// reads it and every commit writes it.
#[derive(Debug, Default)]
#[repr(align(128))]
pub(crate) struct Landed {
    version: AtomicU64,
}

impl Landed {
    /// Raise the held version to `version`, if it is higher.
    pub(crate) fn publish(&self, version: Sequence) {
        // Release: follows the landing of the batch at `version`; pairs with the
        // Acquire in `version()`.
        self.version.fetch_max(version.get(), Ordering::Release);
    }

    /// The newest version published.
    pub(crate) fn version(&self) -> Sequence {
        // Acquire: pairs with the Release in `publish()`.
        Sequence::new(self.version.load(Ordering::Acquire))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::Landed;
    use tessari_types::Sequence;

    #[test]
    fn a_lower_version_never_moves_it_back() {
        let landed = Landed::default();
        landed.publish(Sequence::new(7));
        landed.publish(Sequence::new(3));
        assert_eq!(landed.version(), Sequence::new(7));
    }

    #[test]
    fn concurrent_publishers_leave_the_highest_and_a_reader_never_goes_back() {
        const WRITERS: u64 = 4;
        const EACH: u64 = 10_000;
        let landed = Landed::default();
        std::thread::scope(|scope| {
            for writer in 0..WRITERS {
                let landed = &landed;
                scope.spawn(move || {
                    for step in 0..EACH {
                        landed.publish(Sequence::new(step * WRITERS + writer));
                    }
                });
            }
            let landed = &landed;
            scope.spawn(move || {
                let mut seen = Sequence::ZERO;
                for _ in 0..EACH {
                    let now = landed.version();
                    assert!(
                        now >= seen,
                        "the held version went from {seen:?} to {now:?}"
                    );
                    seen = now;
                }
            });
        });
        assert_eq!(landed.version(), Sequence::new(EACH * WRITERS - 1));
    }
}
