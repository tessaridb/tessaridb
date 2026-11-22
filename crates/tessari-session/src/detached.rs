//! A session with its store taken away, so it can cross a thread.
//!
//! A [`Session`] borrows its store, and an async connection does not hold that
//! borrow between statements. Everything else
//! a session holds is owned, so the owned half is carried between statements as
//! a [`Detached`] and given its store back for the length of one call.
//!
//! Nothing is re-derived on the way back: the identity, the selection and the
//! consumer are the ones the connection established, exactly as if the session
//! had never been taken apart. That is the difference from a
//! [`Ticket`](crate::Ticket), which re-reads its user because it outlives any
//! one connection; a detached session never outlives its own.

use std::sync::Arc;

use tessari_storage::Store;

use crate::elsewhere::Elsewhere;
use crate::gather::Gather;
use crate::identity::Identity;
use crate::session::{Consumer, Session};

/// A session's own state, without the store it runs against.
#[derive(Debug)]
pub struct Detached {
    namespace: Option<String>,
    database: Option<String>,
    identity: Identity,
    consumer: Option<Consumer>,
    elsewhere: Option<Arc<dyn Elsewhere>>,
    gather: Option<Arc<dyn Gather>>,
    participants: Option<Arc<dyn crate::Participants>>,
    backups: Option<Arc<std::path::Path>>,
    at_rest: Option<Arc<tessari_vault::AtRestKey>>,
    budget: Option<Arc<dyn crate::throttle::Budget>>,
    certificates: Option<Arc<dyn crate::presented::Certificates>>,
    /// The transaction a `run_holding` parked, riding to the next attach —
    /// the block a connection opened stays open across the bridge call.
    held: Option<(tessari_storage::Transaction<'static>, tessari_ql::Span)>,
    /// The savepoints the open block holds, oldest first — they belong to
    /// the block, not to the request that named them, so they ride the
    /// bridge with it (ADR-0134 D11).
    savepoints: Vec<(String, tessari_storage::Savepoint)>,
}

impl Session<'_> {
    /// Take this session's state away from its store.
    #[must_use]
    pub fn detach(self) -> Detached {
        Detached {
            namespace: self.namespace,
            database: self.database,
            identity: self.identity,
            consumer: self.consumer,
            elsewhere: self.elsewhere,
            gather: self.gather,
            participants: self.participants,
            backups: self.backups,
            at_rest: self.at_rest,
            budget: self.budget,
            certificates: self.certificates,
            held: self.held,
            savepoints: self.savepoints,
        }
    }
}

impl Detached {
    /// Give this state its store back, as the session it was.
    #[must_use]
    pub fn attach(self, store: &Store) -> Session<'_> {
        Session {
            store,
            namespace: self.namespace,
            database: self.database,
            identity: self.identity,
            consumer: self.consumer,
            elsewhere: self.elsewhere,
            gather: self.gather,
            participants: self.participants,
            backups: self.backups,
            at_rest: self.at_rest,
            budget: self.budget,
            certificates: self.certificates,
            held: self.held,
            savepoints: self.savepoints,
            // A sink belongs to the request that streams, never to state
            // carried to a later one.
            sink: crate::backup_to::Sink::none(),
            landed: false,
            // For the sink's reason: it belongs to a transaction this request
            // opened, and none travels with a detached session.
            acknowledge_open: None,
            across_open: false,
            event_depth: 0,
            now: std::time::SystemTime::now(),
            // The parse cache does not travel the bridge: `Detached` is a
            // block's state, not a session's working set, and a fresh cache
            // only re-parses a guard text on its next use — an immutable
            // mapping, so nothing is lost but the parse.
            guard_cache: dashmap::DashMap::new(),
        }
    }

    /// Whether this state holds a transaction open across the bridge — what
    /// an answering surface reports as the connection's block state.
    #[must_use]
    pub fn transaction_open(&self) -> bool {
        self.held.is_some()
    }
}
