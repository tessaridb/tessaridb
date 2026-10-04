//! The node: listening, conversing, and pushing.
//!
//! Everything about being the server end of this protocol. The client end is
//! `client.rs`, and the two share only the frames.
//!
//! A connection is a task on the process's runtime, not a thread; the store it
//! talks to stays synchronous behind the node's bridge (ADR-0085). The
//! conversation itself is `conversation.rs`.

use std::net::{TcpListener, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tessari_constants::{GREETING_SECONDS, MAX_CONNECTIONS, MAX_STORE_CALLS};
use tessari_serve::{ACCEPT_PAUSE, Admitting, Bridge, Stopping, passes};
use tessaridb::Db;
use tessaridb::feed::Commits;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::carrier::Carrier;
use crate::conversation;
use crate::error::Result;
use crate::{frame, frame_async};

/// Names one connection across every line it produces.
///
/// Monotonic within a process and never reused, so two lines carrying the same
/// number are the same conversation. It says nothing across restarts, which is
/// all an operator following one client through a refusal needs it to say.
static CONNECTIONS: AtomicU64 = AtomicU64::new(0);

/// The next connection's name.
pub(crate) fn next_connection() -> u64 {
    CONNECTIONS.fetch_add(1, Ordering::Relaxed)
}

/// Refuse a connection at the door, in the protocol's own words.
///
/// Best effort by construction: the client may already be gone, and a node that
/// is refusing because it is full has no capacity to spend caring. What matters
/// is that the socket closes here rather than being held.
async fn turn_away(mut stream: tokio::net::TcpStream) {
    drop(
        frame_async::write(
            &mut stream,
            frame::Kind::Refusal,
            b"this node is serving as many connections as it will",
        )
        .await,
    );
}

/// Where a connection came from, for the line that says it arrived.
fn from_where(stream: &tokio::net::TcpStream) -> String {
    stream.peer_addr().map_or_else(
        |_| "an address the socket would not give".to_owned(),
        |at| at.to_string(),
    )
}

/// A node listening for connections.
pub struct Node {
    listener: TcpListener,
    db: Arc<Db>,
    committed: Arc<Commits>,
    stopping: Arc<Stopping>,
    door: Arc<Admitting>,
    /// How many store calls this node's connections may have running at once.
    ///
    /// Sized at the door's ceiling for now, which is what one thread per
    /// connection bounded it at before; the after-measurement sets it apart.
    bridge: Arc<Bridge>,
    /// How many feed rounds may run at once, apart from statements.
    ///
    /// A feed polls the log every round whether anything happened or not, and
    /// rounds bunch: one commit wakes every feed at the same instant. Through the
    /// statements' bridge four hundred idle feeds kept four hundred blocking
    /// threads alive (measured). Bounded at the core count, a burst of rounds
    /// queues behind the cores instead — a round that finds this full waits for
    /// the next signal rather than being refused, since its subscriber was
    /// already admitted.
    rounds: Arc<Bridge>,
    /// How many busy connections may be served on a store thread at once
    /// (`hot.rs`). Beyond it a busy connection is answered from its task, as an
    /// idle one always is, so this bounds threads and never refuses anybody.
    hot: Arc<Semaphore>,
    /// What this node knows about the copies it does not hold, if anything.
    ///
    /// Held as the trait and not as the directory behind it: a node serving
    /// clients does not care *how* the answer is arrived at, only that a bounded
    /// read it cannot satisfy has somewhere to be sent. `None` on a node
    /// standing alone, which is every deployment that was never told about
    /// peers, and such a node refuses exactly as it did before.
    elsewhere: Option<Arc<dyn tessari_session::Elsewhere>>,
    /// TLS for every connection, when the node was given a certificate
    /// (ADR-0108 D4). `None` serves the protocol in the clear.
    secured: Option<tokio_rustls::TlsAcceptor>,
}

impl Node {
    /// Listen on `address`.
    ///
    /// Bound here, synchronously, so a process learns whether it has its
    /// address before it builds anything else; served by [`Node::serve`] on the
    /// runtime.
    ///
    /// # Errors
    ///
    /// Returns the operating system's failure when the address cannot be bound.
    pub fn bind(db: Arc<Db>, address: impl ToSocketAddrs) -> Result<Self> {
        let listener = tessari_serve::listen(address)?;
        // The runtime's listener requires it, and nothing here reads it blocking.
        listener.set_nonblocking(true)?;
        let committed = Arc::clone(db.commits());
        Ok(Self {
            listener,
            db,
            committed,
            stopping: Stopping::new(),
            door: Admitting::to(MAX_CONNECTIONS),
            bridge: Arc::new(Bridge::new(MAX_STORE_CALLS)),
            rounds: Arc::new(Bridge::new(
                std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get),
            )),
            hot: Arc::new(Semaphore::new(MAX_STORE_CALLS)),
            elsewhere: None,
            secured: None,
        })
    }

    /// Serve among the peers `elsewhere` knows about.
    ///
    /// Every session this node opens is given it, so a read carrying a staleness
    /// bound this node's own copy cannot satisfy is redirected to a copy that
    /// can rather than refused — C-07's *any node answers any request by serving
    /// it or by returning a redirect*.
    ///
    /// Taken as a builder rather than an argument to [`Node::bind`] because the
    /// thing that knows about peers is usually started *after* the door is open:
    /// a process binds its surfaces, then spawns the thread that greets. A
    /// required argument would force the two into an order the process does not
    /// have.
    #[must_use]
    pub fn among(mut self, elsewhere: Arc<dyn tessari_session::Elsewhere>) -> Self {
        self.elsewhere = Some(elsewhere);
        self
    }

    /// Speak TLS on every connection, and nothing else (ADR-0108 D4).
    ///
    /// There is no mixed port: a client that greets in the clear fails the
    /// handshake and never reaches the protocol, so a credential it sends is
    /// not read by this node — it has already crossed the network, which is
    /// what refusing it at the door cannot undo and does not pretend to.
    #[must_use]
    pub fn securing(mut self, settings: Arc<rustls::ServerConfig>) -> Self {
        self.secured = Some(tokio_rustls::TlsAcceptor::from(settings));
        self
    }

    /// Where it is listening, which a caller needs when it asked for port zero.
    ///
    /// # Errors
    ///
    /// Returns the operating system's failure when the socket cannot say.
    pub fn address(&self) -> Result<String> {
        Ok(self.listener.local_addr()?.to_string())
    }

    /// The door this node admits connections through.
    ///
    /// Exposed so a process can report how many places are taken and how many
    /// connections have been turned away — the number that says whether the
    /// ceiling is set right, and the one a refusal is worth nothing without.
    #[must_use]
    pub fn door(&self) -> Arc<Admitting> {
        Arc::clone(&self.door)
    }

    /// The bound on store calls in flight, for the process to report.
    #[must_use]
    pub fn bridge(&self) -> Arc<Bridge> {
        Arc::clone(&self.bridge)
    }

    /// What another surface needs to hold wire sessions on this node (ADR-0089).
    ///
    /// Shares this node's door, bridges, drain count and peers, so a session
    /// carried over a WebSocket is counted and bounded exactly as a TCP one is.
    #[must_use]
    pub fn carrier(&self) -> Carrier {
        Carrier {
            db: Arc::clone(&self.db),
            committed: Arc::clone(&self.committed),
            stopping: Arc::clone(&self.stopping),
            door: Arc::clone(&self.door),
            bridge: Arc::clone(&self.bridge),
            rounds: Arc::clone(&self.rounds),
            hot: Arc::clone(&self.hot),
            elsewhere: self.elsewhere.clone(),
        }
    }

    /// What this node counts as in flight, and how it is told to stop.
    ///
    /// Taken **before** [`Node::serve`], which consumes the node's borrow for
    /// as long as it runs: a caller that waited until afterwards would be asking
    /// a node that has already stopped.
    #[must_use]
    pub fn stopping(&self) -> Arc<Stopping> {
        Arc::clone(&self.stopping)
    }

    /// Accept connections until `stop` is cancelled.
    ///
    /// Must be awaited inside a Tokio runtime; the node creates none of its own.
    ///
    /// # What stopping leaves running
    ///
    /// Only the accepting stops. Conversations already admitted go on — a
    /// statement in flight finishes, a feed runs until the process's stage that
    /// ends feeds — because that is what the stages wait on: the drain counts
    /// them through [`Stopping`], and aborting them here would answer the drain
    /// by killing what it was waiting for. They are detached, not leaked: their
    /// lifetime ends with the runtime's own bounded shutdown.
    ///
    /// # Errors
    ///
    /// Returns the listener's failure when accepting fails in a way that does
    /// not pass on its own, which ends the node rather than spinning (Q-834).
    pub async fn serve(&self, stop: CancellationToken) -> Result<()> {
        let listener = tokio::net::TcpListener::from_std(self.listener.try_clone()?)?;
        let carrier = self.carrier();
        let mut conversations = JoinSet::new();
        loop {
            let accepted = tokio::select! {
                biased;
                () = stop.cancelled() => break,
                Some(joined) = conversations.join_next(), if !conversations.is_empty() => {
                    // A connection that panics takes its own task down and
                    // nothing else: a node that one client's malformed frame
                    // could stop would be a node anybody can stop.
                    if let Err(why) = joined
                        && why.is_panic()
                    {
                        log::warn!("a connection ended in a panic: {why}");
                    }
                    continue;
                }
                accepted = listener.accept() => accepted,
            };
            if self.stopping.asked() {
                break;
            }
            let stream = match accepted {
                Ok((stream, _)) => {
                    // A reply written in pieces must not wait for the peer's
                    // delayed acknowledgement (Q-762).
                    if let Err(why) = stream.set_nodelay(true) {
                        log::warn!("a connection could not turn off Nagle's algorithm: {why}");
                    }
                    stream
                }
                Err(why) if passes(&why) => {
                    log::warn!("accepting a connection failed ({why}); resting before the next");
                    tokio::time::sleep(ACCEPT_PAUSE).await;
                    continue;
                }
                Err(why) => return Err(why.into()),
            };
            let id = next_connection();
            // Before the task, because the connection is the resource being
            // bounded. A refusal costs one frame and a close — in the clear;
            // under TLS the socket is closed, since a frame before the
            // handshake is bytes the client cannot read.
            let Some(place) = self.door.admit() else {
                log::warn!(
                    "connection {id} refused from {}: {} already open",
                    from_where(&stream),
                    self.door.limit()
                );
                if self.secured.is_none() {
                    conversations.spawn(turn_away(stream));
                }
                continue;
            };
            log::info!("connection {id} accepted from {}", from_where(&stream));
            let (talk, session) = carrier.opening(id);
            let busy = self.stopping.busy();
            if let Some(acceptor) = self.secured.clone() {
                conversations.spawn(async move {
                    // Under the greeting's deadline, which is what it replaces
                    // at the door: a client that opens a socket and never
                    // finishes a handshake holds a place exactly as one that
                    // never greets would.
                    let shaken = tokio::time::timeout(
                        std::time::Duration::from_secs(GREETING_SECONDS),
                        acceptor.accept(stream),
                    )
                    .await;
                    let secured = match shaken {
                        Ok(Ok(secured)) => secured,
                        Ok(Err(why)) => {
                            log::info!("connection {id} failed its TLS handshake: {why}");
                            return;
                        }
                        Err(_) => {
                            log::info!("connection {id} did not finish its TLS handshake in time");
                            return;
                        }
                    };
                    match conversation::converse(talk, busy, place, session, secured).await {
                        Ok(()) => log::info!("connection {id} closed"),
                        Err(why) => log::info!("connection {id} ended: {why}"),
                    }
                });
                continue;
            }
            conversations.spawn(async move {
                match conversation::converse(talk, busy, place, session, stream).await {
                    Ok(()) => log::info!("connection {id} closed"),
                    // Not a warning. A client hanging up mid-frame is the
                    // ordinary end of a conversation.
                    Err(why) => log::info!("connection {id} ended: {why}"),
                }
            });
        }
        conversations.detach_all();
        Ok(())
    }
}

#[cfg(test)]
mod tests;
