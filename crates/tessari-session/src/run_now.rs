//! Running a script from a synchronous test or harness.
//!
//! [`Session::run`] is asynchronous. A test that is not itself asynchronous runs it through
//! [`RunNow`], which drives the future on a runtime this thread keeps for the purpose; so do the
//! synchronous harnesses, the benchmark and the conformance runner. The library never creates a
//! runtime for its own work; this module exists only behind `test-support`.

use std::cell::OnceCell;
use std::future::Future;

use tessari_ql::Parameters;

use crate::{Outcome, Result, Session};

thread_local! {
    /// The runtime this thread's synchronous tests drive their scripts on.
    static RUNTIME: OnceCell<tokio::runtime::Runtime> = const { OnceCell::new() };
}

/// Drives `future` to its end on this thread's test runtime: any of the session's asynchronous
/// entry points, for a synchronous test ([`RunNow`] is this for `run` and `run_with`).
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

/// [`Session::run`] and [`Session::run_with`] for a synchronous test.
pub trait RunNow {
    /// [`Session::run`], driven to its end on this thread.
    ///
    /// # Errors
    ///
    /// As [`Session::run`].
    fn run_now(&mut self, source: &str) -> Result<Vec<Outcome>>;

    /// [`Session::run_with`], driven to its end on this thread.
    ///
    /// # Errors
    ///
    /// As [`Session::run_with`].
    fn run_with_now(&mut self, source: &str, parameters: &Parameters) -> Result<Vec<Outcome>>;
}

impl RunNow for Session<'_> {
    fn run_now(&mut self, source: &str) -> Result<Vec<Outcome>> {
        now(self.run(source))
    }

    fn run_with_now(&mut self, source: &str, parameters: &Parameters) -> Result<Vec<Outcome>> {
        now(self.run_with(source, parameters))
    }
}
