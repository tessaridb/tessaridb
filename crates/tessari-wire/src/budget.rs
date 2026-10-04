//! One sign-in budget for a cluster (ADR-0108 D5).
//!
//! Every node holding the user catalog is a sign-in point, and each counted
//! misses alone, so N nodes gave a guesser N times the allowance per account
//! (R-11). The store line's leader's table is the cluster's: a clustered node
//! asks it before hashing a password and tells it how the try went. The name
//! crosses the mutually authenticated link; a password never does.
//!
//! The leader is the one this node last heard lead the store line — no network
//! call decides *who*, one call asks. A node that leads asks nobody: its own
//! table is the cluster's. A node that cannot reach the leader within
//! [`SIGN_IN_ASK_MILLIS`] decides on its own count, which is what a partition
//! costs while it lasts.

use std::sync::Arc;
use std::time::Duration;

use tessari_constants::SIGN_IN_ASK_MILLIS;
use tessari_encoding::NODE_ID_LEN;

use crate::driver::Published;
use crate::error::{Error, Result};
use crate::frame;
use crate::gatherer::Greeting;
use crate::keys::PeerKeys;
use crate::link::{Answered, Ask, call_within};

/// What a sign-in tells or asks the leader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Try {
    /// May this name try now?
    Permit,
    /// A try missed.
    Failed,
    /// A try succeeded.
    Succeeded,
}

/// One question or report about a name's tries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    /// Which.
    pub kind: Try,
    /// The name the try was made as.
    pub name: String,
}

impl Attempt {
    /// The body of a [`crate::peer::PeerFrame::Attempt`] frame.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut body = vec![match self.kind {
            Try::Permit => 0,
            Try::Failed => 1,
            Try::Succeeded => 2,
        }];
        frame::put_text(&mut body, &self.name);
        body
    }

    /// Read one back.
    ///
    /// # Errors
    ///
    /// [`Error::Malformed`] for a kind this build does not know or a short body.
    pub fn decode(body: &[u8]) -> Result<Self> {
        let kind = match body.first() {
            Some(0) => Try::Permit,
            Some(1) => Try::Failed,
            Some(2) => Try::Succeeded,
            _ => return Err(Error::Malformed),
        };
        let (name, _) = frame::take_text(body, 1)?;
        Ok(Self { kind, name })
    }

    /// The leader's answer, from `store`'s own table: whether the name may
    /// try now, and `true` for a report.
    #[must_use]
    pub fn answered(&self, store: &tessari_storage::Store) -> bool {
        match self.kind {
            Try::Permit => tessari_session::permit_shared(store, &self.name),
            Try::Failed => {
                tessari_session::failed_shared(store, &self.name);
                true
            }
            Try::Succeeded => {
                tessari_session::succeeded_shared(store, &self.name);
                true
            }
        }
    }
}

/// Asks the store line's leader about sign-in tries.
pub struct SharedBudget {
    me: [u8; NODE_ID_LEN],
    keys: PeerKeys,
    routing: Arc<Published>,
    greeting: Greeting,
}

impl core::fmt::Debug for SharedBudget {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SharedBudget")
            .field("me", &self.me)
            .finish_non_exhaustive()
    }
}

impl SharedBudget {
    /// A budget for the node `me`, dialling with its peer `credential`.
    #[must_use]
    pub fn new(
        me: [u8; NODE_ID_LEN],
        keys: PeerKeys,
        routing: Arc<Published>,
        greeting: Greeting,
    ) -> Self {
        Self {
            me,
            keys,
            routing,
            greeting,
        }
    }

    /// Ask the leader, or `None` when this node leads or the leader cannot be
    /// asked in time.
    fn ask(&self, kind: Try, name: &str) -> Option<bool> {
        let (endpoint, node) = self.routing.current().writable()?;
        if node == self.me {
            return None;
        }
        let said = (self.greeting)().ok()?;
        let asked = Attempt {
            kind,
            name: name.to_owned(),
        };
        match call_within(
            endpoint.as_str(),
            (&self.keys, self.keys.duplicate()),
            node,
            &said,
            Ask::Attempt(&asked),
            Duration::from_millis(SIGN_IN_ASK_MILLIS),
        ) {
            Ok((_, Answered::Attempted(answer))) => Some(answer),
            Ok(_) => None,
            Err(why) => {
                tracing::warn!(endpoint = %endpoint, error = %why, "the cluster's sign-in budget could not be asked");
                None
            }
        }
    }
}

impl tessari_session::Budget for SharedBudget {
    fn permit(&self, name: &str) -> Option<bool> {
        self.ask(Try::Permit, name)
    }

    fn failed(&self, name: &str) {
        let _ = self.ask(Try::Failed, name);
    }

    fn succeeded(&self, name: &str) {
        let _ = self.ask(Try::Succeeded, name);
    }
}

#[cfg(test)]
mod tests {
    use super::{Attempt, Try};
    use tessari_constants::FREE_SIGN_IN_FAILURES;

    #[test]
    fn an_attempt_crosses_the_wire_unchanged_and_a_short_one_is_refused() {
        for kind in [Try::Permit, Try::Failed, Try::Succeeded] {
            let sent = Attempt {
                kind,
                name: "ada".to_owned(),
            };
            assert_eq!(Attempt::decode(&sent.encode()).expect("decoded"), sent);
        }
        assert!(Attempt::decode(&[]).is_err());
        assert!(
            Attempt::decode(&[9, 0, 0, 0, 0]).is_err(),
            "an unknown kind was read"
        );
    }

    #[test]
    fn misses_a_peer_reports_make_the_next_try_wait_here() {
        let db = tessaridb::Db::in_memory().expect("an in-memory store");
        let store = db.store();
        let name = "ada";
        let asked = |kind| {
            Attempt {
                kind,
                name: name.to_owned(),
            }
            .answered(store)
        };
        assert!(asked(Try::Permit), "a fresh name was made to wait");
        for _ in 0..FREE_SIGN_IN_FAILURES {
            assert!(asked(Try::Failed));
        }
        assert!(!asked(Try::Permit), "reported misses did not count here");
        assert!(asked(Try::Succeeded));
        assert!(
            asked(Try::Permit),
            "a reported success did not give the allowance back"
        );
    }
}
