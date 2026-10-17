//! Driving an asynchronous entry point to its end from a synchronous test or harness.
//!
//! The store's entry points are asynchronous. A test that is not itself asynchronous drives one
//! through [`now`] (or [`Now::now`]), on one multi-thread runtime the process keeps for the
//! purpose; so do the synchronous harnesses. The library never creates a runtime for its own work;
//! this module exists only behind `test-support`.
//!
//! The runtime is shared and multi-threaded because the engine starts its background work on the
//! runtime a store is opened on. A runtime per thread runs that work only while its own thread is
//! inside `now`, so a store opened on one thread and written from others stops the moment the
//! writers wait on a flush the opener's runtime is not driving.

use std::future::Future;
use std::sync::OnceLock;

/// The runtime every synchronous test drives its futures on.
static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

/// Drives `future` to its end on the test runtime: any of the store's asynchronous entry points,
/// for a synchronous test. The future runs on the calling thread; what it spawns runs on the
/// runtime's workers.
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
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("a test runtime")
        })
        .block_on(future)
}

/// [`now`] as a method, for a synchronous test: `session.vault(target, act).now()`.
pub trait Now: Future + Sized {
    /// This future, driven to its end on the test runtime.
    ///
    /// # Panics
    ///
    /// As [`now`].
    fn now(self) -> Self::Output {
        now(self)
    }
}

impl<F: Future> Now for F {}
