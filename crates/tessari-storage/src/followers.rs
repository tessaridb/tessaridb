//! What a leader knows about the followers that collect from it.
//!
//! # Why the leader can know this at all
//!
//! A follower collects by asking, and the only authorized way to ask is
//! [`crate::Store::log_records_within`] behind the session door. So the leader
//! already handles every byte a follower receives; what it has never done is
//! *keep* what it handled. This module keeps it, and that is the whole
//! difference between a leader that can be asked how its followers are doing
//! and one that can only be asked how it is doing itself.
//!
//! It requires no connection back to the follower, which is the point: a
//! leader that dials out to measure lag has made the measurement depend on the
//! thing being measured being reachable.
//!
//! # Why none of this is persisted
//!
//! The same argument [`crate::running`] makes in its own header. A follower's
//! progress is a fact about a live relationship, and a relationship does not
//! survive the process that was holding it up. A leader killed mid-stream would
//! leave behind a row saying a follower reached sequence 900 four seconds ago,
//! and the next process to read it would publish a claim nobody can check
//! against anything.
//!
//! So this is empty at open and fills as followers collect. A restarted leader
//! reports no followers until one asks it for something, which is the answer
//! that is true.
//!
//! # A later pull replaces an earlier one; it does not raise a high-water mark
//!
//! A follower that asks again from an *earlier* position is telling the leader
//! it holds less than it did — which is what a follower recovering from a
//! divergence looks like. Keeping the maximum would hide exactly that, and it
//! is the case an operator most needs to see. The last thing a follower said
//! about itself is the current answer.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tessari_encoding::NODE_ID_LEN;
use tessari_types::Sequence;

/// What a leader has given one follower, and when.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Served {
    /// The highest sequence that follower has been given.
    pub sequence: Sequence,
    /// When it last collected — including a collection that carried nothing.
    ///
    /// An empty answer is still contact: a follower that is level polls and
    /// receives nothing, and treating that as silence would report a healthy
    /// follower as absent.
    pub at: Instant,
}

/// How far behind one follower is, in both units.
///
/// Two numbers because each has a blind spot the other covers. A follower that
/// has stopped collecting while the leader is idle is not behind by a single
/// sequence — `behind` reads zero and it looks well, because in sequences it
/// *is* well; only `quiet_for` grows. A follower collecting steadily but unable
/// to keep up has barely any `quiet_for`; only `behind` grows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FollowerLag {
    /// Which follower, by the id it named itself with.
    pub node: [u8; NODE_ID_LEN],
    /// The highest sequence it has been given.
    pub sequence: Sequence,
    /// How many sequences short of this leader's committed tail that is.
    pub behind: u64,
    /// How long since it last collected.
    ///
    /// Not the delay between a commit here and its application there — that
    /// needs either a time in the log or a report from the follower, and this
    /// build has neither. When a follower is level, its copy was current as of
    /// this long ago; when it is behind, this is how long it has not been
    /// trying.
    pub quiet_for: Duration,
}

/// The followers this process has served, by the id each named itself with.
///
/// Shared by every handle to one store, for the reason the snapshot registry
/// is: two handles are not two leaders, and a follower recorded against one of
/// them is a follower the other cannot report.
#[derive(Debug, Default)]
pub struct Followers {
    seen: Mutex<BTreeMap<[u8; NODE_ID_LEN], Served>>,
}

impl Followers {
    /// Record that `node` has now been given everything up to `sequence`.
    ///
    /// Takes no result and returns none. A registry that can refuse is a
    /// registry that can fail a follower's replication in order to protect a
    /// diagnostic, which is the wrong way round: a lost lag row is a gap in a
    /// report, a refused pull is a follower that stops advancing.
    pub fn served(&self, node: [u8; NODE_ID_LEN], sequence: Sequence) {
        if let Ok(mut seen) = self.seen.lock() {
            seen.insert(
                node,
                Served {
                    sequence,
                    at: Instant::now(),
                },
            );
        }
    }

    /// Every follower this process has served, in id order.
    ///
    /// A follower that has never collected is absent rather than zero. The
    /// difference is the same one `INFO FOR NODE` draws between an unbound node
    /// and a drained one: *we have never heard from it* and *it is level* are
    /// different answers, and a zero row would spell them the same way.
    #[must_use]
    pub fn seen(&self) -> Vec<([u8; NODE_ID_LEN], Served)> {
        self.seen.lock().map_or_else(
            |_| Vec::new(),
            |seen| seen.iter().map(|(node, held)| (*node, *held)).collect(),
        )
    }
}
