//! The Kafka surface: a listener on the node's runtime that answers the
//! protocol with the node's own topics (ADR-0129).
//!
//! # What a request becomes
//!
//! Every request that reads or writes a topic becomes statements on the
//! connection's own session — `READ FROM`, `INSERT … RETURN POSITION`,
//! `INFO FOR TOPIC` — with every value bound and every name checked as an
//! identifier first. A Kafka client is therefore exactly as able as the store
//! user it signed in as, and a topic a script cannot read cannot be fetched.
//!
//! # Shape
//!
//! The same as the wire surface's: bound synchronously so a process learns at
//! start whether it has its address, served on the runtime it is handed, its
//! connections bounded by the same door ceiling and its store calls by the
//! same bridge, and its stages driven by the process through [`Stopping`].

mod administering;
mod committing;
mod connection;
mod coordinator;
mod errors;
mod fetching;
mod grouping;
mod listing;
mod metadata;
mod names;
mod offsets;
mod producers;
mod producing;
mod running;

use std::net::ToSocketAddrs;
use std::sync::Arc;

use tessari_constants::{GREETING_SECONDS, MAX_CONNECTIONS, MAX_STORE_CALLS};
use tessari_serve::{ACCEPT_PAUSE, Admitting, Bridge, Stopping, passes};
use tessaridb::Db;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

use coordinator::Coordinator;
pub use names::Database;
use producers::Producers;

/// What every connection of one node shares.
pub(crate) struct Shared {
    pub(crate) db: Arc<Db>,
    pub(crate) bridge: Arc<Bridge>,
    pub(crate) stopping: Arc<Stopping>,
    pub(crate) producers: Producers,
    /// Every group's membership, in node memory.
    pub(crate) coordinator: Arc<Coordinator>,
    /// The address Metadata gives clients, when the operator named one.
    pub(crate) advertised: Option<(String, i32)>,
    /// The database a bare topic name is read in.
    pub(crate) database: Option<Database>,
}

/// A bound Kafka surface, not yet serving.
pub struct Node {
    listener: std::net::TcpListener,
    shared: Arc<Shared>,
    door: Arc<Admitting>,
    secured: Option<tokio_rustls::TlsAcceptor>,
}

impl Node {
    /// Bind `address` for the Kafka protocol over `db`.
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
                producers: Producers::new(),
                coordinator: Arc::new(Coordinator::new()),
                advertised: None,
                database: None,
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

    /// The address Metadata hands clients instead of the one they reached.
    ///
    /// A Kafka client reconnects to the broker Metadata names, so behind a port
    /// mapping or a load balancer it is this, not the bound address, that works.
    #[must_use]
    pub fn advertising(mut self, host: String, port: u16) -> Self {
        if let Some(shared) = Arc::get_mut(&mut self.shared) {
            shared.advertised = Some((host, i32::from(port)));
        }
        self
    }

    /// Read a bare topic name in `database`; without one, a topic is named
    /// `<namespace>.<database>.<topic>`.
    #[must_use]
    pub fn reading_bare_names_in(mut self, database: Database) -> Self {
        if let Some(shared) = Arc::get_mut(&mut self.shared) {
            shared.database = Some(database);
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
    /// Only the accepting stops: a request in flight finishes, which is what
    /// the drain waits on through [`Stopping`].
    ///
    /// # Errors
    ///
    /// The listener's failure when accepting fails in a way that does not pass.
    pub async fn serve(&self, stop: CancellationToken) -> std::io::Result<()> {
        let listener = tokio::net::TcpListener::from_std(self.listener.try_clone()?)?;
        let mut connections = JoinSet::new();
        let sweeping = tokio::spawn(Arc::clone(&self.shared.coordinator).sweep_until_stopped());
        let served = self.accept(&listener, &mut connections, &stop).await;
        // The coordinator's waits end with the accepting, so a join parked for
        // its round does not hold the drain for a whole rebalance timeout.
        self.shared.coordinator.stop();
        if let Err(why) = sweeping.await {
            tracing::warn!(error = %why, "the kafka group sweeper ended in a panic");
        }
        served
    }

    async fn accept(
        &self,
        listener: &tokio::net::TcpListener,
        connections: &mut JoinSet<()>,
        stop: &CancellationToken,
    ) -> std::io::Result<()> {
        loop {
            let accepted = tokio::select! {
                biased;
                () = stop.cancelled() => break,
                Some(joined) = connections.join_next(), if !connections.is_empty() => {
                    if let Err(why) = joined
                        && why.is_panic()
                    {
                        tracing::warn!(error = %why, "a kafka connection ended in a panic");
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
                    tracing::warn!(error = %why, "accepting a kafka connection failed; resting before the next");
                    tokio::time::sleep(ACCEPT_PAUSE).await;
                    continue;
                }
                Err(why) => return Err(why),
            };
            if let Err(why) = stream.set_nodelay(true) {
                tracing::warn!(error = %why, "a kafka connection could not turn off Nagle's algorithm");
            }
            let Some(place) = self.door.admit() else {
                tracing::warn!(from = %peer, open = self.door.limit(), "kafka connection refused: as many open as this node takes");
                continue;
            };
            // The address the client reached, which Metadata answers with when
            // no address is advertised: the client is already talking to it.
            let reached = stream.local_addr().ok();
            let shared = Arc::clone(&self.shared);
            let span = tracing::info_span!("kafka", from = %peer);
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
                                    connection::converse(shared, reached, Some(peer), secured).await
                                }
                                Ok(Err(why)) => {
                                    tracing::info!(error = %why, "kafka connection failed its TLS handshake");
                                    return;
                                }
                                Err(_) => {
                                    tracing::info!("kafka connection did not finish its TLS handshake in time");
                                    return;
                                }
                            }
                        }
                        None => connection::converse(shared, reached, Some(peer), stream).await,
                    };
                    match ended {
                        Ok(()) => tracing::debug!("kafka connection closed"),
                        Err(why) => tracing::info!(reason = %why, "kafka connection ended"),
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
