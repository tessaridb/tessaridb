//! What the process has asked the allocator for, rather than what the operating
//! system has not yet taken back.
//!
//! # Why `ps` is not enough for the question this answers
//!
//! The resident figure every other workload reports is the whole process's
//! resident set. It carries the store, the harness and the answer in one number;
//! it lags, because a freed page is returned when the allocator decides to; and
//! it is a high-water mark across a run, so each reading includes whatever the
//! previous one peaked at. Those properties are acceptable for *is this growing
//! with the width of the range* and useless for *what does one answered record
//! cost*, which needs a live figure that goes down when something is dropped.
//!
//! This counts the bytes each allocation asked for and subtracts them when it is
//! freed. Two consequences, both stated rather than left to be discovered:
//!
//! - It under-reports. The allocator rounds a request up to a size class and
//!   keeps its own bookkeeping, and none of that is counted here. A number from
//!   this module is a floor on what the process is holding, never a ceiling.
//! - It is exact about *change*. `live` after minus `live` before is the bytes a
//!   piece of work is still holding, with no page-return lag and no store growth
//!   folded in, which is the only reason a per-record figure can be derived at
//!   all.
//!
//! # Why it is behind a feature and off by default
//!
//! A global allocator is global: installing one would put an atomic add on every
//! allocation in every workload, including the ones whose recorded baselines are
//! timings. Those baselines were taken without it, so making it the default
//! would quietly invalidate them — and the comparability rule this harness
//! already carries says a before and an after have to come from trees that
//! differ in the thing being measured and nothing else. Off by default, the
//! ordinary build is byte for byte what it was; on, the harness prints that it
//! is counting, exactly as it prints when it is not a release build.
//!
//! ```text
//! cargo run -p tessari-bench --release --features counting -- --workload memory
//! ```

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Bytes requested and not yet freed.
static LIVE: AtomicUsize = AtomicUsize::new(0);

/// The highest `LIVE` has reached since the last [`reset_peak`].
static PEAK: AtomicUsize = AtomicUsize::new(0);

/// The system allocator, with a running total in front of it.
pub struct Counting;

// SAFETY: every method forwards to `System` with the layout it was handed and
// returns what `System` returned, so the allocation contract is `System`'s
// unchanged. The counters are read and written with plain atomics and no pointer
// derives from them, so no allocation's validity depends on their value.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract for
        // `layout`, and it is forwarded unchanged.
        let held = unsafe { System.alloc(layout) };
        if !held.is_null() {
            took(layout.size());
        }
        held
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as above.
        let held = unsafe { System.alloc_zeroed(layout) };
        if !held.is_null() {
            took(layout.size());
        }
        held
    }

    unsafe fn dealloc(&self, held: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: `held` came from this allocator, which is `System`, with
        // `layout`; both are forwarded unchanged.
        unsafe { System.dealloc(held, layout) };
    }

    /// Forwarded rather than left to the default.
    ///
    /// The provided implementation allocates, copies and frees, which would
    /// account correctly and would also stop a growing vector from ever
    /// extending in place. A benchmark that measures a different reallocation
    /// strategy than the one shipped is measuring the harness.
    unsafe fn realloc(&self, held: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // SAFETY: `held`, `layout` and `size` satisfy `GlobalAlloc::realloc`'s
        // contract by the caller, and are forwarded unchanged.
        let moved = unsafe { System.realloc(held, layout, size) };
        if !moved.is_null() {
            // Only on success: a failed `realloc` leaves the original block
            // allocated, so subtracting here would lose it from the total.
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            took(size);
        }
        moved
    }
}

/// Add to the running total and raise the watermark if it moved.
///
/// `Relaxed` throughout, deliberately. Nothing is published through these
/// counters — no allocation's validity and no other value depends on their
/// order — so they are statistics, and a stronger ordering would buy a fence on
/// every allocation in the process to make a benchmark's arithmetic marginally
/// less approximate. The figures are read after the work they describe has
/// finished, on the thread that ran it.
fn took(size: usize) {
    let now = LIVE.fetch_add(size, Ordering::Relaxed).saturating_add(size);
    PEAK.fetch_max(now, Ordering::Relaxed);
}

/// Bytes requested and not yet freed.
#[must_use]
pub fn live() -> usize {
    LIVE.load(Ordering::Relaxed)
}

/// The highest [`live`] has been since the last [`reset_peak`].
#[must_use]
pub fn peak() -> usize {
    PEAK.load(Ordering::Relaxed)
}

/// Drop the watermark to the current total, so the next peak is this phase's.
///
/// Without it every reading after the first would report the peak of whichever
/// earlier phase allocated most, which is a real number about the wrong thing.
pub fn reset_peak() {
    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::{live, peak, reset_peak};

    /// The block the assertions are made against.
    ///
    /// Large for a reason rather than for effect. The counters are process-wide
    /// and the test harness runs tests on several threads, so every reading here
    /// carries whatever the other tests allocated in between. That noise is tens
    /// of kibibytes; at eight mebibytes it cannot reach any bound below, which
    /// is what lets these be assertions rather than approximations.
    const BLOCK: usize = 8 << 20;

    /// Whichever way the noise falls, it stays under this.
    const NOISE: usize = 1 << 20;

    /// The counter is the instrument, so it is the one thing here that cannot be
    /// taken on trust: a workload built on a counter that under-counts would
    /// report a per-record cost that is wrong in the direction that makes the
    /// answer look cheap.
    ///
    /// One test rather than two, because two would race each other for the same
    /// global counters and the second would be measuring the first.
    #[test]
    fn a_known_allocation_moves_the_total_and_the_watermark_keeps_it() {
        reset_peak();
        let before = live();
        let held = vec![0_u8; BLOCK];
        let during = live();
        assert!(
            during.saturating_sub(before) >= BLOCK,
            "{BLOCK} bytes held but the total moved by {}",
            during.saturating_sub(before)
        );
        assert!(
            peak() >= during,
            "the watermark {} sits below a total of {during}",
            peak()
        );

        drop(held);
        let after = live();
        // The drop rather than the level: another thread allocating between the
        // two readings raises `after`, and only a bound on the fall is immune to
        // it in the direction that matters.
        assert!(
            during.saturating_sub(after) >= BLOCK.saturating_sub(NOISE),
            "freeing {BLOCK} bytes moved the total down by only {}",
            during.saturating_sub(after)
        );
        // The watermark held the peak the total no longer shows — the whole
        // reason it exists.
        assert!(
            peak().saturating_sub(after) >= BLOCK.saturating_sub(NOISE),
            "the watermark fell with the total instead of keeping the peak"
        );
    }
}
