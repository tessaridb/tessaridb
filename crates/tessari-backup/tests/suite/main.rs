//! Every integration test of this crate, in one binary.
//!
//! Cargo builds one test binary per file directly under `tests/`, and each one
//! links the whole workspace again. A subdirectory carrying a `main.rs` is one
//! target instead, so the cases sit beside this file and the crate pays that
//! link once rather than once per case.

#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

mod bootstrap;
mod catch_up;
mod from_a_running_node;
mod gap;
mod produced_identity;
mod restore;
mod vault_restore;
mod vault_shredding;
mod version;

use tessari_backup::LogSpan;
use tessari_encoding::LogId;
use tessari_storage::Store;
use tessari_types::Sequence;

/// Every log the store holds, with the sequence each stands at.
///
/// A store holds a log per range now (S6.2), so a single `committed_tail()` is
/// an answer about one of them — which is why these helpers exist rather than
/// each case picking a home and hoping it picked the busy one.
pub(crate) fn tails(store: &Store) -> Vec<(LogId, Sequence)> {
    store
        .logs()
        .unwrap()
        .into_iter()
        .map(|log| (log, store.committed_tail(log).unwrap()))
        .collect()
}

/// How many records the store holds across every log.
///
/// What a whole backup's record count has to equal, and the number that used to
/// be `committed_tail()` back when there was one log to be the tail of.
pub(crate) fn records_held(store: &Store) -> u64 {
    tails(store)
        .into_iter()
        .map(|(_, tail)| tail.get())
        .sum::<u64>()
}

/// Assert a file's sections are exactly the store's logs, each to its own tail.
///
/// Stronger than the `written.tail == store.committed_tail()` this replaces: it
/// checks every log rather than one, and it catches a file that quietly left a
/// log out — the failure the sections exist to prevent.
pub(crate) fn covers(logs: &[LogSpan], store: &Store) {
    let held: Vec<(LogId, Sequence)> = logs.iter().map(|log| (log.log, log.tail)).collect();
    assert_eq!(
        held,
        tails(store),
        "the file's sections are not the store's logs"
    );
}

/// The one section a single-log file holds.
pub(crate) fn only(logs: &[LogSpan]) -> LogSpan {
    match logs {
        [one] => *one,
        many => panic!("expected one section, found {}", many.len()),
    }
}
