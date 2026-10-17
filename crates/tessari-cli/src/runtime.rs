//! The one async runtime a serving process owns (ADR-0085 §1).
//!
//! Built here and nowhere else: the libraries return futures and never start a
//! runtime of their own, and the one-shot modes (a statement, a file, a backup,
//! a health check) drive their store calls on one thread instead.

use std::num::NonZeroUsize;
use std::time::Duration;

use tokio::runtime::{Builder, Runtime};

/// How many blocking threads the runtime may start: one per store call in
/// flight, and one per cadence pass.
///
/// Store calls run as tasks; the room is for the ones that hand their worker
/// over while they block (a password hash, a backup file), which take a
/// thread each for that long. The cadences' room is theirs: a lease renewal
/// that queued behind store calls would lose a leadership it could have kept.
const BLOCKING: usize = tessari_constants::MAX_STORE_CALLS + CADENCES;

/// The cadences a node runs, each one pass at a time: housekeeping, and the
/// greeting, collection and leadership rounds.
const CADENCES: usize = 4;

/// The stack of every runtime thread, workers and blocking threads alike.
///
/// A statement is one future nested as deep as the statement is: a debug
/// build's frames for the vault surface overflowed the 2 MiB default. Stated
/// here so the node does not depend on `RUST_MIN_STACK` in its environment.
const STACK: usize = 8 * 1024 * 1024;

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
        .thread_stack_size(STACK)
        .enable_all()
        .build()
}
