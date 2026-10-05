//! The runner every cadence a node keeps goes through: one pass at a time on the
//! blocking pool, a budget for each pass, and three outcomes reported under the
//! cadence's name — completed, failed, overran.

use super::*;

/// Why one pass of a cadence could not do its work.
///
/// What the pass could not do travels with why, because the runner reports the
/// failure and not the pass: "the collection pass failed: this node cannot say
/// who it is: …" is one line an operator can act on, where the reason alone
/// would not say which question it answers.
#[derive(Debug, thiserror::Error)]
#[error("{doing}: {source}")]
pub struct PassFailed {
    /// What the pass could not do, said the way an operator reads it.
    doing: &'static str,
    /// Why.
    source: Box<dyn std::error::Error + Send + Sync>,
}

impl PassFailed {
    /// A pass that could not do `doing`, because of `source`.
    pub fn new(
        doing: &'static str,
        source: impl Into<Box<dyn std::error::Error + Send + Sync>>,
    ) -> Self {
        Self {
            doing,
            source: source.into(),
        }
    }
}

/// What the runner reports about a pass that did not simply finish in time.
///
/// Both name the cadence, so a line about a pass says which of the node's
/// timers it belongs to without the pass having to say so itself.
#[derive(Debug, thiserror::Error)]
pub enum CadenceError {
    /// The pass ran and could not do its work. The cadence goes on.
    #[error("the {cadence} pass failed: {failed}")]
    Failed {
        /// The cadence the pass belongs to.
        cadence: &'static str,
        /// What it could not do, and why.
        #[source]
        failed: PassFailed,
    },
    /// The pass is still running when its budget ran out. A pass on the
    /// blocking pool cannot be cancelled, so the runner waits for it — passes
    /// of one cadence never overlap — and says so, once, when it happens.
    #[error("the {cadence} pass overran its budget of {budget:?}")]
    Overran {
        /// The cadence the pass belongs to.
        cadence: &'static str,
        /// What the pass was given: the period it was meant to fit in.
        budget: Duration,
    },
}

/// How long to wait before the next pass, given when the last one started.
///
/// Both instants are the **runtime's** clock, the one the wait then sleeps on.
/// Measured on the wall clock instead, the arithmetic and the sleep are two
/// clocks: the same reading in production, and apart wherever the runtime's
/// clock is not the wall's — a test's paused clock, where the real time a pass
/// took was subtracted from a sleep that time does not pass in, and a period
/// came out a millisecond short whenever a pass took more than one (Q-939).
///
/// # A missed tick is not made up
///
/// When a pass overran its period — the peer was slow, the thread was
/// descheduled — this answers [`Duration::ZERO`] and the next pass runs at once.
/// It never answers *run it three times because three periods went by*.
///
/// Catching up would be wrong for each cadence separately. Three greeting rounds
/// back to back dial every peer three times to learn one thing. Three
/// collections are unnecessary because the cursor already carries the position,
/// so one pass fetches as much as the peer's limit allows regardless of how long
/// it has been. And three renewals are three elections where the cluster needed
/// none.
#[must_use]
pub fn due_in(
    period: Duration,
    ran_at: tokio::time::Instant,
    now: tokio::time::Instant,
) -> Duration {
    period.saturating_sub(now.saturating_duration_since(ran_at))
}

/// Run `pass` on `period` until the node is asked to stop.
///
/// The token is checked **before** each pass, so a node already stopping runs
/// none, and it ends the wait between passes the moment it is cancelled — a
/// cadence holds nothing a stop would have to wait for.
///
/// Each pass runs on the runtime's blocking pool, because a pass is store work
/// and a TLS call, both synchronous. The closure is moved there and back, so a
/// pass keeps what it learned between rounds, and one cadence's passes never
/// overlap: the campaign's ballots stay one after another. A pass that panics
/// is re-raised on this task, where the supervisor that started it sees it.
///
/// Each pass runs inside a `cadence{cadence=…}` span, has `period` as its
/// budget — one pass per period is the cadence's whole contract, so a pass that
/// outlasts it has made the next one late — and ends in one of three reported
/// outcomes: completed (at `debug`, because it is every period), failed, or
/// overran (both at `warn`). A failed pass does not end the cadence.
///
/// The token is the node's own stop rather than one of this module's. A driver
/// with a private flag gives a process two ways to ask a node to stop, and the
/// state between them — a node that has stopped serving while it goes on
/// dialling peers — is worse than either.
pub async fn every<P>(
    cadence: &'static str,
    period: Duration,
    stop: &CancellationToken,
    mut pass: P,
) where
    P: FnMut(Instant) -> Result<(), PassFailed> + Send + 'static,
{
    while !stop.is_cancelled() {
        let ran_at = Instant::now();
        let waited_from = tokio::time::Instant::now();
        let Some((returned, _)) = run_pass(cadence, period, ran_at, pass).await else {
            return;
        };
        pass = returned;
        tokio::select! {
            biased;
            () = stop.cancelled() => return,
            () = tokio::time::sleep(due_in(period, waited_from, tokio::time::Instant::now())) => {}
        }
    }
}

/// [`every`], where each pass names how long until the next one, and `wake`
/// starts the next one early.
///
/// For the two rounds whose right period depends on what the last pass found
/// (G053 SG2b): a node that can name no leader greets every round time rather
/// than every awareness interval, because that is when a stale directory costs
/// the most; and a follower whose stream ended collects again the moment the
/// greeting round has found where its leader went, rather than up to a period
/// later. A [`Notify`](tokio::sync::Notify) keeps one permit, so a wake that
/// arrives while a pass is running is not lost — the next wait returns at once.
///
/// The period the last pass chose is the next pass's budget, and it is also
/// what a FAILED pass waits: a failure names no period, and running again at
/// once would turn a node that cannot read its catalog into a loop that does
/// nothing else. `first` stands in for it until a pass has chosen one.
pub async fn every_paced<P>(
    cadence: &'static str,
    stop: &CancellationToken,
    wake: &tokio::sync::Notify,
    first: Duration,
    mut pass: P,
) where
    P: FnMut(Instant) -> Result<Duration, PassFailed> + Send + 'static,
{
    let mut period = first;
    while !stop.is_cancelled() {
        let ran_at = Instant::now();
        let waited_from = tokio::time::Instant::now();
        let Some((returned, chosen)) = run_pass(cadence, period, ran_at, pass).await else {
            return;
        };
        pass = returned;
        if let Some(chosen) = chosen {
            period = chosen;
        }
        tokio::select! {
            biased;
            () = stop.cancelled() => return,
            () = wake.notified() => {}
            () = tokio::time::sleep(due_in(period, waited_from, tokio::time::Instant::now())) => {}
        }
    }
}

/// Run one pass on the blocking pool inside its cadence's span, report its
/// outcome, and hand the closure back with what the pass answered — `None` in
/// place of a failure, which has been reported here.
///
/// `None` overall is the runtime shutting down under the pass; a panic in the
/// pass is re-raised on the calling task.
async fn run_pass<P, T>(
    cadence: &'static str,
    budget: Duration,
    ran_at: Instant,
    mut pass: P,
) -> Option<(P, Option<T>)>
where
    P: FnMut(Instant) -> Result<T, PassFailed> + Send + 'static,
    T: Send + 'static,
{
    let span = tracing::info_span!("cadence", cadence);
    let inside = span.clone();
    let mut running = tokio::task::spawn_blocking(move || {
        let ran = inside.in_scope(|| pass(ran_at));
        (pass, ran)
    });
    let joined = match tokio::time::timeout(budget, &mut running).await {
        Ok(joined) => joined,
        Err(_) => {
            let overran = CadenceError::Overran { cadence, budget };
            span.in_scope(|| {
                tracing::warn!(error = %overran, "pass overran; the next one waits for it");
            });
            running.await
        }
    };
    let (pass, ran) = match joined {
        Ok(ran) => ran,
        Err(ended) => match ended.try_into_panic() {
            Ok(payload) => std::panic::resume_unwind(payload),
            // Cancelled: the runtime is shutting down under it.
            Err(_) => return None,
        },
    };
    let elapsed_ms = u64::try_from(ran_at.elapsed().as_millis()).unwrap_or(u64::MAX);
    let answered = span.in_scope(|| match ran {
        Ok(answered) => {
            tracing::debug!(elapsed_ms, "pass completed");
            Some(answered)
        }
        Err(failed) => {
            let failed = CadenceError::Failed { cadence, failed };
            tracing::warn!(elapsed_ms, error = %failed, "pass failed");
            None
        }
    });
    Some((pass, answered))
}
