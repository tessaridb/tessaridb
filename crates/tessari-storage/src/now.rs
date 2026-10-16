//! Driving an asynchronous entry point to its end from a synchronous test or harness.
//!
//! The store's entry points are asynchronous. A test that is not itself asynchronous drives one
//! through [`now`] (or [`Now::now`]), on a runtime this thread keeps for the purpose; so do the
//! synchronous harnesses. The library never creates a runtime for its own work; this module exists
//! only behind `test-support`.

use std::cell::OnceCell;
use std::future::Future;

thread_local! {
    /// The runtime this thread's synchronous tests drive their scripts on.
    static RUNTIME: OnceCell<tokio::runtime::Runtime> = const { OnceCell::new() };
}

/// Drives `future` to its end on this thread's test runtime: any of the store's asynchronous
/// entry points, for a synchronous test.
///
/// # Panics
///
/// Inside an asynchronous test, which must await the future instead, and when the runtime
/// cannot be built.
#[expect(
    clippy::expect_used,
    reason = "test support: a runtime that cannot be built ends the test"
)]
pub fn now<F: Future>(future: F) -> F::Output {
    assert!(
        tokio::runtime::Handle::try_current().is_err(),
        "an asynchronous test awaits the future instead of calling `run_now` or `now`"
    );
    RUNTIME.with(|cell| {
        cell.get_or_init(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a test runtime")
        })
        .block_on(future)
    })
}

/// [`now`] as a method, for a synchronous test: `session.vault(target, act).now()`.
pub trait Now: Future + Sized {
    /// This future, driven to its end on this thread's test runtime.
    ///
    /// # Panics
    ///
    /// As [`now`].
    fn now(self) -> Self::Output {
        now(self)
    }
}

impl<F: Future> Now for F {}
