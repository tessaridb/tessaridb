//! The one async runtime a serving process owns (ADR-0085 §1).
//!
//! Built here and nowhere else: the libraries return futures and never start a
//! runtime of their own, and the one-shot modes — a statement, a file, a backup,
//! a health check — never build one at all.

use std::num::NonZeroUsize;
use std::time::Duration;

use tokio::runtime::{Builder, Runtime};

/// How many store calls may block at once, which is how many blocking threads
/// the runtime may start.
///
/// The count ADR-0085 opens with: the connection bound the synchronous node
/// already held, so moving to the runtime changes how the store is waited on
/// without changing how many callers may wait on it.
const BLOCKING: usize = tessari_constants::MAX_CONNECTIONS;

/// How long the runtime's own tasks are given to end when the node stops.
///
/// Its tasks do no store work of their own at this point, so this is a
/// courtesy to the signal listener rather than a drain.
pub const LEAVING: Duration = Duration::from_secs(1);

/// Build the serving runtime: one worker per core the process may use.
pub fn build() -> std::io::Result<Runtime> {
    let workers = std::thread::available_parallelism().map_or(1, NonZeroUsize::get);
    Builder::new_multi_thread()
        .worker_threads(workers)
        .max_blocking_threads(BLOCKING)
        .thread_name("tessaridb-runtime")
        .enable_all()
        .build()
}
