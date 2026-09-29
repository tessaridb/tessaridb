//! Wire sessions over a byte stream another surface accepted (ADR-0089).
//!
//! The HTTP surface upgrades `GET /wire` to a WebSocket and has a stream of
//! bytes; this is what turns that stream into a wire session. It is the node's
//! own session in every respect the node controls — the same door, so the
//! connection ceiling counts both carriers together; the same bridge and feed
//! rounds; the same drain count; the same knowledge of peers — because a second
//! set of those would be a second node that happens to share a store.

use std::sync::Arc;

use tessari_serve::{Admitted, Admitting, Bridge, Busy, Stopping};
use tessari_session::Detached;
use tessaridb::Db;
use tessaridb::feed::Commits;
use tokio::io::DuplexStream;
use tokio::sync::Semaphore;

use crate::conversation::{self, Conversation};

/// What a node lends another surface so it can hold wire sessions.
#[derive(Clone)]
pub struct Carrier {
    pub(crate) db: Arc<Db>,
    pub(crate) committed: Arc<Commits>,
    pub(crate) stopping: Arc<Stopping>,
    pub(crate) door: Arc<Admitting>,
    pub(crate) bridge: Arc<Bridge>,
    pub(crate) rounds: Arc<Bridge>,
    pub(crate) hot: Arc<Semaphore>,
    pub(crate) elsewhere: Option<Arc<dyn tessari_session::Elsewhere>>,
}

impl Carrier {
    /// Take a place for one session, or `None` when the node is serving as many
    /// connections as it will.
    ///
    /// Taken before the other surface commits to anything — for a WebSocket,
    /// before the `101` — so a full node refuses in a status rather than by
    /// upgrading and hanging up.
    #[must_use]
    pub fn admit(&self) -> Option<Admission> {
        let place = self.door.admit()?;
        let id = crate::node::next_connection();
        let (talk, session) = self.opening(id);
        Some(Admission {
            id,
            talk,
            busy: self.stopping.busy(),
            place,
            session,
        })
    }

    /// What one conversation shares with the node, and the session it starts in.
    ///
    /// Given the cluster's answer once, at the session, rather than at each
    /// statement: what this node knows about its peers is a fact about the
    /// process and not about the request.
    pub(crate) fn opening(&self, id: u64) -> (Conversation, Detached) {
        let session = match &self.elsewhere {
            Some(known) => self.db.session().among(Arc::clone(known)),
            None => self.db.session(),
        }
        .detach();
        let talk = Conversation {
            id,
            db: Arc::clone(&self.db),
            committed: Arc::clone(&self.committed),
            stopping: Arc::clone(&self.stopping),
            bridge: Arc::clone(&self.bridge),
            rounds: Arc::clone(&self.rounds),
            hot: Arc::clone(&self.hot),
        };
        (talk, session)
    }
}

/// A place at the node's door, held until the session it was taken for ends.
pub struct Admission {
    id: u64,
    talk: Conversation,
    busy: Busy,
    place: Admitted,
    session: Detached,
}

impl Admission {
    /// Hold the session over `stream` until either end closes it.
    ///
    /// The place and the drain count are released when this returns, which is
    /// when the conversation is over.
    pub async fn converse(self, stream: DuplexStream) {
        let id = self.id;
        log::info!("connection {id} accepted over a websocket");
        match conversation::converse(self.talk, self.busy, self.place, self.session, stream).await {
            Ok(()) => log::info!("connection {id} closed"),
            // Not a warning, as over TCP: a client hanging up mid-frame is the
            // ordinary end of a conversation.
            Err(why) => log::info!("connection {id} ended: {why}"),
        }
    }
}
