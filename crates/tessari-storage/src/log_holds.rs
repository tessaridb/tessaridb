//! Positions a log may not be pruned past while a follower is being copied
//! (ADR-0094 D3).
//!
//! A copy hands a follower the leader's state at one version and the position
//! each log stood at. The follower then collects from the next position — and
//! if the retention window has pruned past it by then, it is refused and copies
//! again, which on a leader writing faster than a copy transfers is a loop that
//! never ends. So the leader holds those positions while the copy streams and
//! for a bounded grace afterwards: long enough for the follower's first collect,
//! never long enough for a follower that died to pin the disk.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tessari_encoding::LogId;
use tessari_types::Sequence;

use crate::store::Store;

/// One copy's held positions, and when the hold lapses (`None` while the copy
/// is still streaming).
#[derive(Debug)]
struct Hold {
    id: u64,
    positions: Vec<(LogId, Sequence)>,
    until: Option<Instant>,
}

/// Every hold this process has given.
#[derive(Debug, Default)]
pub(crate) struct LogHolds {
    held: Mutex<Vec<Hold>>,
    next: Mutex<u64>,
}

impl LogHolds {
    pub(crate) fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The lowest position held in `log`, after dropping the holds that lapsed.
    pub(crate) fn floor(&self, log: LogId) -> Option<Sequence> {
        let now = Instant::now();
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        held.retain(|hold| hold.until.is_none_or(|until| until > now));
        held.iter()
            .flat_map(|hold| hold.positions.iter())
            .filter(|(held_log, _)| *held_log == log)
            .map(|(_, at)| *at)
            .min()
    }

    fn release(&self, id: u64, grace: Option<Duration>) {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        match grace {
            Some(grace) => {
                if let Some(hold) = held.iter_mut().find(|hold| hold.id == id) {
                    // A grace too long to add lapses at once rather than
                    // pinning the log for ever.
                    hold.until = Some(
                        Instant::now()
                            .checked_add(grace)
                            .unwrap_or_else(Instant::now),
                    );
                }
            }
            None => held.retain(|hold| hold.id != id),
        }
    }
}

/// A hold on a copy's positions. Dropped, it is released at once — the copy
/// failed and nobody will collect from there; [`LogHold::keep_for`] instead
/// keeps it for the follower's first collect.
#[derive(Debug)]
pub struct LogHold {
    holds: Arc<LogHolds>,
    id: u64,
    kept: bool,
}

impl LogHold {
    /// The copy reached its end: keep the hold for `grace`, then let it lapse.
    pub fn keep_for(mut self, grace: Duration) {
        self.holds.release(self.id, Some(grace));
        self.kept = true;
    }
}

impl Drop for LogHold {
    fn drop(&mut self) {
        if !self.kept {
            self.holds.release(self.id, None);
        }
    }
}

impl Store {
    /// Hold `positions` against pruning until the returned guard says otherwise.
    #[must_use]
    pub fn hold_logs(&self, positions: &[(LogId, Sequence)]) -> LogHold {
        let holds = Arc::clone(self.log_holds());
        let id = {
            let mut next = holds.next.lock().unwrap_or_else(PoisonError::into_inner);
            *next = next.wrapping_add(1);
            *next
        };
        holds
            .held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Hold {
                id,
                positions: positions.to_vec(),
                until: None,
            });
        LogHold {
            holds,
            id,
            kept: false,
        }
    }
}
