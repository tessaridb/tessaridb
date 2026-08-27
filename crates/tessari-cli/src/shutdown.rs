//! Stopping a serving process in the order the stages have to happen.
//!
//! The stages themselves are ADR-0015's and the counting is `tessari-serve`'s.
//! What lives here is the **sequencing**, because it belongs to whatever holds
//! every surface, and that is this binary.
//!
//! # Why there is `unsafe` here
//!
//! Installing a signal handler is not expressible in safe Rust. `std` has no
//! signal API at all, so the choice was between one audited call into `libc` and
//! a dependency whose entire purpose is to wrap that same call. The dependency
//! lost: it is a larger surface, it is not more correct, and this crate's
//! neighbours took no dependency they could spell either.
//!
//! What the handler does is the part that has to be right, because almost
//! nothing is legal inside one. It **increments an atomic and returns**, or on
//! the second signal calls `_exit`, which is async-signal-safe. It allocates
//! nothing, locks nothing, and formats nothing. Everything that reads that
//! counter runs on an ordinary thread.
//!
//! It is the first `unsafe` in code that **ships**. The bench crate's counting
//! allocator has carried one for longer, but that is an instrument rather than
//! a surface a user reaches.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tessari_serve::{Drained, Stopping};

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

/// How often the watcher looks at the counter.
///
/// Parked rather than spun: a process spends its whole life waiting here.
const GLANCE: Duration = Duration::from_millis(100);

/// How many stop signals have arrived.
///
/// A counter rather than a flag, because the second one means something
/// different from the first and the handler must be able to tell without
/// reading anything else.
static ASKED: AtomicUsize = AtomicUsize::new(0);

/// What a signal handler is allowed to do.
///
/// The second signal exits **here**, inside the handler, because that is the
/// case where waiting is exactly what the operator is trying to stop. `_exit`
/// is async-signal-safe; anything that flushes or unwinds is not.
extern "C" fn asked(_signal: libc::c_int) {
    if ASKED.fetch_add(1, Ordering::AcqRel) >= 1 {
        // SAFETY: `_exit` is async-signal-safe by specification and is the one
        // way out of a handler that is guaranteed not to touch the allocator or
        // any lock this process might already hold.
        unsafe { libc::_exit(1) }
    }
}

/// Ask to be told when the operating system wants this process to stop.
///
/// `SIGTERM` is what a supervisor sends and `SIGINT` is what a terminal sends,
/// and a database should treat them the same: both mean *stop*, and only the
/// sender differs.
pub fn listen() {
    // SAFETY: `signal` is called once, before any surface is serving, with a
    // handler that only increments an atomic or calls `_exit`. The cast is the
    // signature this interface requires; there is no safe spelling of it.
    #[allow(clippy::as_conversions)]
    unsafe {
        libc::signal(libc::SIGTERM, asked as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, asked as *const () as libc::sighandler_t);
    }
}

/// Whether a stop has been asked for.
fn wanted() -> bool {
    ASKED.load(Ordering::Acquire) > 0
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
pub fn watch(surfaces: &[Surface], quiet: &(dyn Fn() + Send + Sync)) {
    while !wanted() {
        std::thread::park_timeout(GLANCE);
    }
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
    // No check for a second signal here: the handler exits the process itself on
    // the second one, so an operator who does not want to wait out this window
    // is already gone before this loop could look.
    let began = std::time::Instant::now();
    while began.elapsed() < LAME_DUCK {
        std::thread::park_timeout(GLANCE);
    }

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
        match surface.stopping.drain(PATIENCE) {
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
        let began = std::time::Instant::now();
        while surface.stopping.feeds() > 0 && began.elapsed() < PATIENCE {
            std::thread::park_timeout(GLANCE);
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
    /// `Sync` as well as `Send` because the watcher reads this from a thread
    /// while the surfaces are serving on others — the list is shared, not moved.
    pub wake: Box<dyn Fn() + Send + Sync>,
}
