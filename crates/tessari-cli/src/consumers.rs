//! Starting the declared stream consumers, and stopping them at the right stage.
//!
//! # Why this is not simply a value with a `Drop`
//!
//! The runner's handle does stop and join its threads when it is dropped, and
//! that is the right behaviour for a library. It is **not** a graceful shutdown
//! for a serving node, for two reasons that only show up here.
//!
//! **A drop happens at the wrong moment.** `serve` drops its locals after the
//! serve loop has returned — which is after the lame-duck window, after new
//! connections were refused, and after in-flight requests were drained. A
//! consumer left running through all of that keeps writing while the node has
//! been telling its load balancer it is not ready, and the drain waits for
//! requests that have nothing to do with it. So the stop is wired into the
//! **stage** that refuses new work, beside the listeners.
//!
//! **A drop happens after the store is closed.** `serve` ends with an explicit
//! `drop(db)`, and locals declared before it outlive it. Consumers must be
//! joined *before* that, or the store is flushed and its lock released while
//! threads are still writing through it.
//!
//! # What a second signal does, and why that is fine
//!
//! The second signal calls `_exit`, which runs no destructor and joins nothing.
//! A consumer killed mid-batch has either not written yet, or has written and
//! not committed its offset — and the second case is a redelivery, which
//! converges because every record's identity comes from its message. That is
//! precisely what at-least-once delivery buys, and it is why an immediate exit
//! is safe here rather than merely tolerated.
//!
//! # Two builds
//!
//! The broker client compiles librdkafka and needs a C toolchain, so it is
//! behind the `kafka` feature (ADR-0024 §2). Both shapes below present the same
//! three calls, so `serve` reads the same either way.

use std::sync::Arc;

use tessaridb::Db;

#[cfg(feature = "kafka")]
pub use running::Running;

#[cfg(feature = "kafka")]
mod running {
    /// The consumers this process started.
    pub type Running = Option<tessari_ingest::Started>;
}

#[cfg(not(feature = "kafka"))]
/// Nothing, on a build that carries no broker client.
pub type Running = Option<std::convert::Infallible>;

/// Start every consumer the catalog declares, if this build can run them.
///
/// Failing to start one is **not** a failure to start the node: an unreachable
/// broker must not keep a database from answering the queries that have nothing
/// to do with it.
#[cfg(feature = "kafka")]
pub fn start(db: &Arc<Db>) -> Running {
    let broker: Arc<dyn tessari_ingest::Broker> = Arc::new(tessari_ingest::Kafka);
    match tessari_ingest::Runner::start(db.store(), &broker) {
        Ok(started) => {
            if started.threads() > 0 {
                eprintln!(
                    "tessaridb — {} stream consumer(s) running",
                    started.threads()
                );
            }
            Some(started)
        }
        Err(failure) => {
            eprintln!("tessaridb — the declared consumers could not be started: {failure}");
            None
        }
    }
}

/// A build with no broker client runs nothing, and says so once.
///
/// Printed rather than silent, because "the consumer is declared and no records
/// are arriving" is otherwise a mystery whose answer is which binary is running.
#[cfg(not(feature = "kafka"))]
pub fn start(db: &Arc<Db>) -> Running {
    let mut session = tessari_session::Session::new(db.store());
    let declared = session
        .run("INFO FOR KAFKA CONSUMERS;")
        .is_ok_and(|answered| format!("{answered:?}").contains("name"));
    if declared {
        eprintln!(
            "tessaridb — this build carries no broker client, so the declared \
             consumers are not running (build with `--features kafka`)"
        );
    }
    None
}

/// The call that tells every consumer to stop taking new messages.
///
/// Run at the stage that refuses new connections, so ingestion stops when the
/// node stops accepting rather than when the process finally unwinds. It does
/// not wait: each consumer finishes the batch it is in — writing it and
/// committing its offset — which is what [`stop`] then joins.
pub fn halting(running: &Running) -> Box<dyn Fn() + Send + Sync> {
    #[cfg(feature = "kafka")]
    if let Some(started) = running {
        let flag = started.halting();
        return Box::new(move || flag.store(true, std::sync::atomic::Ordering::Relaxed));
    }
    let _ = running;
    Box::new(|| {})
}

/// Stop every consumer and wait for the batch each one is in.
///
/// Called **before** the store is dropped. Joining afterwards would flush the
/// store and release its lock while threads were still writing through it.
pub fn stop(running: Running) {
    #[cfg(feature = "kafka")]
    if let Some(mut started) = running {
        let waiting = started.threads();
        if waiting > 0 {
            eprintln!("tessaridb — waiting for {waiting} stream consumer(s) to finish their batch");
        }
        started.stop();
        return;
    }
    // On a build with no client there is nothing to join: `Running` is
    // uninhabited, so this arm exists only to consume the argument.
    let _ = running;
}
