//! A session with its store taken away, so it can cross a thread.
//!
//! A [`Session`] borrows its store, and an async connection cannot hold a borrow
//! across the hop that runs each statement on the blocking pool. Everything else
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
    backups: Option<Arc<std::path::Path>>,
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
            backups: self.backups,
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
            backups: self.backups,
        }
    }
}
