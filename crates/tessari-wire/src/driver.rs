//! The three cadences a node runs, and why each gets a task of its own.
//!
//! # Three cadences, three tasks
//!
//! A node that has joined a cluster has three things to do on a timer: greet its
//! peers so it knows how current each one is, collect records from whoever it
//! follows, and renew the lease its leadership rests on. They look alike enough
//! to fold into one loop, and folding them is the mistake.
//!
//! They fail differently. A missed greeting costs the *freshness of a reading* —
//! routing gets more conservative, which is the direction it should fail in. A
//! missed collection costs *data*, and the node simply falls further behind. A
//! missed renewal costs *leadership*, and the fence closes whether or not anyone
//! noticed.
//!
//! One loop gives all three a single period, a single failure path, and a single
//! task's fate. The sharpest consequence is the last: `collect` and `renew`
//! both dial peers, so a collection blocked on a dead peer's TCP connect would
//! hold up a renewal whose fence is closing. The cadence with the tightest
//! deadline would be delayed by the one with the loosest, for no reason beyond
//! their sharing a loop.
//!
//! # Nothing here hands a `Result` to a timer
//!
//! Each cadence answers *what it did* — a cursor, a lease — rather than whether
//! it went well. The caller is a loop that runs again either way, so an error
//! return would only ever be dropped, and a dropped error reads at the call site
//! as if failure were impossible. What each driver owns instead is the rule for
//! **what a failed pass does to the state it holds**, which is the part that is
//! genuinely easy to get wrong.
//!
//! # The pass is a parameter
//!
//! Every driver takes the work as a closure, the way [`crate::Directory`] takes
//! its clock and its greeting. A driver that dialled a socket itself could only
//! be tested by standing up peers, and one that read the clock itself could only
//! have its timing rule tested by waiting.

mod collecting;
mod leadership;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tessari_encoding::{NODE_ID_LEN, NodeVersion, Roles};
use tessari_storage::ReplicaDefinition;
use tessari_types::{Reach, Sequence};
use tokio_util::sync::CancellationToken;

use crate::directory::{Destination, Directory};
use crate::joining::Seed;
pub use leadership::{
    Renewing, campaign_line, campaigns_for, election_timeout, heard_a_leader, heard_a_leader_on,
    heard_a_newer_policy, leader_of_range, preferred_to_yield_to, released, stands, stands_for,
    stands_for_the_store, voters,
};

/// How long to wait before the next pass, given when the last one started.
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
pub fn due_in(period: Duration, ran_at: Instant, now: Instant) -> Duration {
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
/// The token is the node's own stop rather than one of this module's. A driver
/// with a private flag gives a process two ways to ask a node to stop, and the
/// state between them — a node that has stopped serving while it goes on
/// dialling peers — is worse than either.
pub async fn every<P>(period: Duration, stop: &CancellationToken, mut pass: P)
where
    P: FnMut(Instant) + Send + 'static,
{
    while !stop.is_cancelled() {
        let ran_at = Instant::now();
        pass = match tokio::task::spawn_blocking(move || {
            pass(ran_at);
            pass
        })
        .await
        {
            Ok(pass) => pass,
            Err(ended) => match ended.try_into_panic() {
                Ok(payload) => std::panic::resume_unwind(payload),
                // Cancelled: the runtime is shutting down under it.
                Err(_) => return,
            },
        };
        tokio::select! {
            biased;
            () = stop.cancelled() => return,
            () = tokio::time::sleep(due_in(period, ran_at, Instant::now())) => {}
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
pub async fn every_paced<P>(stop: &CancellationToken, wake: &tokio::sync::Notify, mut pass: P)
where
    P: FnMut(Instant) -> Duration + Send + 'static,
{
    while !stop.is_cancelled() {
        let ran_at = Instant::now();
        let (returned, period) = match tokio::task::spawn_blocking(move || {
            let period = pass(ran_at);
            (pass, period)
        })
        .await
        {
            Ok(ran) => ran,
            Err(ended) => match ended.try_into_panic() {
                Ok(payload) => std::panic::resume_unwind(payload),
                // Cancelled: the runtime is shutting down under it.
                Err(_) => return,
            },
        };
        pass = returned;
        tokio::select! {
            biased;
            () = stop.cancelled() => return,
            () = wake.notified() => {}
            () = tokio::time::sleep(due_in(period, ran_at, Instant::now())) => {}
        }
    }
}

/// The directory the routing side reads, and the greeting side replaces.
///
/// # Why a copy and a swap rather than a lock held across the round
///
/// [`Directory::greet_round`] takes `&mut self` and dials each peer *inside* the
/// walk, so a shared `Mutex<Directory>` would hold the lock for the length of
/// every connection attempt. Every routing read would then wait on the slowest
/// unreachable peer in the cluster — which is exactly the node the directory
/// exists to route around, so the structure would turn one node's failure into
/// every reader's latency.
///
/// Instead the greeting side takes a copy, dials into the copy with no lock
/// held, and swaps the result in under a lock held for the swap alone. Readers
/// see the previous round's answers until the new ones are all in, which is a
/// consistent view rather than a partial one.
///
/// The copy is taken from the *current* directory and not from an empty one, so
/// a peer that was heard two rounds ago and has been silent since is carried
/// forward and goes on ageing. That is what makes W234's rule survive the swap:
/// a silent peer grows old, and starting each round from nothing would instead
/// make every silent peer vanish once per period.
#[derive(Debug)]
pub struct Published {
    current: Mutex<Arc<Directory>>,
}

impl Published {
    /// Publish `directory` as the current answer.
    #[must_use]
    pub fn holding(directory: Directory) -> Self {
        Self {
            current: Mutex::new(Arc::new(directory)),
        }
    }

    /// The directory as it stands.
    ///
    /// The lock is held only long enough to clone a pointer, so a reader never
    /// waits on a greeting round.
    #[must_use]
    pub fn current(&self) -> Arc<Directory> {
        Arc::clone(&self.held())
    }

    /// Run one greeting round against a copy, then publish it.
    pub fn round(&self, greet: impl FnOnce(&mut Directory)) {
        let mut next = (*self.current()).clone();
        greet(&mut next);
        *self.held() = Arc::new(next);
    }

    /// The guard, recovering rather than panicking if a holder died mid-swap.
    ///
    /// What the lock protects is one pointer. A thread that panicked while
    /// holding it left a whole directory behind, never half of one, so the value
    /// is sound and refusing to read it would take routing down over an
    /// unrelated failure elsewhere.
    fn held(&self) -> std::sync::MutexGuard<'_, Arc<Directory>> {
        self.current.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The routing question a bounded read asks, answered from the last round.
///
/// This is the join the whole directory was built for: the dialling thread
/// writes a round every awareness interval, and until now nothing read one. The
/// implementation adds no rule of its own — [`Directory::read_within`] already
/// decides *here first, then freshest qualifying peer*, and repeating any part
/// of that here would be a second place for the routing rule to live.
///
/// # Two things this method does that the directory cannot
///
/// It reads the **clock**. Every method on [`Directory`] takes `now` so its
/// ageing rule can be tested without waiting, which means somebody has to be the
/// edge where real time enters, and a production caller is the only honest
/// candidate for it.
///
/// It passes `mine: None`. The session asks only once its own copy has already
/// failed the bound, so *here* is decided; handing the directory this node's
/// currency as well would invite it to answer `Here` to a question that was only
/// asked because the answer was no.
impl tessari_session::Elsewhere for Published {
    fn within(&self, bound: Duration) -> Option<tessari_session::Peer> {
        let directory = self.current();
        match directory.read_within(None, bound, Instant::now()) {
            // The epoch comes back out of the same reading that chose the
            // endpoint, rather than from a second decision: `Heard.said` is
            // exactly what that peer last claimed about itself, and its `epoch`
            // field is documented as the leadership it believes current. Looking
            // it up here and not widening `Destination` keeps the three-valued
            // routing answer about *where*, which is all it has ever decided.
            //
            // A row chosen by `read_within` is a row that is in the map, so the
            // lookup cannot miss — but it is written as a lookup and not as an
            // unwrap because a panic in a routing decision would take a serving
            // node down over a redirect it could simply decline to issue.
            Destination::There { endpoint, node } => {
                let epoch = directory.at(&endpoint)?.said.epoch;
                Some(tessari_session::Peer {
                    endpoint,
                    node,
                    epoch,
                })
            }
            // `Here` cannot arise with no currency of our own offered, and
            // `Nowhere` is the answer the caller already holds. Both mean *not
            // that I know of*, which is what `None` says.
            Destination::Here | Destination::Nowhere => None,
        }
    }

    /// The other routing question, answered from the same last round.
    ///
    /// No clock and no bound, because leadership does not age into being
    /// slightly wrong the way a currency reading does — see
    /// [`Directory::writable`]. The epoch is looked up out of the same reading
    /// that chose the endpoint, exactly as it is above, and for the same reason:
    /// it is what that peer claimed about itself, and it is what makes the
    /// redirect checkable when the client arrives.
    fn writable(&self) -> Option<tessari_session::Peer> {
        let directory = self.current();
        let (endpoint, node) = directory.writable()?;
        let epoch = directory.at(&endpoint)?.said.epoch;
        Some(tessari_session::Peer {
            endpoint,
            node,
            epoch,
        })
    }

    /// The named node, if the last round heard it at that address carrying
    /// `SERVING` — a drained node still greets, and sending a client to it is
    /// what draining exists to prevent, exactly as [`Directory::read_within`]
    /// decides. The node id is compared because the address alone is not the
    /// node: a row naming one node at an address another answers from is not a
    /// place this node can vouch for.
    fn serving(&self, endpoint: &str, node: &[u8; NODE_ID_LEN]) -> Option<tessari_session::Peer> {
        let directory = self.current();
        let heard = directory.at(endpoint)?;
        (heard.said.node == *node && heard.said.roles.has(Roles::SERVING)).then(|| {
            tessari_session::Peer {
                endpoint: endpoint.to_owned(),
                node: *node,
                epoch: heard.said.epoch,
            }
        })
    }

    /// Who the last round heard leading `range`'s line.
    fn leading(&self, range: tessari_types::Reach) -> Option<tessari_session::Peer> {
        let (endpoint, node, epoch) = self.current().leading(range)?;
        Some(tessari_session::Peer {
            endpoint,
            node,
            epoch,
        })
    }

    /// What the last round heard the node at `endpoint` greet as.
    fn build_at(&self, endpoint: &str) -> Option<NodeVersion> {
        self.current().at(endpoint).map(|heard| heard.said.build)
    }
}

pub use collecting::{Collecting, bootstrap_from, upstream};
/// Does the catalog name a peer that is not this node?
///
/// Re-exported from `tessari_storage` and not defined here: the write gate asks
/// the same question of the same rows, and membership defined twice is
/// membership that agrees until it does not. The reasoning — why this is not
/// *is the catalog empty*, and why the gate asks it rather than asking what
/// role the node was given — is on the definition.
pub use tessari_storage::names_a_peer;

#[cfg(test)]
mod tests;
