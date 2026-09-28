//! Where a panic stops, and what the node does next.
//!
//! # A defect ends the work that met it, not the node
//!
//! A panic is a defect, and the lint floor keeps the obvious ones out — no
//! `unwrap`, no `panic!`, no truncating cast. What is left is the defect nobody
//! foresaw, and the question is how much of the node it takes with it. The
//! answer this binary gives is: the unit of work that met it. A connection or
//! a request is its own thread, and a thread that unwinds releases what it held
//! — its place at the door, its count in the drain — while every other
//! conversation carries on. A cadence the node runs for itself (housekeeping,
//! the peer door, the three cluster rounds) is a loop with nothing above it to
//! notice it ended, so it runs under [`supervised`] and is started again.
//!
//! Both depend on the build unwinding. A build that aborts turns every one of
//! those boundaries into a process exit, which is why `main.rs` refuses to
//! compile as one.
//!
//! # Why restarting a cadence is safe
//!
//! Each cadence borrows the store and its own flag and keeps nothing of its own
//! between rounds that a fresh start would lose: what it learned lives in the
//! store or in the shared registries, and each of those is written in one
//! step. A write that was under way when the panic happened either reached the
//! engine as a whole batch or did not reach it.
//!
//! # Why the node still ends when a surface does
//!
//! The two listeners are not restarted. A listener that stopped is a node that
//! no longer answers, and a restart by whatever runs the process — with its
//! exit logged — is the recovery that also resets everything the listener held.
//! So a panic that reaches a listener's own loop ends the process through
//! [`or_the_node_ends`], rather than leaving the other surface serving alone.
//! That is the one place a panic is allowed to end the node.

use std::any::Any;
use std::panic::{self, AssertUnwindSafe};
use std::time::Duration;

use tessari_serve::Stopping;

/// How long a cadence that panicked waits before it is started again.
///
/// Long enough that a defect met on every round logs once a second rather than
/// spinning a core, short enough that a lease renewal survives it.
const RESTART_PAUSE: Duration = Duration::from_secs(1);

/// Run a cadence until it returns, starting it again whenever it panics.
///
/// Returns when `work` returns, or when it panicked after the node was asked
/// to stop — a cadence that is leaving anyway is not worth another round.
pub fn supervised(name: &str, stopping: &Stopping, work: impl FnMut()) {
    restarting(name, stopping, RESTART_PAUSE, work);
}

fn restarting(name: &str, stopping: &Stopping, pause: Duration, mut work: impl FnMut()) {
    loop {
        // `AssertUnwindSafe`, because what `work` touches after a panic is the
        // store and the registries, whose poison handling is decided per lock
        // (see the module header) rather than inferred from a marker trait.
        match panic::catch_unwind(AssertUnwindSafe(&mut work)) {
            Ok(()) => return,
            Err(payload) => {
                if stopping.asked() {
                    log::error!("{name} panicked while stopping ({})", described(&*payload));
                    return;
                }
                log::error!(
                    "{name} panicked ({}); starting it again in {} ms",
                    described(&*payload),
                    pause.as_millis()
                );
                std::thread::sleep(pause);
            }
        }
    }
}

/// Run a listener, and end the process if a panic reaches its loop.
///
/// Aborting rather than exiting: a stopped listener is a defect in the node
/// itself, and the store is crash-safe by design, so nothing is gained by
/// running teardown code on a process already known to be wrong.
pub fn or_the_node_ends(name: &str, work: impl FnOnce()) {
    if let Err(payload) = panic::catch_unwind(AssertUnwindSafe(work)) {
        log::error!(
            "the {name} listener panicked ({}); the node ends here",
            described(&*payload)
        );
        std::process::abort();
    }
}

/// Serve the wire surface on the runtime until `stop` is cancelled.
///
/// Ends the process on a panic, as every listener does, and on an accept
/// failure that does not pass on its own: a listener that has gone bad is the
/// same defect as one that panicked, and retrying it forever is how the old loop
/// spun a core while admitting nobody (Q-834).
pub fn wire(
    runtime: &tokio::runtime::Runtime,
    node: &tessari_wire::Node,
    stop: &tokio_util::sync::CancellationToken,
) {
    or_the_node_ends("wire", || {
        if let Err(why) = runtime.block_on(node.serve(stop.clone())) {
            log::error!("the wire listener failed ({why}); the node ends here");
            std::process::abort();
        }
    });
}

/// Send every panic to the log, with the thread and the place it came from.
///
/// The default hook writes to standard error in its own shape; a node that
/// logs everything else through `log` would then report its one defect in a
/// form no log reader parses.
pub fn log_panics() {
    panic::set_hook(Box::new(|info| {
        let thread = std::thread::current();
        let place = info.location().map_or_else(
            || "an unknown place".to_owned(),
            |at| format!("{}:{}", at.file(), at.line()),
        );
        log::error!(
            "thread '{}' panicked at {place}: {}",
            thread.name().unwrap_or("unnamed"),
            described(info.payload())
        );
    }));
}

/// A panic's message, when it carried one.
fn described(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("no message")
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    #[test]
    fn a_cadence_that_panics_is_started_again_until_it_returns() {
        let stopping = Stopping::new();
        let rounds = Cell::new(0_u32);
        restarting("test cadence", &stopping, Duration::ZERO, || {
            rounds.set(rounds.get().saturating_add(1));
            assert!(rounds.get() >= 3, "a defect met on round {}", rounds.get());
        });
        assert_eq!(rounds.get(), 3);
    }

    #[test]
    fn a_cadence_that_panics_while_stopping_is_not_started_again() {
        let stopping = Stopping::new();
        stopping.refuse_new();
        let rounds = Cell::new(0_u32);
        restarting("test cadence", &stopping, Duration::ZERO, || {
            rounds.set(rounds.get().saturating_add(1));
            assert!(rounds.get() > 5, "a defect met while leaving");
        });
        assert_eq!(rounds.get(), 1);
    }

    #[test]
    fn a_panic_message_is_read_from_either_payload_shape() {
        let literal: Box<dyn Any + Send> = Box::new("literal");
        let formatted: Box<dyn Any + Send> = Box::new(String::from("formatted"));
        let neither: Box<dyn Any + Send> = Box::new(7_u8);
        assert_eq!(described(&*literal), "literal");
        assert_eq!(described(&*formatted), "formatted");
        assert_eq!(described(&*neither), "no message");
    }
}
