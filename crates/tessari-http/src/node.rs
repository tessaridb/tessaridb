//! The listener: binding an address and serving one store on it.

use super::*;

/// One wire session over a byte stream: given the stream, it runs until the
/// session ends (ADR-0089).
pub type WireSession = Box<
    dyn FnOnce(tokio::io::DuplexStream) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send>>
        + Send,
>;

/// The wire node's door, as `GET /wire` needs it: a place for one session, or
/// `None` when the wire node is serving as many connections as it will.
///
/// Handed in by the process rather than built here, because this crate does not
/// depend on the wire protocol's crate and a WebSocket is only ever the carrier.
pub type WireDoor = Arc<dyn Fn() -> Option<WireSession> + Send + Sync>;

/// An HTTP listener bound to one address, serving one store.
pub struct Node {
    db: Arc<Db>,
    listener: TcpListener,
    stopping: Arc<Stopping>,
    census: Option<Arc<Census>>,
    committed: Arc<Commits>,
    door: Arc<Admitting>,
    tokens: Arc<tokens::Tokens>,
    /// How many requests' routes may run on the blocking pool at once.
    bridge: Arc<Bridge>,
    /// How many watch rounds may run at once, apart from requests — the
    /// wire node's reason, and measured there: rounds bunch on one commit.
    rounds: Arc<Bridge>,
    /// The wire node's door, when this process serves the wire protocol too.
    wire: Option<WireDoor>,
    /// TLS for every connection, when the node was given a certificate
    /// (ADR-0108 D4). `None` answers in the clear.
    secured: Option<tokio_rustls::TlsAcceptor>,
}

impl Node {
    /// Bind `address`.
    ///
    /// Bound here, synchronously, so a process learns whether it has its
    /// address before it builds anything else; served by [`Node::serve`] on the
    /// runtime.
    ///
    /// # Errors
    ///
    /// Returns the operating system's failure when the address cannot be bound.
    pub fn bind(
        db: Arc<Db>,
        address: &str,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let listener = tessari_serve::listen(address)?;
        // The runtime's listener requires it, and nothing here reads it blocking.
        listener.set_nonblocking(true)?;
        let committed = Arc::clone(db.commits());
        Ok(Self {
            db,
            listener,
            stopping: Stopping::new(),
            census: None,
            committed,
            door: Admitting::to(MAX_CONNECTIONS),
            tokens: Arc::new(tokens::Tokens::default()),
            bridge: Arc::new(Bridge::new(MAX_STORE_CALLS)),
            rounds: Arc::new(Bridge::new(
                std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get),
            )),
            wire: None,
            secured: None,
        })
    }

    /// Answer over TLS, and only over TLS (ADR-0108 D4).
    ///
    /// `GET /wire` and `GET /watch` ride the same connection, so they are
    /// encrypted with everything else. `settings` should offer `http/1.1` by
    /// ALPN, which is the one protocol this surface speaks.
    pub fn securing(&mut self, settings: Arc<rustls::ServerConfig>) {
        self.secured = Some(tokio_rustls::TlsAcceptor::from(settings));
    }

    /// Carry the wire protocol over `GET /wire`, through `door` (ADR-0089).
    ///
    /// Without it the route answers `404`: a node started without a wire
    /// address has no wire sessions to hand out, over any carrier.
    pub fn carrying(&mut self, door: WireDoor) {
        self.wire = Some(door);
    }

    /// Report every surface of this process on `/metrics`, not only this one.
    ///
    /// Set after binding rather than taken by [`Node::bind`], because the census
    /// names this node among the others and so is only complete once it exists.
    pub fn watching(&mut self, census: Arc<Census>) {
        self.census = Some(census);
    }

    /// The address actually bound, which a caller needs when it asked for port zero.
    #[must_use]
    pub fn address(&self) -> String {
        self.listener
            .local_addr()
            .map_or_else(|_| "unknown".to_owned(), |found| found.to_string())
    }

    /// What this node counts as in flight, and how it is told to stop.
    #[must_use]
    pub fn stopping(&self) -> Arc<Stopping> {
        Arc::clone(&self.stopping)
    }

    /// Serve until `stop` is cancelled.
    ///
    /// Must be awaited inside a Tokio runtime; the node creates none of its own.
    /// Cancelling stops accepting and lets requests in flight finish; a watch
    /// ends at the stage that ends feeds, when [`Stopping`] says so.
    ///
    /// # Errors
    ///
    /// Returns the listener's failure.
    pub async fn serve(&self, stop: CancellationToken) -> std::io::Result<()> {
        let listener = tokio::net::TcpListener::from_std(self.listener.try_clone()?)?;
        let shared = Arc::new(Shared {
            db: Arc::clone(&self.db),
            tokens: Arc::clone(&self.tokens),
            stopping: Arc::clone(&self.stopping),
            census: self.census.clone(),
            committed: Arc::clone(&self.committed),
            door: Arc::clone(&self.door),
            bridge: Arc::clone(&self.bridge),
            rounds: Arc::clone(&self.rounds),
            wire: self.wire.clone(),
        });
        let app = axum::Router::new().fallback(handle).with_state(shared);
        match &self.secured {
            None => served(listening::Listening::new(listener), app, stop).await,
            Some(acceptor) => {
                let securing = securing::Securing::new(listener, acceptor.clone());
                served(listening::Listening::new(securing), app, stop).await
            }
        }
    }
}

/// Serve `app` on `listening` until `stop` is cancelled or the listener fails.
pub(super) async fn served<A: listening::Accept>(
    mut listening: listening::Listening<A>,
    app: axum::Router,
    stop: CancellationToken,
) -> std::io::Result<()> {
    let (ended, failure) = (listening.ended(), listening.failure());
    axum::serve(
        // Tapped for the address alone: axum hands a peer's address to
        // `ConnectInfo` only through its own listener or a tapped one.
        listening.tap_io(|_| {}),
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        tokio::select! {
            () = stop.cancelled() => {}
            () = ended.cancelled() => {}
        }
    })
    .await?;
    failure
        .and_then(|mut failed| failed.try_recv().ok())
        .map_or(Ok(()), Err)
}
