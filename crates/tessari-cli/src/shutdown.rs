//! Stopping a serving process in the order the stages have to happen.
//!
//! The stages themselves are ADR-0015's and the counting is `tessari-serve`'s.
//! What lives here is the **sequencing**, because it belongs to whatever holds
//! every surface, and that is this binary.
//!
//! # How a stop arrives
//!
//! A task on the serving runtime waits for `SIGTERM` or `SIGINT` and cancels one
//! [`CancellationToken`] on the first. Everything that stops waits on that token
//! rather than on a flag of its own, so there is one answer to *has a stop been
//! asked for* and nothing to keep in agreement with it (ADR-0085 §3).
//!
//! The second signal ends the process from that same task, because that is the
//! case where waiting is exactly what the operator is trying to stop.

use std::sync::Arc;
use std::time::Duration;

use tessari_serve::{Drained, Stopping};
use tokio::runtime::Runtime;
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;

/// How long in-flight requests are given to finish.
///
/// Long enough that an ordinary request completes rather than being cut off for
/// the sake of a fast deployment, short enough that a supervisor's own patience
/// — commonly thirty seconds before it escalates — is not spent here.
const PATIENCE: Duration = Duration::from_secs(20);

/// How long the node says *not ready* before it stops accepting.
///
/// This window is the entire reason a readiness route can be reached when its
/// answer matters. Refusing connections and being unwilling to serve are two
/// states here (`Stopping::leaving` before `Stopping::refuse_new`); if they were
/// one, the port would close at the instant the answer changed and a load
/// balancer would meet a refused connection where it should have read a `503`
/// and routed elsewhere.
///
/// **Five seconds, and the number is not derivable.** It has to outlast one
/// probe interval of whatever is watching, and the common defaults disagree —
/// two seconds for one proxy, ten for an orchestrator's readiness probe, thirty
/// for a cloud load balancer. Five is the shortest value a common supervisor can
/// observe at all. A constant that cannot be right for every supervisor is
/// configuration eventually, and this store's configuration is statements
/// (ADR-0003), so it belongs in `DEFINE NODE` rather than in a flag.
///
/// The cost is real and is paid by every shutdown. A second signal skips it.
const LAME_DUCK: Duration = Duration::from_secs(5);

/// How often a waiting stage looks again.
///
/// Slept on the runtime's timer rather than spun: most of a stop is waiting.
const GLANCE: Duration = Duration::from_millis(100);

/// Ask to be told when the operating system wants this process to stop.
///
/// `SIGTERM` is what a supervisor sends and `SIGINT` is what a terminal sends,
/// and a database should treat them the same: both mean *stop*, and only the
/// sender differs. Called before anything serves, so a signal arriving during
/// startup is counted rather than killing the process where it stands.
pub fn listen(runtime: &Runtime) -> std::io::Result<CancellationToken> {
    // Registered here rather than inside the task, so a refusal to register is
    // an error at startup and not a node that silently ignores its supervisor.
    let (mut terminate, mut interrupt) = {
        let _inside = runtime.enter();
        (
            signal(SignalKind::terminate())?,
            signal(SignalKind::interrupt())?,
        )
    };
    let asked = CancellationToken::new();
    let first = asked.clone();
    runtime.spawn(async move {
        tokio::select! {
            _ = terminate.recv() => {}
            _ = interrupt.recv() => {}
        }
        first.cancel();
        tokio::select! {
            _ = terminate.recv() => {}
            _ = interrupt.recv() => {}
        }
        leave_now();
    });
    Ok(asked)
}

/// End the process at once, as the second signal asks.
///
/// `_exit` rather than `std::process::exit`, because `exit` runs the C++ static
/// destructors the storage engine registered while its own background threads
/// are still using what they destroy — which turns *leave now* into a crash.
fn leave_now() -> ! {
    // SAFETY: `_exit` takes no pointers, touches no Rust state and cannot return;
    // skipping every destructor is the purpose.
    unsafe { libc::_exit(1) }
}

/// Wait for a stop to be asked for, then run the stages in order.
///
/// Returns when the surfaces have been told to stop and their work has drained,
/// leaving the caller to close the store — which is stage 4 and is the caller's
/// because the caller is what owns it.
///
/// `quiet` stops whatever writes in the background — the declared stream
/// consumers — at stage 1, beside the listeners. It is a closure rather than a
/// surface because the stages' vocabulary does not fit: nothing accepts a
/// consumer, nothing counts one in flight, and there is no listener to wake.
///
/// # The order is the substance
///
/// The node says **not ready** before it refuses anything, so whatever routes
/// traffic here can stop doing so while the node is still able to serve — a
/// refused connection is not an answer a load balancer can act on. New work is
/// then refused **before** existing work is interrupted, so a client mid-request
/// is not punished for a deployment. Subscriptions come **after** the drain
/// because they never end on their own, and waiting for one in stage 2 would
/// mean the drain never completes.
///
/// Awaited on the runtime beside the listeners it stops; every wait in it is a
/// timer, so it holds no thread while it waits.
pub async fn watch(
    asked: &CancellationToken,
    surfaces: &[Surface],
    quiet: &(dyn Fn() + Send + Sync),
) {
    asked.cancelled().await;
    eprintln!("tessaridb — stopping; a second signal exits immediately");

    // Stage 0. Say *not ready* and keep serving, so whatever is routing traffic
    // here learns it before the port goes rather than by a refused connection.
    // Nothing is refused yet — that is the point.
    for surface in surfaces {
        surface.stopping.leaving();
    }
    eprintln!(
        "tessaridb — not ready; still serving for {}s so a load balancer can notice",
        LAME_DUCK.as_secs()
    );
    // No check for a second signal here: the listener exits the process itself
    // on the second one, so an operator who does not want to wait out this
    // window is already gone before this loop could look.
    tokio::time::sleep(LAME_DUCK).await;

    // Stage 1. The intent is set on every surface first, then each is woken —
    // in that order, or a listener can find nothing set and block again.
    for surface in surfaces {
        surface.stopping.refuse_new();
    }
    for surface in surfaces {
        (surface.wake)();
    }
    // And the background writers stop taking new work at the same moment, for
    // the same reason. They are not a `Surface`: nothing accepts them, nothing
    // counts them in flight, and no wake reaches them — what they share with the
    // listeners is only this instant. A consumer left running past here keeps
    // writing while the node has been telling its load balancer it is not ready,
    // and the drain below would be waiting on requests that have nothing to do
    // with it.
    //
    // This does not wait. Each consumer finishes the batch it is in and returns;
    // the join belongs to whoever owns the handle, and happens before the store
    // is dropped.
    quiet();

    // Stage 2. Requests only. Feeds are stage 3 and were moved off this count
    // when they became feeds, which is what lets this finish at all.
    for surface in surfaces {
        match surface.stopping.drain(PATIENCE).await {
            Drained::Finished => {}
            Drained::Deadline { left } => {
                eprintln!(
                    "tessaridb — {} still had {left} request(s) running after {}s",
                    surface.name,
                    PATIENCE.as_secs()
                );
            }
        }
    }

    // Stage 3. A feed notices the same flag stage 1 set, within the interval it
    // already wakes on. Nothing is lost: a subscriber's cursor is a position it
    // holds, so it resumes exactly where it stopped.
    for surface in surfaces {
        let began = tokio::time::Instant::now();
        while surface.stopping.feeds() > 0 && began.elapsed() < PATIENCE {
            tokio::time::sleep(GLANCE).await;
        }
    }
}

/// One serving surface, as the stages see it.
pub struct Surface {
    /// What it is called in a message to an operator.
    pub name: &'static str,
    /// What it counts as in flight.
    pub stopping: Arc<Stopping>,
    /// How its accept loop is woken so it can notice the flag.
    ///
    /// A closure because the two surfaces are woken differently and neither way
    /// generalises: an HTTP server here has a call for it, and a plain
    /// `TcpListener` is woken by connecting to it.
    ///
    /// `Sync` as well as `Send` because the watcher reads this from one task
    /// while the surfaces are serving on others — the list is shared, not moved.
    pub wake: Box<dyn Fn() + Send + Sync>,
}
