//! Where a panic stops, and what the node does next.
//!
//! # A defect ends the work that met it, not the node
//!
//! A panic is a defect, and the lint floor keeps the obvious ones out — no
//! `unwrap`, no `panic!`, no truncating cast. What is left is the defect nobody
//! foresaw, and the question is how much of the node it takes with it. The
//! answer this binary gives is: the unit of work that met it. A connection or
//! a request is its own task, and a task that unwinds releases what it held
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
//! Each cadence holds the store and the stop token and keeps nothing of its own
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
//! [`listener`], rather than leaving the other surface serving alone.
//! That is the one place a panic is allowed to end the node.

use std::any::Any;
use std::future::Future;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

/// How long a cadence that panicked waits before it is started again.
///
/// Long enough that a defect met on every round logs once a second rather than
/// spinning a core, short enough that a lease renewal survives it.
const RESTART_PAUSE: Duration = Duration::from_secs(1);

/// Run a cadence until it returns, starting it again whenever it panics.
///
/// Each start is a task of its own, so a panic ends that task and is seen here
/// as its outcome. Returns when `work` returns, or when it panicked after the
/// node was asked to stop — a cadence that is leaving anyway is not worth
/// another round.
pub async fn supervised<F, W>(name: &'static str, stop: CancellationToken, work: W)
where
    W: FnMut() -> F,
    F: Future<Output = ()> + Send + 'static,
{
    restarting(name, &stop, RESTART_PAUSE, work).await;
}

async fn restarting<F, W>(name: &str, stop: &CancellationToken, pause: Duration, mut work: W)
where
    W: FnMut() -> F,
    F: Future<Output = ()> + Send + 'static,
{
    loop {
        let Err(ended) = tokio::spawn(work()).await else {
            return;
        };
        // Cancelled rather than panicked: the runtime is shutting down under it.
        let Ok(payload) = ended.try_into_panic() else {
            return;
        };
        if stop.is_cancelled() {
            log::error!("{name} panicked while stopping ({})", described(&*payload));
            return;
        }
        log::error!(
            "{name} panicked ({}); starting it again in {} ms",
            described(&*payload),
            pause.as_millis()
        );
        tokio::select! {
            biased;
            () = stop.cancelled() => return,
            () = tokio::time::sleep(pause) => {}
        }
    }
}

/// Serve a listener until it returns, and end the process if it fails or a
/// panic reaches its loop.
///
/// Aborting rather than exiting: a stopped listener is a defect in the node
/// itself, and the store is crash-safe by design, so nothing is gained by
/// running teardown code on a process already known to be wrong. An accept
/// failure that does not pass on its own is the same defect as a panic, and
/// retrying it forever is how the old loop spun a core while admitting nobody
/// (Q-834).
pub async fn listener<E>(
    name: &'static str,
    serving: impl Future<Output = Result<(), E>> + Send + 'static,
) where
    E: std::fmt::Display + Send + 'static,
{
    match tokio::spawn(serving).await {
        Ok(Ok(())) => {}
        Ok(Err(why)) => {
            log::error!("the {name} listener failed ({why}); the node ends here");
            std::process::abort();
        }
        Err(ended) => {
            match ended.try_into_panic() {
                Ok(payload) => log::error!(
                    "the {name} listener panicked ({}); the node ends here",
                    described(&*payload)
                ),
                Err(ended) => {
                    log::error!("the {name} listener ended ({ended}); the node ends here")
                }
            }
            std::process::abort();
        }
    }
}

/// Send every panic to the log, with the thread and the place it came from.
///
/// The default hook writes to standard error in its own shape; a node that
/// logs everything else through `log` would then report its one defect in a
/// form no log reader parses.
pub fn log_panics() {
    std::panic::set_hook(Box::new(|info| {
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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    /// A cadence whose first `until` starts panic, counting every start.
    fn cadence(
        rounds: &Arc<AtomicU32>,
        until: u32,
    ) -> impl FnMut() -> std::pin::Pin<Box<dyn Future<Output = ()> + Send>> + use<> {
        let rounds = Arc::clone(rounds);
        move || {
            let rounds = Arc::clone(&rounds);
            Box::pin(async move {
                let round = rounds.fetch_add(1, Ordering::Relaxed).saturating_add(1);
                assert!(round > until, "a defect met on round {round}");
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_cadence_that_panics_is_started_again_until_it_returns() {
        let stop = CancellationToken::new();
        let rounds = Arc::new(AtomicU32::new(0));
        restarting("test cadence", &stop, RESTART_PAUSE, cadence(&rounds, 2)).await;
        assert_eq!(rounds.load(Ordering::Relaxed), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn a_cadence_that_panics_while_stopping_is_not_started_again() {
        let stop = CancellationToken::new();
        stop.cancel();
        let rounds = Arc::new(AtomicU32::new(0));
        restarting("test cadence", &stop, RESTART_PAUSE, cadence(&rounds, 5)).await;
        assert_eq!(rounds.load(Ordering::Relaxed), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_during_the_restart_pause_ends_the_supervision() {
        let stop = CancellationToken::new();
        let rounds = Arc::new(AtomicU32::new(0));
        let stopping = stop.clone();
        let waiting = Arc::clone(&rounds);
        let supervising = tokio::spawn(async move {
            restarting(
                "test cadence",
                &stopping,
                Duration::from_secs(3600),
                cadence(&waiting, u32::MAX),
            )
            .await;
        });
        while rounds.load(Ordering::Relaxed) < 1 {
            tokio::task::yield_now().await;
        }
        stop.cancel();
        supervising.await.expect("the supervision task");
        assert_eq!(rounds.load(Ordering::Relaxed), 1);
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
