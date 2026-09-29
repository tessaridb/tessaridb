//! The one async runtime a serving process owns (ADR-0085 §1).
//!
//! Built here and nowhere else: the libraries return futures and never start a
//! runtime of their own, and the one-shot modes — a statement, a file, a backup,
//! a health check — never build one at all.

use std::num::NonZeroUsize;
use std::time::Duration;

use tokio::runtime::{Builder, Runtime};

/// How many blocking threads the runtime may start: one per store call in
/// flight, one per busy wire connection served on its own thread, and one per
/// cadence pass.
///
/// The first half is the count ADR-0085 opens with — the connection bound the
/// synchronous node already held. The second is `tessari-wire`'s `hot` bound,
/// which is the same number: a busy connection holds its thread between
/// statements, so without room of its own it would take the threads the store
/// calls queue for. The cadences' room is theirs for the same reason: a lease
/// renewal that queued behind store calls would lose a leadership it could
/// have kept.
const BLOCKING: usize = tessari_constants::MAX_STORE_CALLS * 2 + CADENCES;

/// The cadences a node runs, each one pass at a time: housekeeping, and the
/// greeting, collection and leadership rounds.
const CADENCES: usize = 4;

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
