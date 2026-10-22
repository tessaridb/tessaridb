//! The RESP surface: a listener on the node's runtime that answers the
//! protocol with the keys of the node's own spaces (ADR-0129).
//!
//! # What a command becomes
//!
//! Every command that reads or writes a key becomes statements on the
//! connection's own session — `GET`, `SET … IF … EXPIRE`, `INCR`, `KEYS FROM`
//! — with every key and value bound and every name checked as an identifier
//! first. A RESP client is therefore exactly as able as the store user it
//! signed in as, and a space a script cannot read cannot be read here.
//!
//! # Shape
//!
//! The same as the Kafka surface's: bound synchronously so a process learns
//! at start whether it has its address, served on the runtime it is handed,
//! its connections bounded by the same door ceiling and its store calls by the
//! same bridge, and its stages driven by the process through [`Stopping`].

mod channels;
mod commands;
mod connection;
mod glob;
mod kinds;
mod procedures;
mod replies;
mod rewrite;
mod running;
mod space;
mod statement;
mod values;

use std::net::ToSocketAddrs;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tessari_constants::{
    GREETING_SECONDS, MAX_CONNECTIONS, MAX_STORE_CALLS, RESP_PUBSUB_QUEUE, RESP_PUBSUB_QUEUE_BYTES,
    RESP_PUBSUB_RECHECK_SECONDS, RESP_PUBSUB_SUBSCRIPTIONS,
};
use tessari_serve::{ACCEPT_PAUSE, Admitting, Bridge, Stopping, passes};
use tessaridb::Db;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

pub use space::Space;

/// What every connection of one node shares.
pub(crate) struct Shared {
    pub(crate) db: Arc<Db>,
    pub(crate) bridge: Arc<Bridge>,
    pub(crate) stopping: Arc<Stopping>,
    /// The space a connection starts in, when the operator named one.
    pub(crate) space: Option<Space>,
    /// Who listens to what (ADR-0133 D1).
    pub(crate) channels: Arc<channels::Channels>,
    /// How much one subscriber may have waiting: messages, then bytes.
    pub(crate) inbox: (usize, usize),
    /// How long the store's word on listening or publishing is kept.
    pub(crate) recheck: std::time::Duration,
    /// The most a connection listens on.
    pub(crate) subscriptions: usize,
    clients: AtomicU64,
}

impl Shared {
    /// The id `CLIENT ID` answers for a new connection.
    pub(crate) fn next_client(&self) -> u64 {
        self.clients
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1)
    }
}

/// A bound RESP surface, not yet serving.
pub struct Node {
    listener: std::net::TcpListener,
    shared: Arc<Shared>,
    door: Arc<Admitting>,
    secured: Option<tokio_rustls::TlsAcceptor>,
}

impl Node {
    /// Bind `address` for the RESP protocol over `db`.
    ///
    /// # Errors
    ///
    /// The operating system's failure when the address cannot be bound.
    pub fn bind(db: Arc<Db>, address: impl ToSocketAddrs) -> std::io::Result<Self> {
        let listener = tessari_serve::listen(address)?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            shared: Arc::new(Shared {
                db,
                bridge: Arc::new(Bridge::new(MAX_STORE_CALLS)),
                stopping: Stopping::new(),
                space: None,
                channels: Arc::default(),
                inbox: (RESP_PUBSUB_QUEUE, RESP_PUBSUB_QUEUE_BYTES),
                recheck: std::time::Duration::from_secs(RESP_PUBSUB_RECHECK_SECONDS),
                subscriptions: RESP_PUBSUB_SUBSCRIPTIONS,
                clients: AtomicU64::new(0),
            }),
            door: Admitting::to(MAX_CONNECTIONS),
            secured: None,
        })
    }

    /// Speak TLS on every connection, and nothing else (ADR-0108 D4).
    #[must_use]
    pub fn securing(mut self, settings: Arc<rustls::ServerConfig>) -> Self {
        self.secured = Some(tokio_rustls::TlsAcceptor::from(settings));
        self
    }

    /// The space every connection starts in; without one, a connection names
    /// its own with `TESSARIDB.USE` before any key command.
    #[must_use]
    pub fn serving_space(mut self, space: Space) -> Self {
        if let Some(shared) = Arc::get_mut(&mut self.shared) {
            shared.space = Some(space);
        }
        self
    }

    /// Subscribers' inboxes and the cadence of their re-checks, smaller than
    /// the node's own so a test can reach them.
    #[cfg(test)]
    pub(crate) fn listening_within(
        mut self,
        inbox: (usize, usize),
        recheck: std::time::Duration,
        subscriptions: usize,
    ) -> Self {
        if let Some(shared) = Arc::get_mut(&mut self.shared) {
            shared.inbox = inbox;
            shared.recheck = recheck;
            shared.subscriptions = subscriptions;
        }
        self
    }

    /// Where it is listening.
    ///
    /// # Errors
    ///
    /// The operating system's failure when the socket cannot say.
    pub fn address(&self) -> std::io::Result<String> {
        Ok(self.listener.local_addr()?.to_string())
    }

    /// What this surface counts as in flight, and how it is told to stop;
    /// taken before [`Node::serve`].
    #[must_use]
    pub fn stopping(&self) -> Arc<Stopping> {
        Arc::clone(&self.shared.stopping)
    }

    /// Accept connections until `stop` is cancelled. Must be awaited inside a
    /// Tokio runtime; the surface creates none.
    ///
    /// Only the accepting stops: a command in flight finishes, which is what
    /// the drain waits on through [`Stopping`].
    ///
    /// # Errors
    ///
    /// The listener's failure when accepting fails in a way that does not pass.
    pub async fn serve(&self, stop: CancellationToken) -> std::io::Result<()> {
        let listener = tokio::net::TcpListener::from_std(self.listener.try_clone()?)?;
        let mut connections = JoinSet::new();
        loop {
            let accepted = tokio::select! {
                biased;
                () = stop.cancelled() => break,
                Some(joined) = connections.join_next(), if !connections.is_empty() => {
                    if let Err(why) = joined
                        && why.is_panic()
                    {
                        tracing::warn!(error = %why, "a resp connection ended in a panic");
                    }
                    continue;
                }
                accepted = listener.accept() => accepted,
            };
            if self.shared.stopping.asked() {
                break;
            }
            let (stream, peer) = match accepted {
                Ok(accepted) => accepted,
                Err(why) if passes(&why) => {
                    tracing::warn!(error = %why, "accepting a resp connection failed; resting before the next");
                    tokio::time::sleep(ACCEPT_PAUSE).await;
                    continue;
                }
                Err(why) => return Err(why),
            };
            if let Err(why) = stream.set_nodelay(true) {
                tracing::warn!(error = %why, "a resp connection could not turn off Nagle's algorithm");
            }
            let Some(place) = self.door.admit() else {
                tracing::warn!(from = %peer, open = self.door.limit(), "resp connection refused: as many open as this node takes");
                continue;
            };
            let shared = Arc::clone(&self.shared);
            let span = tracing::info_span!("resp", from = %peer);
            let acceptor = self.secured.clone();
            connections.spawn(
                async move {
                    let _place = place;
                    let ended = match acceptor {
                        Some(acceptor) => {
                            match tokio::time::timeout(
                                std::time::Duration::from_secs(GREETING_SECONDS),
                                acceptor.accept(stream),
                            )
                            .await
                            {
                                Ok(Ok(secured)) => {
                                    connection::converse(shared, Some(peer), secured).await
                                }
                                Ok(Err(why)) => {
                                    tracing::info!(error = %why, "resp connection failed its TLS handshake");
                                    return;
                                }
                                Err(_) => {
                                    tracing::info!("resp connection did not finish its TLS handshake in time");
                                    return;
                                }
                            }
                        }
                        None => connection::converse(shared, Some(peer), stream).await,
                    };
                    match ended {
                        Ok(()) => tracing::debug!("resp connection closed"),
                        Err(why) => tracing::info!(reason = %why, "resp connection ended"),
                    }
                }
                .instrument(span),
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
