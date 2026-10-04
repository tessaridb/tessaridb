//! The one way serving code reaches the synchronous store (ADR-0085 §2).
//!
//! The store blocks — on the engine, on a commit's flush — and a blocked worker
//! thread is a runtime that has stopped serving everybody else. So every store
//! call is moved to the blocking pool, and the number of them running at once is
//! bounded here rather than by the pool, whose own bound queues instead of
//! refusing.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// What a bridged call came back with.
///
/// `S` is the state the work was given, handed back when it was refused: a
/// connection's session travels into the work and out again, and a refusal
/// that dropped it would lose the session along with the call.
#[derive(Debug, PartialEq, Eq)]
pub enum Bridged<T, S> {
    /// The work ran and this is what it returned, untouched.
    Answered(T),
    /// Every slot was taken, so the work was refused without running.
    ///
    /// Refused rather than queued: a caller told *busy* can answer its client
    /// now, while one left waiting holds a connection and adds to the load that
    /// made it wait.
    Busy(S),
    /// The work panicked. Its slot has been given back.
    Panicked,
}

/// A bound on how many store calls are in flight at once.
#[derive(Debug)]
pub struct Bridge {
    slots: Arc<Semaphore>,
    refused: AtomicU64,
}

impl Bridge {
    /// A bridge that lets `in_flight` calls run at once.
    ///
    /// # Panics
    ///
    /// When `in_flight` exceeds [`Semaphore::MAX_PERMITS`], which is far beyond
    /// any count of threads a process can hold.
    #[must_use]
    pub fn new(in_flight: usize) -> Self {
        Self {
            slots: Arc::new(Semaphore::new(in_flight)),
            refused: AtomicU64::new(0),
        }
    }

    /// Run `work` on the blocking pool if a slot is free, or refuse it.
    ///
    /// The slot travels with the work and is released when the work ends — on
    /// return and on panic alike. A caller that stops waiting does not cancel
    /// the work, which cannot be interrupted once it runs; it keeps its slot
    /// until it ends, because that is the load it really is. The work bounds its
    /// own duration: a store call runs under the statement's own limits.
    ///
    /// Must be awaited inside a Tokio runtime.
    pub async fn call<S, T, F>(&self, state: S, work: F) -> Bridged<T, S>
    where
        F: FnOnce(S) -> T + Send + 'static,
        S: Send + 'static,
        T: Send + 'static,
    {
        let Ok(slot) = Arc::clone(&self.slots).try_acquire_owned() else {
            self.refused.fetch_add(1, Ordering::Relaxed);
            return Bridged::Busy(state);
        };
        // The blocking pool runs the work on a thread of its own, which is in no
        // span; the caller's — the connection, the request — goes with it, so
        // what the work reports says whose work it was.
        let span = tracing::Span::current();
        let ran = tokio::task::spawn_blocking(move || {
            let _inside = span.enter();
            let answer = work(state);
            drop(slot);
            answer
        })
        .await;
        match ran {
            Ok(answer) => Bridged::Answered(answer),
            Err(_) => Bridged::Panicked,
        }
    }

    /// Take a slot for a store call the caller runs on its own thread, or `None`
    /// — counted as refused — when every slot is taken.
    ///
    /// For a thread that may already block: the call is bounded by the same
    /// slots and refused by the same rule as [`Bridge::call`], and pays no hop
    /// to reach the blocking pool because it is already there. The slot is given
    /// back when the permit is dropped, on return and on unwind alike.
    #[must_use]
    pub fn slot(&self) -> Option<OwnedSemaphorePermit> {
        let taken = Arc::clone(&self.slots).try_acquire_owned().ok();
        if taken.is_none() {
            self.refused.fetch_add(1, Ordering::Relaxed);
        }
        taken
    }

    /// How many calls have been refused as busy since the bridge was built.
    #[must_use]
    pub fn refused(&self) -> u64 {
        self.refused.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use std::sync::Arc;
    use std::time::Duration;

    use super::{Bridge, Bridged};

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("a runtime")
    }

    #[test]
    fn the_answer_comes_back_untouched() {
        let bridge = Bridge::new(1);
        let answer = runtime().block_on(bridge.call((), |()| Ok::<_, String>(41 + 1)));
        assert!(matches!(answer, Bridged::Answered(Ok(42))));
    }

    #[test]
    fn a_full_bridge_refuses_at_once_and_serves_again_when_a_slot_frees() {
        let bridge = Bridge::new(1);
        // Taken rather than raced for: the one slot is held by the test itself.
        let held = Arc::clone(&bridge.slots)
            .try_acquire_owned()
            .expect("the only slot");
        let runtime = runtime();
        let refused = runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(2), bridge.call(1_u8, |state| state)).await
        });
        let Ok(Bridged::Busy(state)) = refused else {
            panic!("queued, or answered past a full bridge");
        };
        assert_eq!(
            state, 1,
            "the refused call's state comes back to its caller whole"
        );
        assert_eq!(bridge.refused(), 1);
        drop(held);
        assert!(matches!(
            runtime.block_on(bridge.call((), |()| 2)),
            Bridged::Answered(2)
        ));
    }

    #[test]
    fn a_panic_is_reported_and_gives_its_slot_back() {
        let bridge = Bridge::new(1);
        let runtime = runtime();
        // `resume_unwind` unwinds exactly as a panic does, without the hook's noise.
        let panicked = runtime.block_on(bridge.call((), |()| -> u8 {
            std::panic::resume_unwind(Box::new("failed"))
        }));
        assert!(matches!(panicked, Bridged::Panicked));
        assert!(matches!(
            runtime.block_on(bridge.call((), |()| 3)),
            Bridged::Answered(3)
        ));
    }
}
