//! Running a script from a synchronous test or harness.
//!
//! [`Session::run`] is asynchronous. A test that is not itself asynchronous runs it through
//! [`RunNow`], which drives the future on a runtime this thread keeps for the purpose; so do the
//! synchronous harnesses, the benchmark and the conformance runner. The library never creates a
//! runtime for its own work; this module exists only behind `test-support`.

use tessari_ql::Parameters;

use tessari_storage::now;

use crate::{Outcome, Result, Session};

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

// Boxed: a statement's future is large, and handing it to the runtime by value
// copies it on the way in, which a harness timing a statement would count.
impl RunNow for Session<'_> {
    fn run_now(&mut self, source: &str) -> Result<Vec<Outcome>> {
        now(Box::pin(self.run(source)))
    }

    fn run_with_now(&mut self, source: &str, parameters: &Parameters) -> Result<Vec<Outcome>> {
        now(Box::pin(self.run_with(source, parameters)))
    }
}
