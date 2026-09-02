//! The tokens this node has issued, and what they stand for.
//!
//! # Why this table exists
//!
//! A password check costs nineteen mebibytes and tens of milliseconds, and this
//! surface has no conversation to hang an identity on — so without something
//! here every request re-proves the same password, and the node's whole request
//! rate is bounded by [`MAX_SIGN_IN_VERIFICATIONS`], a number chosen to bound an
//! attacker rather than a customer.
//!
//! A caller signs in once at `POST /session`, receives a token, and presents it
//! afterwards. What the token stands for is a [`Ticket`] — the store's own proof
//! of who signed in — and taking one up re-reads the user, so a rotated
//! password, a corrected role or a removed account kills every token of that
//! user on the next request. That rule lives in `tessari-session`; this file
//! holds no authority of its own and must not, or there would be two answers to
//! who a caller is.
//!
//! # Why it is in memory, and per node
//!
//! Persisting bearer tokens would put credentials in the log, and therefore in
//! every replica and every backup — the precise thing this store refuses to do
//! with passwords. So a restart invalidates every token, which is a property to
//! state rather than a gap to close: the cost is one sign-in per client after a
//! restart, and the alternative is a credential store nobody asked for.
//!
//! Per node rather than per process, because two surfaces in one process are two
//! doors and a token issued at one should not open the other.
//!
//! # When an expired token actually leaves
//!
//! Three moments, and between them there is no timer thread:
//!
//! - **presented** — the lookup finds it dead and drops it there and then;
//! - **any sign-in** — issuing sweeps the whole table first;
//! - **never** — a token issued, never presented again, on a node where nobody
//!   signs in again either.
//!
//! The third case is left deliberately rather than overlooked. What it costs is
//! a map entry until the next sign-in, bounded by [`MAX_SESSION_TOKENS`] at
//! roughly two hundred bytes each — and a table full of expired rows does not
//! refuse anybody, because the sweep runs *before* the ceiling is checked. The
//! gauge does not count them either. A thread whose whole job is to delete
//! entries nothing is asking for would cost more attention than the megabyte it
//! saves.
//!
//! # On comparing tokens with `==`
//!
//! A hash-map lookup is not constant time, and that is a considered choice
//! rather than an oversight. A token is 256 bits from the operating system's
//! generator; learning one through a timing side channel requires distinguishing
//! attempts that each cost a network round trip, against a space no number of
//! them reduces. The threat this table is actually built against is a token
//! that has been *read* — off a plaintext connection, out of a log — and against
//! that, expiry and revocation are the defences, not comparison timing.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use tessari_constants::{MAX_SESSION_TOKENS, SESSION_TOKEN_SECONDS};
use tessaridb::Ticket;

/// One issued token.
struct Live {
    /// What the store said about the holder when they signed in.
    ticket: Ticket,
    /// When this stops being accepted.
    expires: Instant,
}

/// The tokens one node has issued.
#[derive(Default)]
pub(crate) struct Tokens {
    /// Keyed by the token text itself.
    ///
    /// A `RwLock` rather than a `Mutex` because presenting a token is the common
    /// path and issuing one is not: a read lock lets every request in flight
    /// look its holder up at once, and only the two paths that change the table
    /// — issuing and forgetting — exclude anybody.
    live: RwLock<HashMap<String, Live>>,
}

/// Why a token could not be issued.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refused {
    /// This node is already holding as many as it will.
    Full,
}

impl Tokens {
    /// Hold `ticket` and return the text its holder presents.
    ///
    /// # Errors
    ///
    /// [`Refused::Full`] when the table is at [`MAX_SESSION_TOKENS`] and nothing
    /// in it has expired. Refusing is deliberate: evicting somebody else's live
    /// token to make room would turn minting tokens into a way to sign other
    /// people out, which is a denial of service anybody with one account could
    /// aim at every other.
    pub(crate) fn issue(&self, ticket: Ticket) -> Result<String, Refused> {
        let bearer = ticket.bearer().to_owned();
        let Ok(mut live) = self.live.write() else {
            // A poisoned lock means a request thread panicked while holding it.
            // The table's contents are still structurally sound — nothing here
            // is a half-written invariant — but refusing is the honest answer
            // rather than reaching past a panic nobody has looked at.
            return Err(Refused::Full);
        };
        // Swept here rather than on a timer, because a node with no traffic has
        // nothing to sweep and a node with traffic sweeps often enough.
        let now = Instant::now();
        live.retain(|_, held| held.expires > now);
        if live.len() >= MAX_SESSION_TOKENS {
            log::warn!("a token was refused: this node is holding {MAX_SESSION_TOKENS} already");
            return Err(Refused::Full);
        }
        live.insert(
            bearer.clone(),
            Live {
                ticket,
                expires: now
                    .checked_add(Duration::from_secs(SESSION_TOKEN_SECONDS))
                    .unwrap_or(now),
            },
        );
        Ok(bearer)
    }

    /// What `bearer` stands for, if this node issued it and it is still good.
    ///
    /// The common path is a read, which is the whole point of the `RwLock`: every
    /// request in flight looks its holder up at once. Finding an **expired** row
    /// is the uncommon path, and that one takes the write lock to drop it rather
    /// than leaving it to be refused again on the next request.
    pub(crate) fn holder(&self, bearer: &str) -> Option<Ticket> {
        {
            let live = self.live.read().ok()?;
            let held = live.get(bearer)?;
            if held.expires > Instant::now() {
                return Some(held.ticket.clone());
            }
        }
        // Dropped here rather than left for the sweep, because this is the moment
        // it is known to be dead and the holder is right in front of us.
        self.forget(bearer);
        None
    }

    /// Forget one token, if it is here.
    ///
    /// Answers the same either way. Whether a token this node never issued
    /// *existed* is not something a caller presenting it should be able to
    /// learn.
    pub(crate) fn forget(&self, bearer: &str) {
        if let Ok(mut live) = self.live.write() {
            live.remove(bearer);
        }
    }

    /// How many **live** tokens are held. For the metrics scrape.
    ///
    /// Counts what would still be accepted rather than what is still in the map.
    /// The two differ between sweeps, and a gauge that reported rows would show
    /// a node filling up while every one of them was already dead — which is the
    /// opposite of what an operator watching this number needs to know.
    pub(crate) fn held(&self) -> usize {
        let now = Instant::now();
        self.live.read().map_or(0, |live| {
            live.values().filter(|held| held.expires > now).count()
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Arc;
    use std::time::Instant;

    use tessari_constants::MAX_SESSION_TOKENS;
    use tessari_kv::{KvBackend, MemoryBackend};
    use tessari_storage::Store;
    use tessaridb::{Session, Ticket};

    use super::{Live, Refused, Tokens};

    fn store() -> Store {
        let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
        Store::open(backend).unwrap()
    }

    /// A store with one user, signed in.
    ///
    /// Returned rather than re-derived per ticket because signing in is
    /// deliberately expensive: the bound test below wants thousands of tickets
    /// and one password check, which is the shape this whole module exists to
    /// make possible. Each `ticket()` call still mints its own token.
    fn signed(store: &Store) -> Session<'_> {
        let mut declaring = Session::new(store);
        declaring
            .run("DEFINE USER ada ROLE owner PASSWORD 'correct horse battery';")
            .unwrap();
        let mut session = Session::new(store);
        session.sign_in("ada", "correct horse battery").unwrap();
        session
    }

    /// One more ticket for the same user.
    fn ticket(session: &Session<'_>) -> Ticket {
        session.ticket().unwrap()
    }

    /// Put an already-dead token in the table, and answer with its text.
    ///
    /// Written straight into the map rather than through [`Tokens::issue`],
    /// because issuing sweeps first: three dead tokens cannot be *issued* one
    /// after another, since each sweep would collect the ones before it. The
    /// real life is twelve hours, and a test that waited for one would not be a
    /// test.
    fn plant_expired(tokens: &Tokens, session: &Session<'_>) -> String {
        let ticket = ticket(session);
        let bearer = ticket.bearer().to_owned();
        tokens.live.write().unwrap().insert(
            bearer.clone(),
            Live {
                ticket,
                // Not after any instant that follows, which is what expired is.
                expires: Instant::now(),
            },
        );
        bearer
    }

    #[test]
    fn what_was_issued_comes_back() {
        let store = store();
        let session = signed(&store);
        let tokens = Tokens::default();
        let bearer = tokens.issue(ticket(&session)).unwrap();
        assert!(tokens.holder(&bearer).is_some());
        assert_eq!(tokens.held(), 1);
    }

    #[test]
    fn a_token_this_node_never_issued_is_nobody() {
        let store = store();
        let session = signed(&store);
        let tokens = Tokens::default();
        tokens.issue(ticket(&session)).unwrap();
        // Including one of exactly the right shape: the table is the authority
        // on what it issued, and a well-formed guess is still a guess.
        assert!(tokens.holder(&"a".repeat(64)).is_none());
        assert!(tokens.holder("").is_none());
    }

    #[test]
    fn forgetting_one_leaves_the_rest() {
        let store = store();
        let session = signed(&store);
        let tokens = Tokens::default();
        let first = tokens.issue(ticket(&session)).unwrap();
        let second = tokens.issue(ticket(&session)).unwrap();

        tokens.forget(&first);
        assert!(tokens.holder(&first).is_none());
        assert!(
            tokens.holder(&second).is_some(),
            "signing one session out must not sign the others out"
        );
        // Forgetting what is not here is not an error, and says nothing.
        tokens.forget(&first);
    }

    #[test]
    fn an_expired_token_is_refused_and_does_not_stay() {
        let store = store();
        let session = signed(&store);
        let tokens = Tokens::default();
        let bearer = plant_expired(&tokens, &session);

        assert!(tokens.holder(&bearer).is_none());
        assert_eq!(
            tokens.live.read().unwrap().len(),
            0,
            "presenting a dead token is when it is known to be dead — it should \
             not survive to be refused a second time"
        );
    }

    #[test]
    fn the_gauge_counts_what_would_be_accepted() {
        let store = store();
        let session = signed(&store);
        let tokens = Tokens::default();
        // The live one first: issuing sweeps, so a dead row planted before it
        // would be collected and there would be nothing to miscount.
        tokens.issue(ticket(&session)).unwrap();
        plant_expired(&tokens, &session);

        // Two rows, one live. A gauge reporting rows would show a node filling
        // up while half of it was already dead.
        assert_eq!(tokens.live.read().unwrap().len(), 2);
        assert_eq!(tokens.held(), 1);
    }

    #[test]
    fn issuing_sweeps_what_has_expired() {
        let store = store();
        let session = signed(&store);
        let tokens = Tokens::default();
        for _ in 0..3 {
            plant_expired(&tokens, &session);
        }
        assert_eq!(tokens.live.read().unwrap().len(), 3);

        // The sweep runs before the ceiling is checked, so a table full of dead
        // tokens refuses nobody.
        tokens.issue(ticket(&session)).unwrap();
        assert_eq!(tokens.live.read().unwrap().len(), 1);
    }

    #[test]
    fn the_table_refuses_rather_than_evicting() {
        let store = store();
        let session = signed(&store);
        let tokens = Tokens::default();
        let first = tokens.issue(ticket(&session)).unwrap();
        for _ in 1..MAX_SESSION_TOKENS {
            tokens.issue(ticket(&session)).unwrap();
        }
        assert_eq!(tokens.held(), MAX_SESSION_TOKENS);

        // The one that does not fit is refused. The token issued first is still
        // live — if it were not, minting tokens would be a way to sign other
        // people out.
        assert_eq!(tokens.issue(ticket(&session)), Err(Refused::Full));
        assert!(tokens.holder(&first).is_some());
    }
}
